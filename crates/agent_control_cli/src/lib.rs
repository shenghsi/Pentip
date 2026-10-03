//! Shared `pentipctl` parser and output implementation. The local binary uses
//! Pentip's local control endpoint. Caller identity comes from
//! the operating system, not CLI data.
//!
//! Sends bare, unauthenticated-looking requests on purpose: the server
//! establishes caller identity itself from the kernel-reported PID of
//! the process that connected to its endpoint, not from
//! anything this binary presents. There is nothing here to mint, deliver,
//! or leak. The endpoint is computed identically by the server and client;
//! the platform-specific override is only for testing.

use std::path::PathBuf;

use agent_control_protocol::{
    ControlCommand, ControlRequest, ControlResponse, ControlResult, ControlSuccess,
    TerminalControlId, TerminalListRequest, TerminalOpenRequest, TerminalOutputMatcher,
    TerminalReadRequest, TerminalReadSource, TerminalRunRequest, TerminalSendKeyRequest,
    TerminalSendTextRequest, TerminalSplitRequest, TerminalWaitOutputRequest,
};
use clap::{ArgGroup, Parser, Subcommand, ValueEnum};

pub const BUNDLED_SKILL: &str = include_str!("../skills/pentipctl/SKILL.md");

#[derive(Parser)]
#[command(name = "pentipctl", about = "Control a running Pentip application")]
struct Cli {
    /// Unix socket to connect to. Defaults to the same path Pentip's control
    /// server computes and binds -- only useful to override for testing.
    #[cfg(unix)]
    #[arg(long, global = true, hide = true)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Status {
        #[arg(long)]
        json: bool,
    },
    Terminal {
        #[command(subcommand)]
        command: TerminalCommand,
    },
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
}

#[derive(Subcommand)]
enum SkillCommand {
    Print,
}

