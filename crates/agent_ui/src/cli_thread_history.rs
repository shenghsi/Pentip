use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::terminal_thread_metadata_store::TerminalThreadMetadata;

#[derive(Clone, Debug)]
pub(crate) struct CliSession {
    pub id: uuid::Uuid,
    pub title: String,
    pub working_directory: PathBuf,
    pub updated_at: DateTime<Utc>,
    pub program: &'static str,
}

pub(crate) fn load_pi_history(
    pi_home: &Path,
    project_paths: &[PathBuf],
) -> anyhow::Result<Vec<CliSession>> {
    let project_paths = canonical_paths(project_paths);
    let mut sessions = Vec::new();
    read_pi_sessions(&pi_home.join("sessions"), &project_paths, &mut sessions)?;
    Ok(sessions)
}

fn read_pi_sessions(
    directory: &Path,
    project_paths: &[PathBuf],
    sessions: &mut Vec<CliSession>,
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
            read_pi_sessions(&path, project_paths, sessions)?;
        } else if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            match read_pi_session(&path) {
                Ok(Some(session)) if matches_project(&session.working_directory, project_paths) => {
                    sessions.push(session);
                }
                Ok(_) => {}
                Err(error) => log::warn!("Could not read Pi session {}: {error:#}", path.display()),
            }
        }
    }
    Ok(())
}

fn read_pi_session(path: &Path) -> anyhow::Result<Option<CliSession>> {
    let reader = BufReader::new(File::open(path)?.take(256 * 1024));
    let mut session = None;
    for line in reader.lines() {
        let line = line?;
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if record["type"] == "session" {
            let (Some(id), Some(working_directory), Some(timestamp)) = (
                record["id"]
                    .as_str()
                    .and_then(|value| uuid::Uuid::parse_str(value).ok()),
                record["cwd"].as_str().map(PathBuf::from),
                record["timestamp"]
                    .as_str()
                    .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                    .map(|value| value.with_timezone(&Utc)),
            ) else {
                continue;
            };
            session = Some(CliSession {
                id,
                title: "Pi".into(),
                working_directory,
                updated_at: timestamp,
                program: "pi",
            });
        } else if record["type"] == "message"
            && record["message"]["role"] == "user"
            && let Some(session) = session.as_mut()
            && let Some(title) = pi_message_title(&record["message"]["content"])
        {
            session.title = title;
            break;
        }
    }
    Ok(session)
}

fn pi_message_title(content: &serde_json::Value) -> Option<String> {
    content.as_array()?.iter().find_map(|block| {
        (block["type"] == "text")
            .then(|| normalized_title(block["text"].as_str()?))
            .flatten()
    })
}

pub(crate) fn load_agy_history(
    agy_home: &Path,
    working_directory: &Path,
) -> anyhow::Result<Vec<CliSession>> {
    let mut sessions = HashMap::new();
    for directory_name in ["conversations", "implicit"] {
        read_agy_sessions(
            &agy_home.join(directory_name),
            working_directory,
            &mut sessions,
        )?;
    }
    read_saved_agy_sessions(working_directory, &mut sessions)?;
    Ok(sessions.into_values().collect())
}

#[derive(Deserialize, Serialize)]
struct SavedAgySession {
    id: uuid::Uuid,
    title: String,
    working_directory: PathBuf,
    updated_at: DateTime<Utc>,
}

fn saved_agy_history_path() -> PathBuf {
    paths::data_dir().join("agy_terminal_history.jsonl")
}

pub fn save_agy_session(metadata: &TerminalThreadMetadata) -> anyhow::Result<()> {
    let Some(id) = metadata
        .agent_cli_session_prefix
        .as_deref()
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
    else {
        return Ok(());
    };
    let Some(working_directory) = metadata.working_directory.clone() else {
        return Ok(());
    };
    let path = saved_agy_history_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let record = SavedAgySession {
        id,
        title: metadata.display_title().to_string(),
        working_directory,
        updated_at: Utc::now(),
    };
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, &record)?;
    file.write_all(b"\n")?;
    Ok(())
}

