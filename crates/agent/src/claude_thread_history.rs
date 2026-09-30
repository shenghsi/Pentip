use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(Clone, Debug)]
pub struct ClaudeSession {
    pub id: uuid::Uuid,
    pub title: String,
    pub working_directory: PathBuf,
    pub updated_at: DateTime<Utc>,
}

impl ClaudeSession {
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
    claude_home: &Path,
    project_paths: &[PathBuf],
) -> anyhow::Result<Vec<ClaudeSession>> {
    let project_paths = project_paths
        .iter()
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()))
        .collect::<Vec<_>>();
    let mut sessions = HashMap::new();
    read_global_history(&claude_home.join("history.jsonl"), &mut sessions)?;
    read_project_history(&claude_home.join("projects"), &mut sessions)?;
    Ok(sessions
        .into_values()
        .filter(|session| session.matches_project(&project_paths))
        .collect())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryRecord {
    session_id: uuid::Uuid,
    project: PathBuf,
    display: String,
    timestamp: i64,
}

fn read_global_history(
    path: &Path,
    sessions: &mut HashMap<uuid::Uuid, ClaudeSession>,
) -> anyhow::Result<()> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for line in BufReader::new(file).lines() {
        let Ok(record) = serde_json::from_str::<HistoryRecord>(&line?) else {
            continue;
        };
        let Some(updated_at) = DateTime::from_timestamp_millis(record.timestamp) else {
            continue;
        };
        let title = normalized_title(&record.display).unwrap_or_else(|| "Claude".to_string());
        let session = ClaudeSession {
            id: record.session_id,
            title,
            working_directory: record.project,
            updated_at,
        };
        if sessions
            .get(&session.id)
            .is_none_or(|existing| existing.updated_at < session.updated_at)
        {
            sessions.insert(session.id, session);
        }
    }
    Ok(())
}

fn read_project_history(
    directory: &Path,
    sessions: &mut HashMap<uuid::Uuid, ClaudeSession>,
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
            read_project_history(&path, sessions)?;
        } else if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            if let Err(error) = read_project_session(&path, sessions) {
                log::warn!(
                    "Could not read Claude session {}: {error:#}",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

fn read_project_session(
    path: &Path,
    sessions: &mut HashMap<uuid::Uuid, ClaudeSession>,
) -> anyhow::Result<()> {
    let mut session_id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| uuid::Uuid::parse_str(stem).ok());
    let mut working_directory = None;
    let mut title = None;
    let mut updated_at = None;
    for line in BufReader::new(File::open(path)?).lines() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&line?) else {
            continue;
        };
        session_id = record["sessionId"]
            .as_str()
            .and_then(|value| uuid::Uuid::parse_str(value).ok())
            .or(session_id);
        working_directory = record["cwd"]
            .as_str()
            .map(PathBuf::from)
            .or(working_directory);
        updated_at = record["timestamp"]
            .as_str()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
            .or(updated_at);
        if record["type"] == "ai-title" {
            title = record["title"]
                .as_str()
                .and_then(normalized_title)
                .or(title);
        } else if title.is_none() && record["type"] == "user" && record["isMeta"] != true {
            title = message_title(&record["message"]["content"]);
        }
    }
    let (Some(id), Some(working_directory), Some(updated_at)) =
        (session_id, working_directory, updated_at)
    else {
        return Ok(());
    };
    let session = ClaudeSession {
        id,
        title: title.unwrap_or_else(|| "Claude".to_string()),
        working_directory,
        updated_at,
    };
    if sessions
        .get(&id)
        .is_none_or(|existing| existing.updated_at <= session.updated_at)
    {
        sessions.insert(id, session);
    }
    Ok(())
}

fn message_title(content: &serde_json::Value) -> Option<String> {
    if let Some(content) = content.as_str() {
        return normalized_title(content);
    }
    content.as_array()?.iter().find_map(|block| {
        (block["type"] == "text")
            .then(|| block["text"].as_str().and_then(normalized_title))
            .flatten()
    })
}

fn normalized_title(value: &str) -> Option<String> {
    let value = value.lines().find(|line| !line.trim().is_empty())?.trim();
    (!value.is_empty()).then(|| value.chars().take(200).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_reads_project_sessions_and_global_fallbacks() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let project = directory.path().join("project");
        std::fs::create_dir_all(&project)?;
        let claude_home = directory.path().join(".claude");
        let projects = claude_home.join("projects/project");
        std::fs::create_dir_all(&projects)?;
        let project_session_id = uuid::Uuid::new_v4();
        let global_session_id = uuid::Uuid::new_v4();
        std::fs::write(
            projects.join(format!("{project_session_id}.jsonl")),
            format!(
                "{{\"type\":\"user\",\"sessionId\":\"{project_session_id}\",\"cwd\":{},\"timestamp\":\"2026-09-13T01:00:00Z\",\"message\":{{\"content\":\"Initial request\"}}}}\n{{\"type\":\"ai-title\",\"sessionId\":\"{project_session_id}\",\"cwd\":{},\"timestamp\":\"2026-09-13T02:00:00Z\",\"title\":\"Generated title\"}}\n",
                serde_json::to_string(&project)?,
                serde_json::to_string(&project)?,
            ),
        )?;
        std::fs::write(
            claude_home.join("history.jsonl"),
            format!(
                "{{\"sessionId\":\"{global_session_id}\",\"project\":{},\"display\":\"Global title\",\"timestamp\":1789261200000}}\n",
                serde_json::to_string(&project)?,
            ),
        )?;

        let sessions = load_history(&claude_home, std::slice::from_ref(&project))?;

        assert_eq!(sessions.len(), 2);
        assert_eq!(
            sessions
                .iter()
                .find(|session| session.id == project_session_id)
                .map(|session| session.title.as_str()),
            Some("Generated title")
        );
        assert!(
            sessions.iter().any(|session| {
                session.id == global_session_id && session.title == "Global title"
            })
        );
        Ok(())
    }
}