#[derive(Subcommand)]
enum TerminalCommand {
    Current {
        #[arg(long)]
        json: bool,
    },
    List {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    Open {
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        focus: bool,
        #[arg(long)]
        json: bool,
    },
    #[command(group(
        ArgGroup::new("target")
            .required(true)
            .multiple(false)
            .args(["current", "terminal_id"])
    ))]
    Split {
        #[arg(long)]
        current: bool,
        #[arg(long = "terminal")]
        terminal_id: Option<String>,
        #[arg(long, value_enum)]
        direction: SplitDirectionArg,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        focus: bool,
        #[arg(long)]
        json: bool,
    },
    Read {
        terminal_id: String,
        #[arg(long, value_enum, default_value_t = ReadSourceArg::Recent)]
        source: ReadSourceArg,
        #[arg(long, default_value_t = agent_control_protocol::DEFAULT_READ_LINES)]
        lines: usize,
        /// Read only output appended after this cursor -- copy it verbatim
        /// from a prior read's `cursor` field (JSON output) or its printed
        /// "cursor" line (human output). Only valid with the default recent
        /// source.
        #[arg(long, value_parser = parse_read_cursor)]
        since: Option<agent_control_protocol::TerminalReadCursor>,
        #[arg(long)]
        json: bool,
    },
    SendText {
        terminal_id: String,
        text: String,
        #[arg(long)]
        json: bool,
    },
    SendKey {
        terminal_id: String,
        /// Key names such as enter, escape, ctrl-c, alt-left, arrows, and F1-F12.
        #[arg(required = true)]
        keys: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    Run {
        terminal_id: String,
        command: String,
        #[arg(long)]
        json: bool,
    },
    #[command(group(
        ArgGroup::new("matcher")
            .required(true)
            .multiple(false)
            .args(["match_text", "regex"])
    ))]
    WaitOutput {
        terminal_id: String,
        #[arg(long = "match")]
        match_text: Option<String>,
        #[arg(long)]
        regex: Option<String>,
        #[arg(long, value_enum, default_value_t = ReadSourceArg::Recent)]
        source: ReadSourceArg,
        #[arg(long, default_value_t = agent_control_protocol::DEFAULT_READ_LINES)]
        lines: usize,
        #[arg(long, default_value = "30s", value_parser = parse_duration_millis)]
        timeout: u64,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SplitDirectionArg {
    Left,
    Right,
    Up,
    Down,
}

impl SplitDirectionArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum ReadSourceArg {
    Visible,
    Recent,
    RecentUnwrapped,
    Detection,
}

impl From<ReadSourceArg> for TerminalReadSource {
    fn from(value: ReadSourceArg) -> Self {
        match value {
            ReadSourceArg::Visible => TerminalReadSource::Visible,
            ReadSourceArg::Recent => TerminalReadSource::Recent,
            ReadSourceArg::RecentUnwrapped => TerminalReadSource::RecentUnwrapped,
            ReadSourceArg::Detection => TerminalReadSource::Detection,
        }
    }
}

pub fn main() {
    let cli = Cli::parse();
    if let Some(result) = cli.run_local_command() {
        match result {
            Ok(()) => return,
            Err(error) => {
                eprintln!("pentipctl: {error:#}");
                std::process::exit(1);
            }
        }
    }
    #[cfg(unix)]
    let socket_override = cli.socket.clone();
    let (request, wants_json) = match cli.into_request() {
        Ok(request) => request,
        Err(error) => {
            eprintln!("pentipctl: {error:#}");
            std::process::exit(2);
        }
    };

    #[cfg(unix)]
    let result = run(request, socket_override);
    #[cfg(not(unix))]
    let result = run(request);

    match result {
        Ok(response) => std::process::exit(print_response(&response, wants_json)),
        Err(error) => {
            eprintln!("pentipctl: {error:#}");
            std::process::exit(1);
        }
    }
}

impl Cli {
    fn run_local_command(&self) -> Option<anyhow::Result<()>> {
        let Command::Skill { command } = &self.command else {
            return None;
        };
        Some(run_skill_command(command))
    }

    fn into_request(self) -> anyhow::Result<(ControlRequest, bool)> {
        let (command, wants_json) = match self.command {
            Command::Status { json } => (ControlCommand::Status, json),
            Command::Terminal { command } => match command {
                TerminalCommand::Current { json } => (ControlCommand::TerminalCurrent, json),
                TerminalCommand::List { all, json } => (
                    ControlCommand::TerminalList(TerminalListRequest { all }),
                    json,
                ),
                TerminalCommand::Open { cwd, focus, json } => (
                    ControlCommand::TerminalOpen(TerminalOpenRequest { cwd, focus }),
                    json,
                ),
                TerminalCommand::Split {
                    current,
                    terminal_id,
                    direction,
                    cwd,
                    focus,
                    json,
                } => (
                    ControlCommand::TerminalSplit(TerminalSplitRequest {
                        current,
                        terminal_id: terminal_id.map(TerminalControlId),
                        direction: direction.as_str().to_string(),
                        cwd,
                        focus,
                    }),
                    json,
                ),
                TerminalCommand::Read {
                    terminal_id,
                    source,
                    lines,
                    since,
                    json,
                } => (
                    ControlCommand::TerminalRead(TerminalReadRequest {
                        terminal_id: TerminalControlId(terminal_id),
                        source: source.into(),
                        lines,
                        since,
                    }),
                    json,
                ),
                TerminalCommand::SendText {
                    terminal_id,
                    text,
                    json,
                } => (
                    ControlCommand::TerminalSendText(TerminalSendTextRequest {
                        terminal_id: TerminalControlId(terminal_id),
                        text,
                    }),
                    json,
                ),
                TerminalCommand::SendKey {
                    terminal_id,
                    keys,
                    json,
                } => (
                    ControlCommand::TerminalSendKey(TerminalSendKeyRequest {
                        terminal_id: TerminalControlId(terminal_id),
                        keys,
                    }),
                    json,
                ),
                TerminalCommand::Run {
                    terminal_id,
                    command,
                    json,
                } => (
                    ControlCommand::TerminalRun(TerminalRunRequest {
                        terminal_id: TerminalControlId(terminal_id),
                        command,
                    }),
                    json,
                ),
                TerminalCommand::WaitOutput {
                    terminal_id,
                    match_text,
                    regex,
                    source,
                    lines,
                    timeout,
                    json,
                } => {
                    let matcher = match (match_text, regex) {
                        (Some(text), None) => TerminalOutputMatcher::Literal(text),
                        (None, Some(pattern)) => TerminalOutputMatcher::Regex(pattern),
                        _ => anyhow::bail!("select exactly one of --match or --regex"),
                    };
                    (
                        ControlCommand::TerminalWaitOutput(TerminalWaitOutputRequest {
                            terminal_id: TerminalControlId(terminal_id),
                            source: source.into(),
                            lines,
                            matcher,
                            timeout_millis: timeout,
                        }),
                        json,
                    )
                }
            },
            Command::Skill { .. } => anyhow::bail!("skill commands do not use the control server"),
        };
        Ok((ControlRequest::current(command), wants_json))
    }
}

fn run_skill_command(command: &SkillCommand) -> anyhow::Result<()> {
    match command {
        SkillCommand::Print => print!("{BUNDLED_SKILL}"),
    }
    Ok(())
}

fn parse_duration_millis(value: &str) -> Result<u64, String> {
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_000)
    } else {
        return Err("duration must end in ms, s, or m".to_string());
    };
    let number = number
        .parse::<u64>()
        .map_err(|_| "duration must start with a positive integer".to_string())?;
    number
        .checked_mul(multiplier)
        .filter(|duration| *duration > 0)
        .ok_or_else(|| "duration is out of range".to_string())
}

