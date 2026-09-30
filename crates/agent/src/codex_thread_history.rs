use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(Clone, Debug)]
pub struct CodexSession {
    pub id: uuid::Uuid,
    pub title: String,
    pub working_directory: PathBuf,
    pub created_at: DateTime<Utc>,
    pub archived: bool,
}

impl CodexSession {
    fn matches_project(&self, project_paths: &[PathBuf]) -> bool {
        let working_directory = self
            .working_directory
            .canonicalize()
            .unwrap_or_else(|_| self.working_directory.clone());
        project_paths
            .iter()
            .any(|path| working_directory.starts_with(path))
    }
}

pub fn load_history(
    codex_home: &Path,
    project_paths: &[PathBuf],
) -> anyhow::Result<Vec<CodexSession>> {
    let project_paths = project_paths
        .iter()
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()))
        .collect::<Vec<_>>();
    let mut sessions = HashMap::new();
    let mut database_error = None;
    let mut databases = Vec::new();
    match std::fs::read_dir(codex_home) {
        Ok(entries) => {
            for entry in entries {
                let path = entry?.path();
                if let Some(version) = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_prefix("state_"))
                    .and_then(|name| name.strip_suffix(".sqlite"))
                    .and_then(|version| version.parse::<u32>().ok())
                {
                    databases.push((version, path));
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    }
    databases.sort_by_key(|(version, _)| std::cmp::Reverse(*version));
    for (_, path) in databases {
        match read_database(&path) {
            Ok(records) => {
                database_error = None;
                for session in records {
                    sessions.insert(session.id, session);
                }
                break;
            }
            Err(error) => {
                log::warn!("Could not read Codex history database: {error:#}");
                database_error = Some(error);
            }
        }
    }
    for (folder, archived) in [("sessions", false), ("archived_sessions", true)] {
        read_session_files(&codex_home.join(folder), archived, &mut sessions)?;
    }
    if sessions.is_empty()
        && let Some(error) = database_error
    {
        return Err(error.context("Could not read Codex history"));
    }
    Ok(sessions
        .into_values()
        .filter(|session| session.matches_project(&project_paths))
        .collect())
}

fn read_database(path: &Path) -> anyhow::Result<Vec<CodexSession>> {
    let connection = db::sqlez::connection::Connection::open_read_only(path)?;
    let records = connection.select::<(String, String, String, i64, bool)>(
        "SELECT id, title, cwd, created_at, archived FROM threads",
    )?()?;
    records
        .into_iter()
        .map(|(id, title, working_directory, created_at, archived)| {
            Ok(CodexSession {
                id: uuid::Uuid::parse_str(&id)?,
                title: if title.trim().is_empty() {
                    "Codex".into()
                } else {
                    title
                },
                working_directory: working_directory.into(),
                created_at: DateTime::from_timestamp(created_at, 0)
                    .context("Invalid Codex session time")?,
                archived,
            })
        })
        .collect()
}

fn read_session_files(
    directory: &Path,
    archived: bool,
    sessions: &mut HashMap<uuid::Uuid, CodexSession>,
) -> anyhow::Result<()> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            read_session_files(&path, archived, sessions)?;
        } else if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            let saved_id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.get(stem.len().checked_sub(36)?..))
                .and_then(|id| uuid::Uuid::parse_str(id).ok());
            if saved_id.is_some_and(|id| sessions.contains_key(&id)) {
                continue;
            }
            match read_session(&path, archived) {
                Ok(Some(session)) => {
                    sessions.entry(session.id).or_insert(session);
                }
                Ok(None) => {}
                Err(error) => {
                    log::warn!("Could not read Codex session {}: {error:#}", path.display())
                }
            }
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct SessionMetadata {
    id: uuid::Uuid,
    cwd: PathBuf,
    timestamp: DateTime<Utc>,
}

