//! Wire types shared between `agent_control_cli` (the `pentipctl`
//! binary an agent's own CLI process invokes) and `agent_ui::agent_control`
//! (the local control server inside Pentip that handles the request). Kept
//! dependency-free of GPUI/terminal so the CLI binary stays small.

use std::path::PathBuf;
#[cfg(unix)]
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub const DEFAULT_READ_LINES: usize = 120;
pub const MAX_READ_LINES: usize = 10_000;
pub const FRAME_LENGTH_BYTES: usize = 4;
pub const SUPPORTED_TERMINAL_KEY_NAMES: &[&str] = &[
    "enter",
    "escape",
    "tab",
    "backspace",
    "delete",
    "insert",
    "home",
    "end",
    "pageup",
    "pagedown",
    "up",
    "down",
    "left",
    "right",
    "f1",
    "f2",
    "f3",
    "f4",
    "f5",
    "f6",
    "f7",
    "f8",
    "f9",
    "f10",
    "f11",
    "f12",
];

pub fn is_supported_terminal_key(value: &str) -> bool {
    let mut parts = value.split('-').peekable();
    while let Some(part) = parts.peek().copied() {
        if matches!(part, "ctrl" | "alt" | "shift" | "cmd") {
            parts.next();
        } else {
            break;
        }
    }
    let Some(key) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    key.chars().count() == 1 || SUPPORTED_TERMINAL_KEY_NAMES.contains(&key)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    MissingLength,
    TooLarge { length: usize, maximum: usize },
    LengthMismatch { declared: usize, actual: usize },
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingLength => write!(formatter, "frame has no length prefix"),
            Self::TooLarge { length, maximum } => {
                write!(formatter, "frame length {length} exceeds maximum {maximum}")
            }
            Self::LengthMismatch { declared, actual } => write!(
                formatter,
                "frame declares {declared} bytes but contains {actual} bytes"
            ),
        }
    }
}

impl std::error::Error for FrameError {}