/// Cursors carry a raw snippet of terminal output (see
/// `agent_control_protocol::TerminalReadCursor`), which can contain
/// anything a shell argument can't safely hold -- quotes, `$`, newlines,
/// control bytes. Base64 keeps the CLI's textual `--since`/printed-cursor
/// form a single shell-safe token; `--json` output carries the cursor's
/// `anchor` field as a plain JSON string instead, with no encoding needed.
fn parse_read_cursor(value: &str) -> Result<agent_control_protocol::TerminalReadCursor, String> {
    use base64::Engine as _;
    let invalid = || "cursor must be the exact value printed by a prior read".to_string();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|_| invalid())?;
    let anchor = String::from_utf8(bytes).map_err(|_| invalid())?;
    Ok(agent_control_protocol::TerminalReadCursor { anchor })
}

fn encode_read_cursor(cursor: &agent_control_protocol::TerminalReadCursor) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(cursor.anchor.as_bytes())
}

#[cfg(unix)]
fn run(
    request: ControlRequest,
    socket_override: Option<PathBuf>,
) -> anyhow::Result<ControlResponse> {
    unix::run(request, socket_override)
}

#[cfg(not(unix))]
fn run(_request: ControlRequest) -> anyhow::Result<ControlResponse> {
    anyhow::bail!("pentipctl is not supported on this platform")
}

fn print_response(response: &ControlResponse, wants_json: bool) -> i32 {
    if wants_json {
        match serde_json::to_string(response) {
            Ok(json) => println!("{json}"),
            Err(error) => eprintln!("pentipctl: failed to encode response: {error}"),
        }
        return exit_code_for(response);
    }
    match &response.result {
        ControlResult::Ok(ControlSuccess::Status(status)) => {
            println!(
                "Pentip {} ({}, protocol {}.{})",
                status.app_version,
                status.release_channel,
                status.protocol_version.major,
                status.protocol_version.minor
            );
        }
        ControlResult::Ok(ControlSuccess::TerminalCurrent(terminal)) => {
            print_terminal(terminal);
        }
        ControlResult::Ok(ControlSuccess::TerminalList(terminals)) => {
            for terminal in terminals {
                print_terminal(terminal);
            }
        }
        ControlResult::Ok(ControlSuccess::TerminalCreated(terminal)) => {
            print_terminal(terminal);
        }
        ControlResult::Ok(ControlSuccess::TerminalRead(snapshot))
        | ControlResult::Ok(ControlSuccess::TerminalWaitOutput(snapshot)) => {
            print!("{}", snapshot.text);
            eprintln!(
                "pentipctl: cursor {} (pass as --since to read only what's new)",
                encode_read_cursor(&snapshot.cursor)
            );
        }
        ControlResult::Ok(ControlSuccess::TerminalInputAccepted) => {}
        ControlResult::NotReady => {
            eprintln!(
                "pentipctl: this process does not appear to be in a controllable Pentip terminal"
            );
        }
        ControlResult::Error(error) => {
            eprintln!(
                "pentipctl: {}: {}",
                error_code_name(error.code),
                error.message
            );
        }
    }
    exit_code_for(response)
}

fn exit_code_for(response: &ControlResponse) -> i32 {
    match &response.result {
        ControlResult::Ok(_) => 0,
        ControlResult::NotReady | ControlResult::Error(_) => 1,
    }
}

fn print_terminal(terminal: &agent_control_protocol::TerminalMetadata) {
    let working_directory = terminal
        .working_directory
        .as_deref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "-".to_string());
    println!(
        "{}\t{}\t{}",
        terminal.id.0, terminal.title, working_directory
    );
}