fn read_saved_agy_sessions(
    project_path: &Path,
    sessions: &mut HashMap<uuid::Uuid, CliSession>,
) -> anyhow::Result<()> {
    let file = match File::open(saved_agy_history_path()) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for line in BufReader::new(file).lines() {
        let Ok(record) = serde_json::from_str::<SavedAgySession>(&line?) else {
            continue;
        };
        if !matches_project(&record.working_directory, &[project_path.to_path_buf()]) {
            continue;
        }
        sessions.insert(
            record.id,
            CliSession {
                id: record.id,
                title: record.title,
                working_directory: record.working_directory,
                updated_at: record.updated_at,
                program: "agy",
            },
        );
    }
    Ok(())
}

fn read_agy_sessions(
    directory: &Path,
    working_directory: &Path,
    sessions: &mut HashMap<uuid::Uuid, CliSession>,
) -> anyhow::Result<()> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file()
            || !path.extension().is_some_and(|extension| extension == "pb")
        {
            continue;
        }
        let Some(id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|value| uuid::Uuid::parse_str(value).ok())
        else {
            continue;
        };
        let modified = entry.metadata()?.modified()?;
        let session = CliSession {
            id,
            title: "Antigravity".into(),
            working_directory: working_directory.to_path_buf(),
            updated_at: DateTime::<Utc>::from(modified),
            program: "agy",
        };
        if sessions
            .get(&id)
            .is_none_or(|existing| existing.updated_at < session.updated_at)
        {
            sessions.insert(id, session);
        }
    }
    Ok(())
}

fn canonical_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()))
        .collect()
}

fn matches_project(working_directory: &Path, project_paths: &[PathBuf]) -> bool {
    let working_directory = working_directory
        .canonicalize()
        .unwrap_or_else(|_| working_directory.to_path_buf());
    project_paths
        .iter()
        .any(|project_path| working_directory.starts_with(project_path))
}

fn normalized_title(value: &str) -> Option<String> {
    let value = value.lines().find(|line| !line.trim().is_empty())?.trim();
    (!value.is_empty()).then(|| value.chars().take(200).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_history_reads_sessions_for_the_project() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let project = directory.path().join("project");
        std::fs::create_dir_all(&project)?;
        let sessions = directory.path().join("sessions/project");
        std::fs::create_dir_all(&sessions)?;
        let id = uuid::Uuid::new_v4();
        std::fs::write(
            sessions.join(format!("session_{id}.jsonl")),
            format!(
                "{{\"type\":\"session\",\"id\":\"{id}\",\"timestamp\":\"2026-09-19T02:07:25Z\",\"cwd\":{}}}\n{{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"Fix history\\nDetails\"}}]}}}}\n",
                serde_json::to_string(&project)?,
            ),
        )?;
        let history = load_pi_history(directory.path(), std::slice::from_ref(&project))?;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].title, "Fix history");
        assert_eq!(history[0].id, id);
        Ok(())
    }

    #[test]
    fn agy_history_uses_conversation_file_metadata() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let conversations = directory.path().join("conversations");
        std::fs::create_dir_all(&conversations)?;
        let id = uuid::Uuid::new_v4();
        std::fs::write(conversations.join(format!("{id}.pb")), [1, 2, 3])?;
        let history = load_agy_history(directory.path(), directory.path())?;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, id);
        assert_eq!(history[0].program, "agy");
        Ok(())
    }

    #[test]
    fn agy_history_reads_implicit_sessions_and_deduplicates_ids() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let conversations = directory.path().join("conversations");
        let implicit = directory.path().join("implicit");
        std::fs::create_dir_all(&conversations)?;
        std::fs::create_dir_all(&implicit)?;
        let id = uuid::Uuid::new_v4();
        std::fs::write(conversations.join(format!("{id}.pb")), [1])?;
        std::fs::write(implicit.join(format!("{id}.pb")), [2])?;
        let implicit_id = uuid::Uuid::new_v4();
        std::fs::write(implicit.join(format!("{implicit_id}.pb")), [3])?;

        let history = load_agy_history(directory.path(), directory.path())?;

        assert_eq!(history.len(), 2);
        assert!(history.iter().any(|session| session.id == id));
        assert!(history.iter().any(|session| session.id == implicit_id));
        Ok(())
    }
}