pub fn frame_payload(payload: &[u8], maximum: usize) -> Result<Vec<u8>, FrameError> {
    if payload.len() > maximum || payload.len() > u32::MAX as usize {
        return Err(FrameError::TooLarge {
            length: payload.len(),
            maximum,
        });
    }
    let mut frame = Vec::with_capacity(FRAME_LENGTH_BYTES + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

pub fn decode_frame(frame: &[u8], maximum: usize) -> Result<&[u8], FrameError> {
    let length_bytes: [u8; FRAME_LENGTH_BYTES] = frame
        .get(..FRAME_LENGTH_BYTES)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(FrameError::MissingLength)?;
    let declared = u32::from_be_bytes(length_bytes) as usize;
    if declared > maximum {
        return Err(FrameError::TooLarge {
            length: declared,
            maximum,
        });
    }
    let payload = &frame[FRAME_LENGTH_BYTES..];
    if payload.len() != declared {
        return Err(FrameError::LengthMismatch {
            declared,
            actual: payload.len(),
        });
    }
    Ok(payload)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 1, minor: 2 };

/// `stable` | `dev` | `nightly` | `preview`. A local re-derivation of
/// `release_channel::RELEASE_CHANNEL_NAME`'s own logic, not a dependency on
/// that crate: `release_channel` itself depends on `gpui`, which would drag
/// the entire GPUI/rendering stack into `agent_control_cli`, defeating this
/// crate's whole purpose of keeping that binary small.
#[cfg(unix)]
static RELEASE_CHANNEL_NAME: LazyLock<String> = LazyLock::new(|| {
    if cfg!(debug_assertions) {
        std::env::var("ZED_RELEASE_CHANNEL")
            .unwrap_or_else(|_| include_str!("../../zed/RELEASE_CHANNEL").trim().to_string())
    } else {
        include_str!("../../zed/RELEASE_CHANNEL").trim().to_string()
    }
});

/// The control socket's path, computed identically by the server
/// (`agent_ui::agent_control`) and the client (`agent_control_cli`) so
/// neither needs to hand the other a value at runtime -- it's a pure
/// function of the user's data directory and release channel, unaffected by
/// whatever a coding agent's own subprocess exec strips from custom
/// environment variables (the reason this crate exists at all: see
/// `AgentControlLocation`'s doc comment for the env-var failure this
/// replaced).
#[cfg(unix)]
pub fn socket_path() -> PathBuf {
    paths::data_dir().join(format!("agent-control-{}.sock", *RELEASE_CHANNEL_NAME))
}

/// Path to the marker file at which Pentip records where its own
/// `pentipctl` executable lives, so an agent's own CLI process can
/// discover what command to run in the first place. One location per
/// release channel; rewritten on each Pentip launch so it always refers to the
/// current installed version. It is not per-thread because the executable's
/// location is the same for every thread in one Pentip session.
#[cfg(unix)]
pub fn executable_location_path() -> PathBuf {
    paths::data_dir().join(format!(
        "agent-control-{}-executable.json",
        *RELEASE_CHANNEL_NAME
    ))
}

/// Contents of the file at `executable_location_path()`.
///
/// This is the only handoff data left on disk. The platform endpoint is
/// independently computable (`socket_path()` or `pipe_name()`), and caller identity is
/// established by the server asking the kernel who actually connected
/// (`SO_PEERCRED` on Linux, `LOCAL_PEERPID` on macOS, or a named-pipe peer
/// process query on Windows) rather than by a
/// client-presented secret -- so there's no per-thread token to mint,
/// deliver, or collide over. The original design used environment variables
/// for all of this, but some coding-agent CLIs run their own shell commands
/// through a subprocess exec that silently strips custom env vars (observed
/// with Codex CLI's shell execution path, independent of its documented
/// `shell_environment_policy` setting -- explicit `inherit = "all"` did not
/// fix it); a plain file read is not subject to that filtering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentControlLocation {
    pub executable: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlRequest {
    pub protocol: ProtocolVersion,
    #[serde(flatten)]
    pub command: ControlCommand,
}

impl ControlRequest {
    pub fn current(command: ControlCommand) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            command,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum ControlCommand {
    Status,
    TerminalCurrent,
    TerminalList(TerminalListRequest),
    TerminalOpen(TerminalOpenRequest),
    TerminalSplit(TerminalSplitRequest),
    TerminalRead(TerminalReadRequest),
    TerminalSendText(TerminalSendTextRequest),
    TerminalSendKey(TerminalSendKeyRequest),
    TerminalRun(TerminalRunRequest),
    TerminalWaitOutput(TerminalWaitOutputRequest),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TerminalControlId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalListRequest {
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalOpenRequest {
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub focus: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalSplitRequest {
    #[serde(default)]
    pub current: bool,
    pub terminal_id: Option<TerminalControlId>,
    pub direction: String,
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub focus: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalReadSource {
    Visible,
    #[default]
    Recent,
    RecentUnwrapped,
    /// The same bottom-anchored physical lines as `Recent`, but always sized
    /// to the terminal's current row count rather than `lines` -- a
    /// canonical "what does the screen show right now" snapshot for
    /// agent-state-detection heuristics, independent of whatever line count
    /// a caller happened to request.
    Detection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalReadRequest {
    pub terminal_id: TerminalControlId,
    #[serde(default)]
    pub source: TerminalReadSource,
    #[serde(default = "default_read_lines")]
    pub lines: usize,
    /// Read only the output appended after this cursor, instead of the usual
    /// bounded snapshot. Only valid with the default `Recent` source. A
    /// cursor from any prior `TerminalRead`/`TerminalWaitOutput` response
    /// can be used here; the server rejects one it can no longer find in the
    /// terminal's retained output with `CursorExpired`, since it can no
    /// longer prove no output was missed.
    #[serde(default)]
    pub since: Option<TerminalReadCursor>,
}

fn default_read_lines() -> usize {
    DEFAULT_READ_LINES
}

/// An opaque position in a terminal's output stream, handed back on every
/// read so a later call can pass it as `since` to read only what's new.
/// Callers must treat this as opaque and never construct or inspect one by
/// hand -- it's matched against the terminal's retained output as a literal
/// string, not interpreted as a line number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalReadCursor {
    pub anchor: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalSendTextRequest {
    pub terminal_id: TerminalControlId,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalSendKeyRequest {
    pub terminal_id: TerminalControlId,
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalRunRequest {
    pub terminal_id: TerminalControlId,
    #[serde(rename = "text")]
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "match-kind", content = "pattern", rename_all = "kebab-case")]
pub enum TerminalOutputMatcher {
    Literal(String),
    Regex(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalWaitOutputRequest {
    pub terminal_id: TerminalControlId,
    #[serde(default)]
    pub source: TerminalReadSource,
    #[serde(default = "default_read_lines")]
    pub lines: usize,
    pub matcher: TerminalOutputMatcher,
    pub timeout_millis: u64,
}

/// The connecting process could not yet be matched to a registered thread --
/// either because it hasn't finished registering (the terminal spawned very
/// recently and `AgentThreadStore::register` hasn't run yet), or because it
/// genuinely isn't one Pentip is tracking. These two cases are indistinguishable
/// from the server's side without extra bookkeeping this design deliberately
/// avoids, so both get `NotReady` rather than a hard error: the CLI retries
/// with bounded backoff, and a request that's never going to match simply
/// exhausts its retries and reports failure like any other unauthorized call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlResponse {
    pub protocol: ProtocolVersion,
    #[serde(flatten)]
    pub result: ControlResult,
}

impl ControlResponse {
    pub fn ok(success: ControlSuccess) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            result: ControlResult::Ok(success),
        }
    }

    pub fn not_ready() -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            result: ControlResult::NotReady,
        }
    }

    pub fn error(code: ControlErrorCode, message: impl Into<String>) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            result: ControlResult::Error(ControlError {
                code,
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", content = "result", rename_all = "kebab-case")]
pub enum ControlResult {
    Ok(ControlSuccess),
    NotReady,
    Error(ControlError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlError {
    pub code: ControlErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ControlErrorCode {
    CallerNotRecognized,
    TerminalNotFound,
    TerminalOutsideWorkspace,
    TerminalExited,
    InvalidKey,
    InvalidPattern,
    InvalidRequest,
    InvalidWorkingDirectory,
    InvalidSplitDirection,
    InvalidPlacement,
    TerminalCreateFailed,
    TerminalPlacementFailed,
    CursorExpired,
    Timeout,
    ResponseTooLarge,
    UnsupportedProtocol,
    RemoteControlUnavailable,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "kebab-case")]
pub enum ControlSuccess {
    Status(StatusResult),
    TerminalCurrent(TerminalMetadata),
    TerminalList(Vec<TerminalMetadata>),
    TerminalCreated(TerminalMetadata),
    TerminalRead(TerminalSnapshot),
    TerminalInputAccepted,
    TerminalWaitOutput(TerminalSnapshot),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResult {
    pub app_version: String,
    pub protocol_version: ProtocolVersion,
    pub release_channel: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalMetadata {
    pub id: TerminalControlId,
    pub title: String,
    pub working_directory: Option<PathBuf>,
    /// The agent CLI that the Agent Panel started in this terminal, if any.
    pub agent: Option<String>,
    pub has_exited: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalSnapshot {
    pub terminal: TerminalMetadata,
    pub source: TerminalReadSource,
    pub text: String,
    pub alternate_screen: bool,
    pub truncated: bool,
    /// Pass this back as `TerminalReadRequest::since` to read only the
    /// output appended after this snapshot.
    pub cursor: TerminalReadCursor,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_frame_round_trips_and_eof_json_is_rejected() {
        let payload = br#"{"command":"status"}"#;
        let frame = frame_payload(payload, MAX_REQUEST_BYTES).expect("frame payload");
        assert_eq!(
            decode_frame(&frame, MAX_REQUEST_BYTES),
            Ok(payload.as_slice())
        );
        assert_eq!(
            decode_frame(payload, MAX_REQUEST_BYTES),
            Err(FrameError::TooLarge {
                length: u32::from_be_bytes(*b"{\"co") as usize,
                maximum: MAX_REQUEST_BYTES,
            })
        );
    }

    #[test]
    fn frame_rejects_mismatched_and_oversized_lengths() {
        assert!(matches!(
            decode_frame(&[0, 0, 0, 2, b'a'], MAX_REQUEST_BYTES),
            Err(FrameError::LengthMismatch { .. })
        ));
        assert!(matches!(
            decode_frame(&[0, 0, 0, 5], 4),
            Err(FrameError::TooLarge { .. })
        ));
    }

    #[test]
    fn terminal_key_validation_accepts_documented_keys_and_modifiers() {
        for key in ["enter", "escape", "ctrl-c", "alt-left", "shift-f12"] {
            assert!(is_supported_terminal_key(key), "{key}");
        }
        for key in ["", "ctrl-", "not-a-key", "ctrl-not-a-key"] {
            assert!(!is_supported_terminal_key(key), "{key}");
        }
    }

    #[test]
    fn terminal_creation_requests_keep_raw_direction_and_placement_options() {
        let split: ControlRequest = serde_json::from_value(serde_json::json!({
            "protocol": { "major": 1, "minor": 1 },
            "command": "terminal-split",
            "current": true,
            "terminal_id": null,
            "direction": "diagonal",
            "cwd": "/tmp",
            "focus": true
        }))
        .expect("decode raw split request");

        match split.command {
            ControlCommand::TerminalSplit(request) => {
                assert!(request.current);
                assert_eq!(request.direction, "diagonal");
                assert_eq!(request.cwd, Some(PathBuf::from("/tmp")));
                assert!(request.focus);
            }
            other => panic!("expected terminal split, got {other:?}"),
        }

        let open = ControlRequest::current(ControlCommand::TerminalOpen(TerminalOpenRequest {
            cwd: Some(PathBuf::from("/var/tmp")),
            focus: false,
        }));
        let json = serde_json::to_value(open).expect("encode terminal open");
        assert_eq!(json["command"], "terminal-open");
        assert_eq!(json["cwd"], "/var/tmp");
        assert_eq!(json["focus"], false);
    }

    #[test]
    fn terminal_created_response_returns_immediately_addressable_metadata() {
        let response = serde_json::json!({
            "protocol": { "major": 1, "minor": 1 },
            "status": "ok",
            "result": {
                "kind": "terminal-created",
                "data": {
                    "id": "terminal-18-test",
                    "title": "shell",
                    "working_directory": "/tmp",
                    "agent": null,
                    "has_exited": false
                }
            }
        });

        assert!(serde_json::from_value::<ControlResponse>(response).is_ok());
    }

    #[test]
    fn response_ignores_unknown_minor_version_fields() {
        let json = r#"{
            "protocol":{"major":1,"minor":1},
            "status":"ok",
            "result":{
                "kind":"status",
                "data":{
                    "app_version":"1.2.3",
                    "protocol_version":{"major":1,"minor":1},
                    "release_channel":"stable",
                    "capabilities":["terminal-read"],
                    "future_field":true
                }
            },
            "future_field":true
        }"#;

        let response = serde_json::from_str::<ControlResponse>(json).expect("deserialize");
        assert!(matches!(
            response.result,
            ControlResult::Ok(ControlSuccess::Status(_))
        ));
    }

    #[test]
    fn agent_control_location_round_trips_through_json() {
        let location = AgentControlLocation {
            executable: PathBuf::from("/Applications/Pentip.app/Contents/MacOS/pentipctl"),
        };
        let json = serde_json::to_string(&location).expect("serialize");
        let decoded: AgentControlLocation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.executable, location.executable);
    }

    #[test]
    fn terminal_run_request_round_trips_without_conflicting_command_fields() {
        let request = ControlRequest::current(ControlCommand::TerminalRun(TerminalRunRequest {
            terminal_id: TerminalControlId("terminal-1".to_string()),
            command: "pentipctl --help".to_string(),
        }));

        let json = serde_json::to_value(&request).expect("serialize");
        assert_eq!(json["command"], "terminal-run");
        assert_eq!(json["text"], "pentipctl --help");

        let decoded: ControlRequest = serde_json::from_value(json).expect("deserialize");
        match decoded.command {
            ControlCommand::TerminalRun(request) => {
                assert_eq!(
                    request.terminal_id,
                    TerminalControlId("terminal-1".to_string())
                );
                assert_eq!(request.command, "pentipctl --help");
            }
            other => panic!("expected TerminalRun, got {other:?}"),
        }
    }

    #[test]
    fn terminal_read_request_omits_since_by_default_and_round_trips_when_present() {
        let json = r#"{
            "protocol":{"major":1,"minor":0},
            "command":"terminal-read",
            "terminal_id":"terminal-1"
        }"#;
        let decoded: ControlRequest = serde_json::from_str(json).expect("deserialize");
        match decoded.command {
            ControlCommand::TerminalRead(request) => assert_eq!(request.since, None),
            other => panic!("expected TerminalRead, got {other:?}"),
        }

        let request = ControlRequest::current(ControlCommand::TerminalRead(TerminalReadRequest {
            terminal_id: TerminalControlId("terminal-1".to_string()),
            source: TerminalReadSource::Recent,
            lines: DEFAULT_READ_LINES,
            since: Some(TerminalReadCursor {
                anchor: "previous output\n".to_string(),
            }),
        }));
        let json = serde_json::to_string(&request).expect("serialize");
        let decoded: ControlRequest = serde_json::from_str(&json).expect("deserialize");
        match decoded.command {
            ControlCommand::TerminalRead(request) => assert_eq!(
                request.since,
                Some(TerminalReadCursor {
                    anchor: "previous output\n".to_string(),
                })
            ),
            other => panic!("expected TerminalRead, got {other:?}"),
        }
    }

    #[test]
    fn terminal_read_source_detection_serializes_as_kebab_case() {
        let json = serde_json::to_value(TerminalReadSource::Detection).expect("serialize");
        assert_eq!(json, "detection");
        let decoded: TerminalReadSource =
            serde_json::from_value(json).expect("deserialize detection");
        assert_eq!(decoded, TerminalReadSource::Detection);
    }

    #[test]
    fn terminal_snapshot_carries_its_read_cursor_through_json() {
        let snapshot = TerminalSnapshot {
            terminal: TerminalMetadata {
                id: TerminalControlId("terminal-1".to_string()),
                title: "zsh".to_string(),
                working_directory: None,
                agent: None,
                has_exited: false,
            },
            source: TerminalReadSource::Recent,
            text: "hello".to_string(),
            alternate_screen: false,
            truncated: false,
            cursor: TerminalReadCursor {
                anchor: "hello".to_string(),
            },
        };
        let json = serde_json::to_string(&snapshot).expect("serialize");
        let decoded: TerminalSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.cursor, snapshot.cursor);
    }

    #[cfg(unix)]
    #[test]
    fn socket_path_and_executable_location_path_are_release_channel_scoped_siblings() {
        let socket = socket_path();
        let executable_location = executable_location_path();
        assert_eq!(socket.parent(), executable_location.parent());
        assert_ne!(socket, executable_location);
    }
}