fn error_code_name(code: agent_control_protocol::ControlErrorCode) -> &'static str {
    use agent_control_protocol::ControlErrorCode;
    match code {
        ControlErrorCode::CallerNotRecognized => "caller-not-recognized",
        ControlErrorCode::TerminalNotFound => "terminal-not-found",
        ControlErrorCode::TerminalOutsideWorkspace => "terminal-outside-workspace",
        ControlErrorCode::TerminalExited => "terminal-exited",
        ControlErrorCode::InvalidKey => "invalid-key",
        ControlErrorCode::InvalidPattern => "invalid-pattern",
        ControlErrorCode::InvalidRequest => "invalid-request",
        ControlErrorCode::InvalidWorkingDirectory => "invalid-working-directory",
        ControlErrorCode::InvalidSplitDirection => "invalid-split-direction",
        ControlErrorCode::InvalidPlacement => "invalid-placement",
        ControlErrorCode::TerminalCreateFailed => "terminal-create-failed",
        ControlErrorCode::RemoteControlUnavailable => "remote-control-unavailable",
        ControlErrorCode::TerminalPlacementFailed => "terminal-placement-failed",
        ControlErrorCode::CursorExpired => "cursor-expired",
        ControlErrorCode::Timeout => "timeout",
        ControlErrorCode::ResponseTooLarge => "response-too-large",
        ControlErrorCode::UnsupportedProtocol => "unsupported-protocol",
        ControlErrorCode::Internal => "internal",
    }
}

#[cfg(unix)]
mod unix {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::time::Duration;

    use agent_control_protocol::{
        ControlRequest, ControlResponse, ControlResult, FRAME_LENGTH_BYTES, MAX_REQUEST_BYTES,
        MAX_RESPONSE_BYTES, frame_payload,
    };
    use anyhow::{Context as _, bail};

    /// Bounded backoff for a `NotReady` response, which means Pentip hasn't
    /// (yet, or ever will) matched the connecting process's PID ancestry to
    /// a registered thread. Keeping the wait client-side avoids parking
    /// requests inside the server.
    const RETRY_BACKOFFS: &[Duration] = &[
        Duration::from_millis(250),
        Duration::from_millis(500),
        Duration::from_millis(1000),
    ];

    pub(crate) fn run(
        request: ControlRequest,
        socket_override: Option<PathBuf>,
    ) -> anyhow::Result<ControlResponse> {
        let socket_path = socket_override.unwrap_or_else(agent_control_protocol::socket_path);

        let mut attempt = 0;
        loop {
            let response = send_once(&socket_path, &request)?;
            if !matches!(&response.result, ControlResult::NotReady) {
                return Ok(response);
            }
            if attempt >= RETRY_BACKOFFS.len() {
                return Ok(ControlResponse::error(
                    agent_control_protocol::ControlErrorCode::CallerNotRecognized,
                    "caller was not recognized before the retry deadline",
                ));
            }
            std::thread::sleep(RETRY_BACKOFFS[attempt]);
            attempt += 1;
        }
    }

