//! The local-only server backing agent-initiated control of the agent panel
//! terminals: an agent's own CLI process invokes the `pentipctl` binary
//! (`agent_control_cli`), which sends one JSON request over a Unix socket
//! per invocation.
//!
//! Caller identity is established by asking the kernel who actually
//! connected -- `LOCAL_PEERPID` on macOS, `SO_PEERCRED` on Linux -- and
//! walking that process's parent-PID ancestry looking for a PID that is the
//! process of an agent panel terminal. There is no client-presented secret,
//! and no environment variable: some agent CLIs remove custom environment
//! variables from the commands they run.

use gpui::{AnyWindowHandle, App, Global, WeakEntity};

use crate::agent_panel::AgentPanel;

#[derive(Default)]
struct ControlPanels(Vec<(WeakEntity<AgentPanel>, AnyWindowHandle)>);

impl Global for ControlPanels {}

pub(crate) fn register_panel(panel: WeakEntity<AgentPanel>, window: AnyWindowHandle, cx: &mut App) {
    let panels = cx.default_global::<ControlPanels>();
    panels.0.retain(|(panel, _)| panel.upgrade().is_some());
    panels.0.push((panel, window));
}

#[cfg(not(unix))]
pub(crate) fn init(_cx: &mut App) {}

#[cfg(unix)]
pub(crate) use server::init;

#[cfg(unix)]
mod server {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use agent_control_protocol::{
        ControlCommand, ControlErrorCode, ControlRequest, ControlResponse, ControlSuccess,
        FRAME_LENGTH_BYTES, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, PROTOCOL_VERSION, StatusResult,
        TerminalControlId, TerminalMetadata, TerminalOpenRequest, TerminalOutputMatcher,
        TerminalReadRequest, TerminalReadSource, TerminalRunRequest, TerminalSendKeyRequest,
        TerminalSendTextRequest, TerminalSnapshot, TerminalSplitRequest, TerminalWaitOutputRequest,
        frame_payload,
    };
    use anyhow::{Context as _, Result};
    use collections::HashMap;
    use gpui::{AnyWindowHandle, App, AppContext as _, AsyncApp, Entity, Global, Keystroke, Task};
    use settings::Settings as _;
    use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use smol::net::unix::{UnixListener, UnixStream};
    use terminal::Terminal;
    use util::ResultExt as _;
    use workspace::SplitDirection;

    use super::ControlPanels;
    use crate::TerminalId;
    use crate::agent_panel::{AgentControlTerminal, AgentPanel};

    /// How far up a connecting process's parent chain to look for a match
    /// before giving up. Generous relative to the shallow real-world case (the
    /// CLI is typically a direct child of the tracked terminal process, or one
    /// shell layer removed) without risking an unbounded walk.
    const MAX_ANCESTRY_DEPTH: usize = 32;

    const TERMINAL_START_TIMEOUT: Duration = Duration::from_secs(15);
    const TERMINAL_START_POLL_INTERVAL: Duration = Duration::from_millis(50);

    const RESPONSE_METADATA_ALLOWANCE: usize = 4096;
    const RESPONSE_TEXT_BYTE_BUDGET: usize = MAX_RESPONSE_BYTES - RESPONSE_METADATA_ALLOWANCE;