fn read_session(path: &Path, archived: bool) -> anyhow::Result<Option<CodexSession>> {
    // Read only the start of a rollout. Long tool output must not block history loading.
    let reader = BufReader::new(File::open(path)?.take(256 * 1024));
    let mut session = None;
    for line in reader.lines() {
        let line = line?;
        let record: serde_json::Value = match serde_json::from_str(&line) {
            Ok(record) => record,
            Err(_) => continue,
        };
        match record["type"].as_str() {
            Some("session_meta") => {
                let metadata: SessionMetadata = serde_json::from_value(record["payload"].clone())?;
                session = Some(CodexSession {
                    id: metadata.id,
                    title: "Codex".into(),
                    working_directory: metadata.cwd,
                    created_at: metadata.timestamp,
                    archived,
                });
            }
            Some("event_msg") if record["payload"]["type"] == "user_message" => {
                if let Some(session) = session.as_mut()
                    && let Some(message) = record["payload"]["message"].as_str()
                {
                    session.title = message
                        .lines()
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or("Codex")
                        .chars()
                        .take(200)
                        .collect();
                    break;
                }
            }
            _ => {}
        }
    }
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_filters_project_and_reads_archived_sessions() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let project = directory.path().join("project");
        std::fs::create_dir_all(project.join("src"))?;
        let sessions = directory.path().join("sessions/2026/09/13");
        std::fs::create_dir_all(&sessions)?;
        let archived = directory.path().join("archived_sessions");
        std::fs::create_dir_all(&archived)?;
        for (folder, working_directory, id) in [
            (&sessions, project.join("src"), uuid::Uuid::new_v4()),
            (
                &sessions,
                directory.path().join("project-other"),
                uuid::Uuid::new_v4(),
            ),
            (&archived, project.clone(), uuid::Uuid::new_v4()),
        ] {
            let metadata = serde_json::json!({"type": "session_meta", "payload": {
                "id": id, "cwd": working_directory, "timestamp": "2026-09-13T01:00:00Z"
            }});
            let message = serde_json::json!({"type": "event_msg", "payload": {
                "type": "user_message", "message": "Fix the history\nDetails"
            }});
            std::fs::write(
                folder.join(format!("rollout-{id}.jsonl")),
                format!("{metadata}\n{message}\n{{"),
            )?;
        }
        let history = load_history(directory.path(), &[project])?;
        assert_eq!(history.len(), 2);
        assert!(
            history
                .iter()
                .all(|session| session.title == "Fix the history")
        );
        assert_eq!(history.iter().filter(|session| session.archived).count(), 1);
        Ok(())
    }

    #[test]
    fn absent_codex_home_has_no_history() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        assert!(
            load_history(
                &directory.path().join("missing"),
                &[directory.path().into()]
            )?
            .is_empty()
        );
        Ok(())
    }

    #[test]
    fn database_title_takes_priority_over_rollout() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let project = directory.path().join("project");
        let id = uuid::Uuid::new_v4();
        let connection = db::sqlez::connection::Connection::open_file(
            &directory.path().join("state_5.sqlite").to_string_lossy(),
        );
        connection.exec("CREATE TABLE threads (id TEXT, title TEXT, cwd TEXT, created_at INTEGER, archived INTEGER)")?()?;
        connection.exec_bound("INSERT INTO threads VALUES (?, ?, ?, ?, ?)")?((
            id.to_string(),
            "Renamed session",
            project.to_string_lossy().to_string(),
            1_789_257_600_i64,
            false,
        ))?;
        let sessions = directory.path().join("sessions");
        std::fs::create_dir_all(&sessions)?;
        std::fs::write(sessions.join(format!("rollout-{id}.jsonl")), "invalid")?;
        let read_only = db::sqlez::connection::Connection::open_read_only(
            &directory.path().join("state_5.sqlite"),
        )?;
        assert!(!read_only.can_write());
        assert!(read_only.exec("DELETE FROM threads").is_err());
        let history = load_history(directory.path(), &[project])?;
        assert_eq!(history.len(), 1);
        assert_eq!(
            history.first().map(|session| session.title.as_str()),
            Some("Renamed session")
        );
        Ok(())
    }
}