    fn send_once(
        socket_path: &std::path::Path,
        request: &ControlRequest,
    ) -> anyhow::Result<ControlResponse> {
        let mut stream = UnixStream::connect(socket_path).with_context(|| {
            format!(
                "failed to connect to Pentip's agent control socket at {}",
                socket_path.display()
            )
        })?;
        let payload = serde_json::to_vec(request).context("failed to encode request")?;
        let frame =
            frame_payload(&payload, MAX_REQUEST_BYTES).context("failed to frame request")?;
        stream.write_all(&frame).context("failed to send request")?;
        let mut length_bytes = [0; FRAME_LENGTH_BYTES];
        stream
            .read_exact(&mut length_bytes)
            .context("failed to read response length")?;
        let response_length = u32::from_be_bytes(length_bytes) as usize;
        if response_length > MAX_RESPONSE_BYTES {
            bail!("response exceeds the {MAX_RESPONSE_BYTES}-byte protocol limit");
        }
        let mut response_bytes = vec![0; response_length];
        stream
            .read_exact(&mut response_bytes)
            .context("failed to read response payload")?;
        serde_json::from_slice(&response_bytes).context("failed to decode response")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_commands_parse_without_a_control_endpoint() {
        assert!(Cli::try_parse_from(["pentipctl", "skill", "print"]).is_ok());
        assert!(Cli::try_parse_from(["pentipctl", "skill", "install"]).is_err());
    }

    #[test]
    fn terminal_wait_requires_exactly_one_matcher() {
        assert!(
            Cli::try_parse_from([
                "pentipctl",
                "terminal",
                "wait-output",
                "t1",
                "--match",
                "ready"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["pentipctl", "terminal", "wait-output", "t1"]).is_err());
        assert!(
            Cli::try_parse_from([
                "pentipctl",
                "terminal",
                "wait-output",
                "t1",
                "--match",
                "ready",
                "--regex",
                "ready.*"
            ])
            .is_err()
        );
    }

    #[test]
    fn terminal_open_and_split_build_creation_requests() {
        let open = Cli::try_parse_from([
            "pentipctl",
            "terminal",
            "open",
            "--cwd",
            "/tmp",
            "--focus",
            "--json",
        ])
        .expect("parse terminal open");
        let (request, wants_json) = open.into_request().expect("build terminal open request");
        assert!(wants_json);
        assert!(matches!(
            request.command,
            ControlCommand::TerminalOpen(TerminalOpenRequest { cwd: Some(cwd), focus: true })
                if cwd.as_path() == std::path::Path::new("/tmp")
        ));

        let split = Cli::try_parse_from([
            "pentipctl",
            "terminal",
            "split",
            "--terminal",
            "t1",
            "--direction",
            "left",
        ])
        .expect("parse terminal split");
        let (request, _) = split.into_request().expect("build terminal split request");
        assert!(matches!(
            request.command,
            ControlCommand::TerminalSplit(TerminalSplitRequest {
                current: false,
                terminal_id: Some(TerminalControlId(ref id)),
                ref direction,
                focus: false,
                ..
            }) if id == "t1" && direction == "left"
        ));
    }

    #[test]
    fn terminal_split_requires_one_target_and_a_valid_direction() {
        assert!(
            Cli::try_parse_from(["pentipctl", "terminal", "split", "--direction", "right"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "pentipctl",
                "terminal",
                "split",
                "--current",
                "--terminal",
                "t1",
                "--direction",
                "right"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "pentipctl",
                "terminal",
                "split",
                "--current",
                "--direction",
                "diagonal"
            ])
            .is_err()
        );
    }

    #[test]
    fn read_cursor_round_trips_through_its_printed_encoding() {
        let cursor = agent_control_protocol::TerminalReadCursor {
            anchor: "line one\nline two".to_string(),
        };
        let encoded = encode_read_cursor(&cursor);
        // Base64 output is a single shell-safe token: no quotes, `$`, or
        // whitespace that would need escaping on a command line.
        assert!(
            encoded
                .chars()
                .all(|character| character.is_ascii_alphanumeric()
                    || character == '+'
                    || character == '/'
                    || character == '=')
        );
        assert_eq!(parse_read_cursor(&encoded), Ok(cursor));
    }

    #[test]
    fn read_cursor_rejects_malformed_input() {
        assert!(parse_read_cursor("not valid base64!!").is_err());
    }

    #[test]
    fn terminal_read_since_flag_builds_a_request_with_the_decoded_cursor() {
        let cursor = agent_control_protocol::TerminalReadCursor {
            anchor: "previous tail".to_string(),
        };
        let encoded = encode_read_cursor(&cursor);
        let cli = Cli::try_parse_from(["pentipctl", "terminal", "read", "t1", "--since", &encoded])
            .expect("parse terminal read with --since");

        let (request, _wants_json) = cli.into_request().expect("build request");
        match request.command {
            ControlCommand::TerminalRead(request) => assert_eq!(request.since, Some(cursor)),
            other => panic!("expected TerminalRead, got {other:?}"),
        }
    }

    #[test]
    fn terminal_read_without_since_defaults_to_none() {
        let cli = Cli::try_parse_from(["pentipctl", "terminal", "read", "t1"])
            .expect("parse terminal read");

        let (request, _wants_json) = cli.into_request().expect("build request");
        match request.command {
            ControlCommand::TerminalRead(request) => assert_eq!(request.since, None),
            other => panic!("expected TerminalRead, got {other:?}"),
        }
    }

    #[test]
    fn terminal_read_source_detection_is_parsed() {
        let cli = Cli::try_parse_from([
            "pentipctl",
            "terminal",
            "read",
            "t1",
            "--source",
            "detection",
        ])
        .expect("parse terminal read --source detection");

        let (request, _wants_json) = cli.into_request().expect("build request");
        match request.command {
            ControlCommand::TerminalRead(request) => {
                assert_eq!(request.source, TerminalReadSource::Detection)
            }
            other => panic!("expected TerminalRead, got {other:?}"),
        }
    }

    #[cfg(windows)]
    #[cfg(windows)]
    #[test]
    fn unsuccessful_protocol_responses_have_nonzero_exit_codes() {
        assert_eq!(exit_code_for(&ControlResponse::not_ready()), 1);
        assert_eq!(
            exit_code_for(&ControlResponse::error(
                agent_control_protocol::ControlErrorCode::Internal,
                "failed",
            )),
            1
        );
    }
}