    struct ControlServer(#[allow(dead_code)] Task<()>);

    impl Global for ControlServer {}

    /// The terminal that sent a request, and the agent panel that owns it.
    /// Control never leaves this panel.
    struct Caller {
        panel: Entity<AgentPanel>,
        window: AnyWindowHandle,
        terminal_id: TerminalId,
    }

    /// Starts the accept loop one time and keeps its `Task` in a global so
    /// that its lifetime is the lifetime of the app.
    pub(crate) fn init(cx: &mut App) {
        if cx.has_global::<ControlServer>() {
            return;
        }
        let socket_path = agent_control_protocol::socket_path();
        write_executable_location(&agent_control_protocol::executable_location_path());
        let owns_socket = Arc::new(AtomicBool::new(false));
        let task = cx.spawn({
            let socket_path = socket_path.clone();
            let owns_socket = owns_socket.clone();
            async move |cx| {
                if let Err(error) = run_server(socket_path, owns_socket, cx).await {
                    log::error!("agent control server did not start: {error:#}");
                }
            }
        });
        cx.on_app_quit(move |_cx| {
            let socket_path = socket_path.clone();
            let owns_socket = owns_socket.clone();
            async move {
                // An instance that found a different live owner must not
                // remove a socket that it does not own.
                if owns_socket.load(Ordering::Acquire) {
                    std::fs::remove_file(&socket_path).log_err();
                }
            }
        })
        .detach();
        cx.set_global(ControlServer(task));
    }

    async fn run_server(
        socket_path: PathBuf,
        owns_socket: Arc<AtomicBool>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {parent:?}"))?;
        }

        if socket_path.exists() {
            match UnixStream::connect(&socket_path).await {
                Ok(_stream) => {
                    log::info!(
                        "another instance already owns the agent control socket at {socket_path:?}; this instance does not start a control server"
                    );
                    return Ok(());
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    // Stale: the owning process is gone.
                    std::fs::remove_file(&socket_path).log_err();
                }
                Err(error) => {
                    return Err(error).context("failed to probe the existing agent control socket");
                }
            }
        }

        let listener = UnixListener::bind(&socket_path).with_context(|| {
            format!("failed to bind the agent control socket at {socket_path:?}")
        })?;
        smol::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
            .await
            .with_context(|| format!("failed to set permissions on {socket_path:?}"))?;
        owns_socket.store(true, Ordering::Release);

        loop {
            let (stream, _) = listener
                .accept()
                .await
                .context("failed to accept an agent control connection")?;
            cx.spawn(async move |cx| {
                if let Err(error) = handle_connection(stream, cx).await {
                    log::warn!("agent control request failed: {error:#}");
                }
            })
            .detach();
        }
    }

    /// Records where the `pentipctl` executable of this instance is, so that
    /// an agent's CLI process can find the command to run.
    fn write_executable_location(executable_location_path: &Path) {
        let executable = match util::get_pentipctl_path() {
            Ok(executable) => executable,
            Err(error) => {
                log::warn!(
                    "could not find pentipctl, so agents cannot discover it through the marker file: {error:#}"
                );
                return;
            }
        };
        let result = (|| {
            if let Some(parent) = executable_location_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let location = agent_control_protocol::AgentControlLocation { executable };
            let json = serde_json::to_string_pretty(&location)?;
            let temporary_path =
                executable_location_path.with_extension(format!("{}.tmp", std::process::id()));
            std::fs::write(&temporary_path, json)?;
            std::fs::rename(&temporary_path, executable_location_path)?;
            anyhow::Ok(())
        })();
        result
            .context("failed to write the agent control executable marker")
            .log_err();
    }

    async fn handle_connection(mut stream: UnixStream, cx: &mut AsyncApp) -> Result<()> {
        let mut length_bytes = [0; FRAME_LENGTH_BYTES];
        stream
            .read_exact(&mut length_bytes)
            .await
            .context("failed to read request length")?;
        let request_length = u32::from_be_bytes(length_bytes) as usize;
        if request_length > MAX_REQUEST_BYTES {
            anyhow::bail!("request exceeds the {MAX_REQUEST_BYTES}-byte protocol limit");
        }
        let mut request_bytes = vec![0; request_length];
        stream
            .read_exact(&mut request_bytes)
            .await
            .context("failed to read request payload")?;

        let response = match serde_json::from_slice::<ControlRequest>(&request_bytes) {
            Ok(request) => match get_peer_pid(&stream) {
                Ok(peer_pid) => {
                    let mut disconnect_stream = stream.clone();
                    smol::future::race(dispatch(peer_pid, &request, cx), async move {
                        let mut byte = [0];
                        match disconnect_stream.read(&mut byte).await {
                            Ok(0) => ControlResponse::error(
                                ControlErrorCode::CallerNotRecognized,
                                "control client disconnected",
                            ),
                            Ok(_) => ControlResponse::error(
                                ControlErrorCode::InvalidRequest,
                                "connection contains data after its request frame",
                            ),
                            Err(error) => error_response(format_args!(
                                "failed to observe control client: {error}"
                            )),
                        }
                    })
                    .await
                }
                Err(error) => error_response(format_args!(
                    "could not determine caller identity: {error:#}"
                )),
            },
            Err(error) => ControlResponse::error(
                ControlErrorCode::InvalidRequest,
                format!("malformed request: {error}"),
            ),
        };

        let response_bytes = serde_json::to_vec(&response).context("failed to encode response")?;
        let response_frame = frame_payload(&response_bytes, MAX_RESPONSE_BYTES)
            .context("failed to frame response")?;
        stream
            .write_all(&response_frame)
            .await
            .context("failed to write response")?;
        stream.flush().await.context("failed to flush response")?;
        Ok(())
    }

    /// Returns the PID of the process on the other end of `stream`, per the
    /// kernel -- not anything the client claims about itself.
    #[cfg(target_os = "macos")]
    fn get_peer_pid(stream: &UnixStream) -> Result<u32> {
        let pid = nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::LocalPeerPid)
            .context("failed to read the connecting process's PID (LOCAL_PEERPID)")?;
        Ok(pid as u32)
    }

    #[cfg(target_os = "linux")]
    fn get_peer_pid(stream: &UnixStream) -> Result<u32> {
        let credentials =
            nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::PeerCredentials)
                .context("failed to read the connecting process's credentials (SO_PEERCRED)")?;
        Ok(credentials.pid() as u32)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn get_peer_pid(_stream: &UnixStream) -> Result<u32> {
        anyhow::bail!("peer-credential resolution is not supported on this platform")
    }

    async fn dispatch(
        peer_pid: u32,
        request: &ControlRequest,
        cx: &mut AsyncApp,
    ) -> ControlResponse {
        if request.protocol.major != PROTOCOL_VERSION.major {
            return ControlResponse::error(
                ControlErrorCode::UnsupportedProtocol,
                format!(
                    "unsupported protocol major {}; server supports {}",
                    request.protocol.major, PROTOCOL_VERSION.major
                ),
            );
        }
        if matches!(request.command, ControlCommand::Status) {
            let (app_version, release_channel) = cx.update(|cx| {
                (
                    release_channel::AppVersion::global(cx).to_string(),
                    release_channel::ReleaseChannel::try_global(cx)
                        .unwrap_or(*release_channel::RELEASE_CHANNEL)
                        .dev_name()
                        .to_string(),
                )
            });
            return ControlResponse::ok(ControlSuccess::Status(StatusResult {
                app_version,
                protocol_version: PROTOCOL_VERSION,
                release_channel,
                capabilities: command_capabilities(),
            }));
        }

        let Some(caller) = resolve_caller(peer_pid, cx).await else {
            return ControlResponse::not_ready();
        };

        match &request.command {
            ControlCommand::Status => error_response("status needs no caller"),
            ControlCommand::TerminalCurrent => cx.update(|cx| {
                find_terminal(&caller, caller.terminal_id, cx)
                    .map(|terminal| {
                        ControlResponse::ok(ControlSuccess::TerminalCurrent(metadata(
                            &terminal, cx,
                        )))
                    })
                    .unwrap_or_else(|| terminal_not_found(&control_id(caller.terminal_id)))
            }),
            ControlCommand::TerminalList(options) => cx.update(|cx| {
                let terminals = caller
                    .panel
                    .read(cx)
                    .control_terminals(cx)
                    .iter()
                    .filter(|terminal| options.all || terminal.id != caller.terminal_id)
                    .map(|terminal| metadata(terminal, cx))
                    .collect();
                ControlResponse::ok(ControlSuccess::TerminalList(terminals))
            }),
            ControlCommand::TerminalOpen(options) => terminal_open(&caller, options, cx).await,
            ControlCommand::TerminalSplit(options) => terminal_split(&caller, options, cx).await,
            ControlCommand::TerminalRead(read) => terminal_read(&caller, read, cx),
            ControlCommand::TerminalSendText(input) => terminal_send_text(&caller, input, cx),
            ControlCommand::TerminalSendKey(input) => terminal_send_keys(&caller, input, cx),
            ControlCommand::TerminalRun(input) => terminal_run(&caller, input, cx),
            ControlCommand::TerminalWaitOutput(wait) => {
                terminal_wait_output(&caller, wait, cx).await
            }
        }
    }

    /// Finds the agent panel terminal whose process is `peer_pid` or an
    /// ancestor of it.
    async fn resolve_caller(peer_pid: u32, cx: &mut AsyncApp) -> Option<Caller> {
        let mut callers = Vec::new();
        let mut tracked_pids = HashMap::default();
        cx.update(|cx| {
            let Some(panels) = cx.try_global::<ControlPanels>() else {
                return;
            };
            for (panel, window) in &panels.0 {
                let Some(panel) = panel.upgrade() else {
                    continue;
                };
                for terminal in panel.read(cx).control_terminals(cx) {
                    let Some(pid) = terminal.terminal.read(cx).pid() else {
                        continue;
                    };
                    tracked_pids.insert(pid.as_u32(), callers.len());
                    callers.push(Caller {
                        panel: panel.clone(),
                        window: *window,
                        terminal_id: terminal.id,
                    });
                }
            }
        });
        if tracked_pids.is_empty() {
            return None;
        }

        // The process scan reads `/proc` for every process on the host, so
        // it must not run on the foreground thread.
        let index = cx
            .background_spawn(async move { walk_ancestry_for_match(peer_pid, &tracked_pids) })
            .await?;
        callers.into_iter().nth(index)
    }

    fn walk_ancestry_for_match(peer_pid: u32, tracked_pids: &HashMap<u32, usize>) -> Option<usize> {
        let refresh = sysinfo::ProcessRefreshKind::nothing();
        let mut system = sysinfo::System::new_with_specifics(
            sysinfo::RefreshKind::nothing().with_processes(refresh),
        );
        system.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, refresh);

        let mut current = sysinfo::Pid::from_u32(peer_pid);
        for _ in 0..MAX_ANCESTRY_DEPTH {
            if let Some(index) = tracked_pids.get(&current.as_u32()) {
                return Some(*index);
            }
            let parent = system.process(current)?.parent()?;
            if parent == current {
                return None;
            }
            current = parent;
        }
        None
    }

    fn control_id(terminal_id: TerminalId) -> TerminalControlId {
        TerminalControlId(terminal_id.to_key_string())
    }

    fn metadata(terminal: &AgentControlTerminal, cx: &App) -> TerminalMetadata {
        let terminal_state = terminal.terminal.read(cx);
        TerminalMetadata {
            id: control_id(terminal.id),
            title: terminal.title.clone(),
            working_directory: terminal_state.working_directory(),
            agent: terminal.agent.clone(),
            has_exited: terminal_state.has_exited(),
        }
    }

    fn find_terminal(
        caller: &Caller,
        terminal_id: TerminalId,
        cx: &App,
    ) -> Option<AgentControlTerminal> {
        caller
            .panel
            .read(cx)
            .control_terminals(cx)
            .into_iter()
            .find(|terminal| terminal.id == terminal_id)
    }

    /// Finds a terminal of the caller's agent panel from the id in a request.
    fn accessible_terminal(
        caller: &Caller,
        id: &TerminalControlId,
        cx: &App,
    ) -> Result<AgentControlTerminal, Box<ControlResponse>> {
        TerminalId::from_key_string(&id.0)
            .ok()
            .and_then(|terminal_id| find_terminal(caller, terminal_id, cx))
            .ok_or_else(|| Box::new(terminal_not_found(id)))
    }

    fn live_terminal(
        caller: &Caller,
        id: &TerminalControlId,
        cx: &App,
    ) -> Result<Entity<Terminal>, Box<ControlResponse>> {
        let terminal = accessible_terminal(caller, id, cx)?.terminal;
        if terminal.read(cx).has_exited() {
            return Err(Box::new(ControlResponse::error(
                ControlErrorCode::TerminalExited,
                "terminal process has exited",
            )));
        }
        Ok(terminal)
    }

    /// Terminal control reaches only terminals of the local machine. A remote
    /// project's terminal runs its shell on the remote host, where this
    /// server and its working-directory checks do not apply.
    fn ensure_local_project(caller: &Caller, cx: &AsyncApp) -> Result<(), Box<ControlResponse>> {
        let is_local = cx.update(|cx| caller.panel.read(cx).is_local_project(cx));
        if is_local {
            return Ok(());
        }
        Err(Box::new(ControlResponse::error(
            ControlErrorCode::RemoteControlUnavailable,
            "agent control is not supported for remote projects",
        )))
    }

    fn validate_working_directory(cwd: Option<&Path>) -> Result<(), Box<ControlResponse>> {
        if let Some(cwd) = cwd
            && (!cwd.is_absolute() || !cwd.is_dir())
        {
            return Err(Box::new(ControlResponse::error(
                ControlErrorCode::InvalidWorkingDirectory,
                format!("{} is not an absolute existing directory", cwd.display()),
            )));
        }
        Ok(())
    }

    fn usable_working_directory(terminal: &Entity<Terminal>, cx: &App) -> Option<PathBuf> {
        terminal
            .read(cx)
            .working_directory()
            .filter(|directory| directory.is_absolute() && directory.is_dir())
    }

    async fn terminal_open(
        caller: &Caller,
        request: &TerminalOpenRequest,
        cx: &mut AsyncApp,
    ) -> ControlResponse {
        if let Err(response) = ensure_local_project(caller, cx) {
            return *response;
        }
        if let Err(response) = validate_working_directory(request.cwd.as_deref()) {
            return *response;
        }
        let creation = caller.window.update(cx, |_, window, cx| {
            let cwd = request.cwd.clone().or_else(|| {
                find_terminal(caller, caller.terminal_id, cx)
                    .and_then(|terminal| usable_working_directory(&terminal.terminal, cx))
            });
            caller.panel.update(cx, |panel, cx| {
                panel.control_open_terminal(cwd, request.focus, window, cx)
            })
        });
        match creation {
            Ok(Ok(terminal_id)) => terminal_creation_response(caller, terminal_id, cx).await,
            Ok(Err(error)) | Err(error) => {
                ControlResponse::error(ControlErrorCode::TerminalCreateFailed, error.to_string())
            }
        }
    }

    async fn terminal_split(
        caller: &Caller,
        request: &TerminalSplitRequest,
        cx: &mut AsyncApp,
    ) -> ControlResponse {
        let direction = match request.direction.as_str() {
            "left" => SplitDirection::Left,
            "right" => SplitDirection::Right,
            "up" => SplitDirection::Up,
            "down" => SplitDirection::Down,
            direction => {
                return ControlResponse::error(
                    ControlErrorCode::InvalidSplitDirection,
                    format!("invalid split direction {direction:?}"),
                );
            }
        };
        if let Err(response) = ensure_local_project(caller, cx) {
            return *response;
        }
        if let Err(response) = validate_working_directory(request.cwd.as_deref()) {
            return *response;
        }
        let target = if request.current {
            Ok(cx.update(|cx| find_terminal(caller, caller.terminal_id, cx)))
        } else if let Some(terminal_id) = request.terminal_id.as_ref() {
            cx.update(|cx| accessible_terminal(caller, terminal_id, cx).map(Some))
        } else {
            return ControlResponse::error(
                ControlErrorCode::InvalidPlacement,
                "terminal split requires one target",
            );
        };
        let target = match target {
            Ok(Some(target)) => target,
            Ok(None) => return terminal_not_found(&control_id(caller.terminal_id)),
            Err(response) => return *response,
        };
        let creation = caller.window.update(cx, |_, window, cx| {
            let cwd = request
                .cwd
                .clone()
                .or_else(|| usable_working_directory(&target.terminal, cx));
            caller.panel.update(cx, |panel, cx| {
                panel.control_split_terminal(target.id, direction, cwd, request.focus, window, cx)
            })
        });
        match creation {
            Ok(Ok(terminal_id)) => terminal_creation_response(caller, terminal_id, cx).await,
            Ok(Err(error)) | Err(error) => {
                ControlResponse::error(ControlErrorCode::InvalidPlacement, error.to_string())
            }
        }
    }

    /// The agent panel starts a terminal asynchronously. This waits until
    /// the panel has the terminal, so that the response has an id that the
    /// caller can use immediately.
    async fn terminal_creation_response(
        caller: &Caller,
        terminal_id: TerminalId,
        cx: &mut AsyncApp,
    ) -> ControlResponse {
        let deadline = Instant::now() + TERMINAL_START_TIMEOUT;
        loop {
            let created = cx.update(|cx| {
                find_terminal(caller, terminal_id, cx).map(|terminal| metadata(&terminal, cx))
            });
            if let Some(metadata) = created {
                return ControlResponse::ok(ControlSuccess::TerminalCreated(metadata));
            }
            if Instant::now() >= deadline {
                caller
                    .window
                    .update(cx, |_, window, cx| {
                        caller.panel.update(cx, |panel, cx| {
                            panel.control_abandon_terminal(terminal_id, window, cx);
                        });
                    })
                    .log_err();
                return ControlResponse::error(
                    ControlErrorCode::TerminalCreateFailed,
                    "the terminal did not start before the timeout",
                );
            }
            cx.background_executor()
                .timer(TERMINAL_START_POLL_INTERVAL)
                .await;
        }
    }

    fn snapshot_source(source: TerminalReadSource) -> terminal::ControlSnapshotSource {
        match source {
            TerminalReadSource::Visible => terminal::ControlSnapshotSource::Visible,
            TerminalReadSource::Recent => terminal::ControlSnapshotSource::Recent,
            TerminalReadSource::RecentUnwrapped => terminal::ControlSnapshotSource::RecentUnwrapped,
            TerminalReadSource::Detection => terminal::ControlSnapshotSource::Detection,
        }
    }

    fn protocol_read_cursor(
        cursor: terminal::ControlReadCursor,
    ) -> agent_control_protocol::TerminalReadCursor {
        agent_control_protocol::TerminalReadCursor {
            anchor: cursor.anchor,
        }
    }

    fn bounded_terminal_text(mut text: String) -> (String, bool) {
        if text.len() <= RESPONSE_TEXT_BYTE_BUDGET {
            return (text, false);
        }
        let mut end = RESPONSE_TEXT_BYTE_BUDGET;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        (text, true)
    }

    fn read_snapshot(
        terminal: &AgentControlTerminal,
        source: TerminalReadSource,
        lines: usize,
        cx: &App,
    ) -> TerminalSnapshot {
        let snapshot = terminal
            .terminal
            .read(cx)
            .control_snapshot(snapshot_source(source), lines);
        let (text, truncated) = bounded_terminal_text(snapshot.text);
        TerminalSnapshot {
            terminal: metadata(terminal, cx),
            source,
            text,
            alternate_screen: snapshot.alternate_screen,
            truncated,
            cursor: protocol_read_cursor(snapshot.cursor),
        }
    }

    fn terminal_read(
        caller: &Caller,
        request: &TerminalReadRequest,
        cx: &mut AsyncApp,
    ) -> ControlResponse {
        if request.lines > agent_control_protocol::MAX_READ_LINES {
            return ControlResponse::error(
                ControlErrorCode::InvalidRequest,
                format!(
                    "line count exceeds {}",
                    agent_control_protocol::MAX_READ_LINES
                ),
            );
        }
        if request.since.is_some() && request.source != TerminalReadSource::Recent {
            return ControlResponse::error(
                ControlErrorCode::InvalidRequest,
                "since is only supported with the default recent source",
            );
        }
        cx.update(|cx| {
            let terminal = match accessible_terminal(caller, &request.terminal_id, cx) {
                Ok(terminal) => terminal,
                Err(response) => return *response,
            };
            let Some(since) = request.since.clone() else {
                return ControlResponse::ok(ControlSuccess::TerminalRead(read_snapshot(
                    &terminal,
                    request.source,
                    request.lines,
                    cx,
                )));
            };
            match terminal.terminal.read(cx).control_snapshot_since(
                terminal::ControlReadCursor {
                    anchor: since.anchor,
                },
                request.lines,
                RESPONSE_TEXT_BYTE_BUDGET,
                agent_control_protocol::MAX_READ_LINES,
            ) {
                Ok(snapshot) => ControlResponse::ok(ControlSuccess::TerminalRead(TerminalSnapshot {
                    terminal: metadata(&terminal, cx),
                    source: request.source,
                    text: snapshot.text,
                    alternate_screen: snapshot.alternate_screen,
                    truncated: false,
                    cursor: protocol_read_cursor(snapshot.cursor),
                })),
                Err(terminal::ControlReadCursorExpired) => ControlResponse::error(
                    ControlErrorCode::CursorExpired,
                    "cursor is older than the terminal's retained scrollback; read again without since",
                ),
            }
        })
    }

    fn terminal_send_text(
        caller: &Caller,
        request: &TerminalSendTextRequest,
        cx: &mut AsyncApp,
    ) -> ControlResponse {
        if request.text.contains('\0') {
            return ControlResponse::error(ControlErrorCode::InvalidRequest, "text contains NUL");
        }
        cx.update(|cx| {
            let terminal = match live_terminal(caller, &request.terminal_id, cx) {
                Ok(terminal) => terminal,
                Err(response) => return *response,
            };
            terminal.update(cx, |terminal, _cx| {
                terminal.input(request.text.clone().into_bytes());
            });
            ControlResponse::ok(ControlSuccess::TerminalInputAccepted)
        })
    }

    fn terminal_send_keys(
        caller: &Caller,
        request: &TerminalSendKeyRequest,
        cx: &mut AsyncApp,
    ) -> ControlResponse {
        let keys = match request
            .keys
            .iter()
            .map(|key| {
                if !agent_control_protocol::is_supported_terminal_key(key) {
                    anyhow::bail!("unsupported terminal key {key:?}");
                }
                Keystroke::parse(key).map_err(anyhow::Error::from)
            })
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(keys) => keys,
            Err(error) => {
                return ControlResponse::error(ControlErrorCode::InvalidKey, error.to_string());
            }
        };
        cx.update(|cx| {
            let terminal = match live_terminal(caller, &request.terminal_id, cx) {
                Ok(terminal) => terminal,
                Err(response) => return *response,
            };
            let option_as_meta =
                terminal::terminal_settings::TerminalSettings::get_global(cx).option_as_meta;
            terminal.update(cx, |terminal, _cx| {
                for key in &keys {
                    terminal.try_keystroke(key, option_as_meta);
                }
            });
            ControlResponse::ok(ControlSuccess::TerminalInputAccepted)
        })
    }

    fn terminal_run(
        caller: &Caller,
        request: &TerminalRunRequest,
        cx: &mut AsyncApp,
    ) -> ControlResponse {
        if request.command.contains('\0') {
            return ControlResponse::error(
                ControlErrorCode::InvalidRequest,
                "command contains NUL",
            );
        }
        let Ok(enter) = Keystroke::parse("enter") else {
            return error_response("could not map the terminal Enter key");
        };
        cx.update(|cx| {
            let terminal = match live_terminal(caller, &request.terminal_id, cx) {
                Ok(terminal) => terminal,
                Err(response) => return *response,
            };
            let option_as_meta =
                terminal::terminal_settings::TerminalSettings::get_global(cx).option_as_meta;
            let accepted = terminal.update(cx, |terminal, _cx| {
                terminal.input(request.command.as_bytes().to_vec());
                terminal.try_keystroke(&enter, option_as_meta)
            });
            if !accepted {
                return error_response("could not map the terminal Enter key");
            }
            ControlResponse::ok(ControlSuccess::TerminalInputAccepted)
        })
    }

    async fn terminal_wait_output(
        caller: &Caller,
        request: &TerminalWaitOutputRequest,
        cx: &mut AsyncApp,
    ) -> ControlResponse {
        if request.lines > agent_control_protocol::MAX_READ_LINES {
            return ControlResponse::error(
                ControlErrorCode::InvalidRequest,
                format!(
                    "line count exceeds {}",
                    agent_control_protocol::MAX_READ_LINES
                ),
            );
        }
        let matcher: Box<dyn Fn(&str) -> bool> = match &request.matcher {
            TerminalOutputMatcher::Literal(pattern) => {
                let pattern = pattern.clone();
                Box::new(move |text| text.contains(&pattern))
            }
            TerminalOutputMatcher::Regex(pattern) => match regex::Regex::new(pattern) {
                Ok(regex) => Box::new(move |text| regex.is_match(text)),
                Err(error) => {
                    return ControlResponse::error(
                        ControlErrorCode::InvalidPattern,
                        error.to_string(),
                    );
                }
            },
        };
        let terminal = match cx.update(|cx| accessible_terminal(caller, &request.terminal_id, cx)) {
            Ok(terminal) => terminal,
            Err(response) => return *response,
        };
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(request.timeout_millis))
            .unwrap_or_else(Instant::now);
        let (output_sender, output_events) = async_channel::bounded(1);
        let _subscription = cx.update(|cx| {
            cx.subscribe(&terminal.terminal, move |_terminal, event, _cx| {
                if matches!(
                    event,
                    terminal::Event::Wakeup | terminal::Event::CloseTerminal
                ) {
                    // A full channel already has a pending wake-up.
                    output_sender.try_send(()).ok();
                }
            })
        });
        loop {
            let snapshot = cx.update(|cx| {
                find_terminal(caller, terminal.id, cx)
                    .map(|terminal| read_snapshot(&terminal, request.source, request.lines, cx))
            });
            let Some(snapshot) = snapshot else {
                return terminal_not_found(&request.terminal_id);
            };
            if matcher(&snapshot.text) {
                return ControlResponse::ok(ControlSuccess::TerminalWaitOutput(snapshot));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let output_event = async { output_events.recv().await.is_ok() };
            let timeout = async {
                cx.background_executor().timer(remaining).await;
                false
            };
            if remaining.is_zero() || !smol::future::race(output_event, timeout).await {
                return ControlResponse::error(
                    ControlErrorCode::Timeout,
                    "terminal output did not match before the timeout",
                );
            }
        }
    }

    fn terminal_not_found(id: &TerminalControlId) -> ControlResponse {
        ControlResponse::error(
            ControlErrorCode::TerminalNotFound,
            format!("terminal {} was not found", id.0),
        )
    }

    fn error_response(error: impl std::fmt::Display) -> ControlResponse {
        ControlResponse::error(ControlErrorCode::Internal, error.to_string())
    }

    fn command_capabilities() -> Vec<String> {
        [
            "status",
            "terminal-current",
            "terminal-list",
            "terminal-open",
            "terminal-split",
            "terminal-read",
            "terminal-read-since",
            "terminal-send-text",
            "terminal-send-key",
            "terminal-run",
            "terminal-wait-output",
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn caller_resolves_through_a_tracked_parent_process() {
            let this_pid = std::process::id();
            let mut child = smol::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("failed to start the child process");
            let tracked = HashMap::from_iter([(this_pid, 7)]);

            let resolved = walk_ancestry_for_match(child.id(), &tracked);
            child.kill().expect("failed to stop the child process");
            smol::block_on(child.status()).expect("failed to wait for the child process");

            assert_eq!(resolved, Some(7));
        }

        #[test]
        fn caller_without_a_tracked_ancestor_is_not_resolved() {
            let tracked = HashMap::from_iter([(u32::MAX, 0)]);

            assert_eq!(walk_ancestry_for_match(std::process::id(), &tracked), None);
        }

        #[test]
        fn terminal_text_truncation_keeps_utf8_valid() {
            let text = "é".repeat(RESPONSE_TEXT_BYTE_BUDGET);

            let (text, truncated) = bounded_terminal_text(text);

            assert!(truncated);
            assert!(text.len() <= RESPONSE_TEXT_BYTE_BUDGET);
        }
    }
}
