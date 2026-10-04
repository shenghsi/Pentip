//! The remote half of agent control. A process in a terminal of a remote
//! project cannot reach the control socket of the Pentip application, which
//! runs on another machine. Instead it runs `pentipctl`, which is this
//! binary under another name, and which talks to an endpoint of the running
//! remote server on the same host. The server finds which terminal the
//! caller is in from the kernel-reported peer process and its ancestry, then
//! forwards the request over the existing connection to the application.
//! The application does the work with its own terminal state.
//!
//! Caller identity is never a value that the client presents.

use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use agent_control_protocol::{
    AgentControlLocation, ControlErrorCode, ControlRequest, ControlResponse, ControlResult,
    FRAME_LENGTH_BYTES, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, PROTOCOL_VERSION,
    RemoteControlEnvelope, RemoteTerminalRegistrationId, frame_payload,
};
use anyhow::{Context as _, Result};
use futures::{FutureExt as _, select};
use gpui::AppContext as _;
use parking_lot::Mutex;
use release_channel::RELEASE_CHANNEL;
use rpc::{AnyProtoClient, proto};
use serde::{Deserialize, Serialize};

const MAX_DISCOVERY_RECORDS: usize = 64;
const MAX_ANCESTRY_DEPTH: usize = 32;
const PENDING_REGISTRATION_LIFETIME: Duration = Duration::from_secs(10 * 60);
const RETRY_BACKOFFS: &[Duration] = &[
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_millis(1_000),
];
const REGISTRATION_RETRY_DELAYS_MILLIS: &[u64] = &[0, 100, 250, 500, 1_000];

#[derive(Debug, Serialize, Deserialize)]
struct DiscoveryRecord {
    endpoint: PathBuf,
    server_process_id: u32,
    protocol_major: u16,
    protocol_minor: u16,
}

struct DiscoverySet {
    records: Vec<DiscoveryRecord>,
    version_mismatch: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "remote-control-transport", rename_all = "kebab-case")]
enum EndpointRequest {
    RegisterTerminal {
        remote_terminal_registration_id: RemoteTerminalRegistrationId,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct EndpointResponse {
    claimed: bool,
}

/// What an endpoint answers to a request. A server that does not own the
/// caller answers with a claim that is not made, so that the client tries
/// the next server.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum EndpointReply {
    Control(ControlResponse),
    Claim(EndpointResponse),
}

/// The root process of a terminal: the shell wrapper that registered itself.
struct RegisteredTerminal {
    root_process_id: u32,
    root_process_start_time: u64,
    /// Set once the application accepted a request for this terminal. Before
    /// that, a stale answer means that the application has not yet learned
    /// about the terminal.
    application_verified: bool,
}

pub(crate) struct Registration {
    allocated_at: Instant,
    terminal: Option<RegisteredTerminal>,
}

pub(crate) type Registrations = HashMap<RemoteTerminalRegistrationId, Registration>;

struct RegistrationState {
    registrations: Arc<Mutex<Registrations>>,
}

/// Answers requests for a registration id. The application asks for one
/// before it starts a terminal and tells the terminal's shell wrapper to
/// claim it.
pub(crate) fn register_allocation_handler(
    session: &AnyProtoClient,
    cx: &mut gpui::App,
) -> Arc<Mutex<Registrations>> {
    let registrations = Arc::new(Mutex::new(HashMap::new()));
    let state = cx.new(|_| RegistrationState {
        registrations: registrations.clone(),
    });
    session.add_request_handler(
        state.downgrade(),
        |state,
         _envelope: rpc::TypedEnvelope<proto::AllocateRemoteTerminalRegistration>,
         mut cx| async move {
            if !cfg!(unix) {
                return Ok(proto::AllocateRemoteTerminalRegistrationResponse::default());
            }
            let server_executable = std::env::current_exe()
                .context("failed to locate the remote server executable")?
                .to_string_lossy()
                .into_owned();
            let registration_id = RemoteTerminalRegistrationId(uuid::Uuid::new_v4().to_string());
            state.update(&mut cx, |state, _cx| {
                let mut registrations = state.registrations.lock();
                prune_registrations(&mut registrations);
                registrations.insert(
                    registration_id.clone(),
                    Registration {
                        allocated_at: Instant::now(),
                        terminal: None,
                    },
                );
            });
            Ok(proto::AllocateRemoteTerminalRegistrationResponse {
                registration_id: registration_id.0,
                server_executable,
            })
        },
    );
    // The handler holds the state weakly. The state lives as long as the app.
    cx.on_app_quit(move |_cx| {
        let state = state.clone();
        async move {
            drop(state);
        }
    })
    .detach();
    registrations
}

/// Links this binary under the name `pentipctl`, which selects the control
/// client mode, and records where the link is, so that an agent on this host
/// can find the command without a `PATH` entry or an environment variable.
#[cfg(unix)]
pub(crate) fn install_command() -> Result<()> {
    let server_executable = std::env::current_exe().context("failed to locate remote server")?;
    install_command_at(
        &server_executable,
        &command_directory(),
        &agent_control_protocol::executable_location_path(),
    )
}

#[cfg(not(unix))]
pub(crate) fn install_command() -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn install_command_at(
    server_executable: &Path,
    command_directory: &Path,
    marker_path: &Path,
) -> Result<()> {
    std::fs::create_dir_all(command_directory)
        .with_context(|| format!("failed to create {command_directory:?}"))?;
    let command = command_directory.join("pentipctl");
    replace_with_symlink(server_executable, &command)?;

    let marker_directory = marker_path
        .parent()
        .context("the agent control marker has no parent directory")?;
    std::fs::create_dir_all(marker_directory)
        .with_context(|| format!("failed to create {marker_directory:?}"))?;
    let temporary_marker = marker_path.with_extension(format!("{}.tmp", std::process::id()));
    let marker = serde_json::to_vec_pretty(&AgentControlLocation {
        executable: command,
    })?;
    std::fs::write(&temporary_marker, marker)
        .with_context(|| format!("failed to write {temporary_marker:?}"))?;
    std::fs::rename(&temporary_marker, marker_path)
        .with_context(|| format!("failed to replace {marker_path:?}"))
}

#[cfg(unix)]
fn replace_with_symlink(target: &Path, link: &Path) -> Result<()> {
    let temporary = link.with_extension(format!("{}.tmp", std::process::id()));
    if let Err(error) = std::fs::remove_file(&temporary)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error).context("failed to remove a stale pentipctl link");
    }
    std::os::unix::fs::symlink(target, &temporary)
        .context("failed to create the pentipctl link")?;
    std::fs::rename(&temporary, link).context("failed to replace the pentipctl link")
}

#[cfg(not(unix))]
fn replace_with_symlink(_target: &Path, _link: &Path) -> Result<()> {
    anyhow::bail!("pentipctl is not supported on this platform")
}

#[cfg(unix)]
pub(crate) fn start(session: AnyProtoClient, cx: &mut gpui::App) -> Result<()> {
    let registrations = register_allocation_handler(&session, cx);
    let directory = control_directory();
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("failed to create remote control directory {directory:?}"))?;
    let instance = uuid::Uuid::new_v4().simple().to_string();
    // Unix socket paths are short. The directory name is a short hash of the
    // instance, not the whole id.
    let instance = &instance[..12];
    let endpoint = directory.join(format!("{instance}.sock"));
    let record_path = directory.join(format!("{instance}.json"));
    let listener = bind_endpoint(&endpoint)?;
    write_discovery_record(&record_path, &endpoint)?;

    cx.on_app_quit({
        let endpoint = endpoint.clone();
        let record_path = record_path.clone();
        move |_cx| {
            let endpoint = endpoint.clone();
            let record_path = record_path.clone();
            async move {
                remove_file_logged(&endpoint);
                remove_file_logged(&record_path);
            }
        }
    })
    .detach();

    cx.spawn(async move |cx| {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(connection) => connection,
                Err(error) => {
                    log::error!("remote control endpoint stopped accepting: {error:#}");
                    break;
                }
            };
            let session = session.clone();
            let registrations = registrations.clone();
            cx.spawn(async move |_cx| {
                if let Err(error) = handle_connection(stream, session, registrations).await {
                    log::warn!("remote pentipctl connection failed: {error:#}");
                }
            })
            .detach();
        }
        remove_file_logged(&endpoint);
        remove_file_logged(&record_path);
    })
    .detach();
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn start(session: AnyProtoClient, cx: &mut gpui::App) -> Result<()> {
    register_allocation_handler(&session, cx);
    Ok(())
}

fn remove_file_logged(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("failed to remove {path:?}: {error}");
    }
}

#[cfg(unix)]
fn bind_endpoint(endpoint: &Path) -> Result<net::async_net::UnixListener> {
    use std::os::unix::fs::PermissionsExt as _;

    let listener = net::async_net::UnixListener::bind(endpoint)
        .with_context(|| format!("failed to bind remote control endpoint {endpoint:?}"))?;
    std::fs::set_permissions(endpoint, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to protect remote control endpoint {endpoint:?}"))?;
    Ok(listener)
}

#[cfg(unix)]
async fn handle_connection(
    mut stream: net::async_net::UnixStream,
    session: AnyProtoClient,
    registrations: Arc<Mutex<Registrations>>,
) -> Result<()> {
    let peer_process_id = peer_process_id(&stream)?;
    let request = read_frame(&mut stream, MAX_REQUEST_BYTES).await?;

    if let Ok(EndpointRequest::RegisterTerminal {
        remote_terminal_registration_id,
    }) = serde_json::from_slice(&request)
    {
        let claimed = claim_registration(
            &mut registrations.lock(),
            remote_terminal_registration_id,
            peer_process_id,
        );
        return write_frame(&mut stream, &EndpointResponse { claimed }).await;
    }

    let control_request: ControlRequest =
        serde_json::from_slice(&request).context("failed to decode remote control request")?;
    let remote_terminal_registration_id = {
        let mut registrations = registrations.lock();
        prune_registrations(&mut registrations);
        resolve_registration(peer_process_id, &registrations)
    };
    let Some(remote_terminal_registration_id) = remote_terminal_registration_id else {
        return write_frame(&mut stream, &EndpointResponse { claimed: false }).await;
    };

    let envelope = serde_json::to_vec(&RemoteControlEnvelope {
        remote_terminal_registration_id: remote_terminal_registration_id.clone(),
        control_request,
    })
    .context("failed to encode remote envelope")?;
    if envelope.len() > MAX_REQUEST_BYTES {
        return write_frame(
            &mut stream,
            &ControlResponse::error(
                ControlErrorCode::InvalidRequest,
                "remote control envelope exceeds the request byte limit",
            ),
        )
        .await;
    }

    let reply = request_before_disconnect(
        session.request(proto::RemoteTerminalControl { envelope }),
        wait_for_disconnect(stream.clone()),
    )
    .await;
    let Some(reply) = reply else {
        return Ok(());
    };
    let mut response = match reply {
        Ok(reply) if reply.response.len() > MAX_RESPONSE_BYTES => ControlResponse::error(
            ControlErrorCode::ResponseTooLarge,
            "remote control response exceeds the byte limit",
        ),
        Ok(reply) => serde_json::from_slice(&reply.response)
            .context("failed to decode remote control response")?,
        Err(error) => ControlResponse::error(
            ControlErrorCode::RemoteControlUnavailable,
            format!("the matching remote session is unavailable: {error}"),
        ),
    };

    {
        let mut registrations = registrations.lock();
        let terminal = registrations
            .get_mut(&remote_terminal_registration_id)
            .and_then(|registration| registration.terminal.as_mut());
        if let Some(terminal) = terminal {
            if matches!(response.result, ControlResult::Ok(_)) {
                terminal.application_verified = true;
            } else if matches!(
                response.result,
                ControlResult::Error(ref error) if error.code == ControlErrorCode::RemoteSessionStale
            ) && !terminal.application_verified
            {
                // The application learns about the terminal a little after the
                // shell wrapper claims it. The client retries on this answer.
                response = ControlResponse::not_ready();
            }
        }
    }
    write_frame(&mut stream, &response).await
}

fn claim_registration(
    registrations: &mut Registrations,
    registration_id: RemoteTerminalRegistrationId,
    peer_process_id: u32,
) -> bool {
    prune_registrations(registrations);
    // The caller is the `register-terminal` child of the shell wrapper. The
    // wrapper is the root of the terminal's process tree.
    let root_process_id = parent_process_id(peer_process_id).unwrap_or(peer_process_id);
    let Some(root_process_start_time) = process_start_time(root_process_id) else {
        return false;
    };
    let Some(registration) = registrations.get_mut(&registration_id) else {
        return false;
    };
    if registration.terminal.is_some() {
        return false;
    }
    registration.terminal = Some(RegisteredTerminal {
        root_process_id,
        root_process_start_time,
        application_verified: false,
    });
    true
}

async fn request_before_disconnect<T>(
    request: impl Future<Output = T>,
    disconnect: impl Future<Output = ()>,
) -> Option<T> {
    let request = request.fuse();
    let disconnect = disconnect.fuse();
    futures::pin_mut!(request, disconnect);
    select! {
        response = request => Some(response),
        _ = disconnect => None,
    }
}

#[cfg(unix)]
async fn wait_for_disconnect(mut stream: net::async_net::UnixStream) {
    use futures::AsyncReadExt as _;
    let mut byte = [0; 1];
    loop {
        match stream.read(&mut byte).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

/// Runs the request of `pentipctl` against the endpoints of the servers on
/// this host. Each server answers only for terminals that it registered, so
/// the first server that claims the caller is the one that owns it.
#[cfg(unix)]
pub(crate) fn run_client(request: ControlRequest) -> Result<ControlResponse> {
    run_client_in(&request, &control_directory())
}

#[cfg(unix)]
fn run_client_in(request: &ControlRequest, directory: &Path) -> Result<ControlResponse> {
    let payload = serde_json::to_vec(request)?;
    let mut selected_endpoint: Option<PathBuf> = None;
    for attempt in 0..=RETRY_BACKOFFS.len() {
        let discovery = discovery_records_in(directory)?;
        let records = discovery
            .records
            .into_iter()
            .filter(|record| {
                selected_endpoint
                    .as_ref()
                    .is_none_or(|endpoint| endpoint == &record.endpoint)
            })
            .collect::<Vec<_>>();
        let has_records = !records.is_empty();
        let mut retry = false;
        for record in records {
            let mut stream = match std::os::unix::net::UnixStream::connect(&record.endpoint) {
                Ok(stream) => stream,
                Err(error) if selected_endpoint.is_some() => {
                    return Ok(ControlResponse::error(
                        ControlErrorCode::RemoteControlUnavailable,
                        format!("the matching remote session is unavailable: {error}"),
                    ));
                }
                Err(_) => continue,
            };
            write_sync_frame(&mut stream, &payload, MAX_REQUEST_BYTES)?;
            let reply: EndpointReply = read_sync_json(&mut stream, MAX_RESPONSE_BYTES)?;
            let response = match reply {
                EndpointReply::Control(response) => response,
                EndpointReply::Claim(reply) if !reply.claimed => continue,
                EndpointReply::Claim(_) => {
                    anyhow::bail!("the remote control endpoint returned no control response")
                }
            };
            if matches!(response.result, ControlResult::NotReady) {
                selected_endpoint = Some(record.endpoint);
                retry = true;
                break;
            }
            return Ok(response);
        }
        if discovery.version_mismatch {
            return Ok(ControlResponse::error(
                ControlErrorCode::RemoteVersionMismatch,
                "the installed pentipctl protocol does not match the available remote session",
            ));
        }
        let Some(backoff) = RETRY_BACKOFFS.get(attempt) else {
            break;
        };
        if retry || has_records {
            std::thread::sleep(*backoff);
        } else {
            break;
        }
    }
    Ok(ControlResponse::error(
        ControlErrorCode::CallerNotRecognized,
        "this process is not in a controllable Pentip remote terminal",
    ))
}

#[cfg(not(unix))]
pub(crate) fn run_client(_request: ControlRequest) -> Result<ControlResponse> {
    anyhow::bail!("remote pentipctl is not supported on this platform")
}

/// Called by the shell wrapper of a new terminal, before the shell starts.
#[cfg(unix)]
pub(crate) fn register_current_terminal(
    remote_terminal_registration_id: RemoteTerminalRegistrationId,
) -> Result<()> {
    let payload = serde_json::to_vec(&EndpointRequest::RegisterTerminal {
        remote_terminal_registration_id,
    })?;
    for delay in REGISTRATION_RETRY_DELAYS_MILLIS {
        if *delay > 0 {
            std::thread::sleep(Duration::from_millis(*delay));
        }
        for record in discovery_records_in(&control_directory())?.records {
            let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&record.endpoint) else {
                continue;
            };
            write_sync_frame(&mut stream, &payload, MAX_REQUEST_BYTES)?;
            let response: EndpointResponse = read_sync_json(&mut stream, MAX_RESPONSE_BYTES)?;
            if response.claimed {
                return Ok(());
            }
        }
    }
    anyhow::bail!("no matching Pentip remote control endpoint is available")
}

#[cfg(not(unix))]
pub(crate) fn register_current_terminal(
    _remote_terminal_registration_id: RemoteTerminalRegistrationId,
) -> Result<()> {
    anyhow::bail!("remote terminal registration is not supported on this platform")
}

fn control_directory() -> PathBuf {
    paths::data_dir()
        .join("ac")
        .join(RELEASE_CHANNEL.dev_name())
}

fn command_directory() -> PathBuf {
    paths::data_dir()
        .join("agent-control")
        .join(RELEASE_CHANNEL.dev_name())
        .join(crate::VERSION.as_str())
}

fn write_discovery_record(record_path: &Path, endpoint: &Path) -> Result<()> {
    let record = DiscoveryRecord {
        endpoint: endpoint.to_path_buf(),
        server_process_id: std::process::id(),
        protocol_major: PROTOCOL_VERSION.major,
        protocol_minor: PROTOCOL_VERSION.minor,
    };
    let temporary_path = record_path.with_extension("json.tmp");
    std::fs::write(&temporary_path, serde_json::to_vec(&record)?)?;
    std::fs::rename(temporary_path, record_path)?;
    Ok(())
}

fn discovery_records_in(directory: &Path) -> Result<DiscoverySet> {
    let mut records = Vec::new();
    let mut version_mismatch = false;
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Ok(DiscoverySet {
            records,
            version_mismatch,
        });
    };
    for entry in entries.take(MAX_DISCOVERY_RECORDS) {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<DiscoveryRecord>(&bytes) else {
            continue;
        };
        if !process_is_live(record.server_process_id) {
            continue;
        }
        if record.protocol_major != PROTOCOL_VERSION.major {
            version_mismatch = true;
            continue;
        }
        records.push(record);
    }
    records.sort_by(|left, right| left.endpoint.cmp(&right.endpoint));
    Ok(DiscoverySet {
        records,
        version_mismatch,
    })
}

fn resolve_registration(
    peer_process_id: u32,
    registrations: &Registrations,
) -> Option<RemoteTerminalRegistrationId> {
    let tracked = registrations
        .iter()
        .filter_map(|(registration_id, registration)| {
            let terminal = registration.terminal.as_ref()?;
            Some((
                terminal.root_process_id,
                (registration_id, terminal.root_process_start_time),
            ))
        })
        .collect::<HashMap<_, _>>();
    if tracked.is_empty() {
        return None;
    }
    let system = process_snapshot();
    let mut current = sysinfo::Pid::from_u32(peer_process_id);
    for _ in 0..MAX_ANCESTRY_DEPTH {
        let process = system.process(current);
        // A matching id with another start time is a reused process id.
        if let Some((registration_id, start_time)) = tracked.get(&current.as_u32())
            && process.is_some_and(|process| process.start_time() == *start_time)
        {
            return Some((*registration_id).clone());
        }
        let parent = process.and_then(sysinfo::Process::parent)?;
        if parent == current {
            return None;
        }
        current = parent;
    }
    None
}

fn process_snapshot() -> sysinfo::System {
    let refresh = sysinfo::ProcessRefreshKind::nothing();
    let mut system = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing().with_processes(refresh),
    );
    system.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, refresh);
    system
}

fn parent_process_id(process_id: u32) -> Option<u32> {
    process_snapshot()
        .process(sysinfo::Pid::from_u32(process_id))?
        .parent()
        .map(sysinfo::Pid::as_u32)
}

fn process_start_time(process_id: u32) -> Option<u64> {
    process_snapshot()
        .process(sysinfo::Pid::from_u32(process_id))
        .map(sysinfo::Process::start_time)
}

fn process_is_live(process_id: u32) -> bool {
    let processes = [sysinfo::Pid::from_u32(process_id)];
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&processes), true);
    system.process(sysinfo::Pid::from_u32(process_id)).is_some()
}

fn prune_registrations(registrations: &mut Registrations) {
    let now = Instant::now();
    let system = process_snapshot();
    registrations.retain(|_, registration| match registration.terminal.as_ref() {
        Some(terminal) => system
            .process(sysinfo::Pid::from_u32(terminal.root_process_id))
            .is_some_and(|process| process.start_time() == terminal.root_process_start_time),
        None => now.duration_since(registration.allocated_at) < PENDING_REGISTRATION_LIFETIME,
    });
}

#[cfg(target_os = "linux")]
fn peer_process_id(stream: &net::async_net::UnixStream) -> Result<u32> {
    use std::os::fd::AsRawFd as _;
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: credentials and length are writable for the duration of getsockopt.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to read peer credentials");
    }
    u32::try_from(credentials.pid).context("peer process id is invalid")
}

#[cfg(target_os = "macos")]
fn peer_process_id(stream: &net::async_net::UnixStream) -> Result<u32> {
    use std::os::fd::AsRawFd as _;
    let mut process_id: libc::pid_t = 0;
    let mut length = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: process_id and length are writable for the duration of getsockopt.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut process_id as *mut libc::pid_t).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to read peer process id");
    }
    u32::try_from(process_id).context("peer process id is invalid")
}

#[cfg(unix)]
async fn read_frame(stream: &mut net::async_net::UnixStream, maximum: usize) -> Result<Vec<u8>> {
    use futures::AsyncReadExt as _;
    let mut length = [0; FRAME_LENGTH_BYTES];
    stream.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length) as usize;
    if length > maximum {
        anyhow::bail!("frame exceeds the {maximum}-byte limit");
    }
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

#[cfg(unix)]
async fn write_frame(
    stream: &mut net::async_net::UnixStream,
    response: &impl Serialize,
) -> Result<()> {
    use futures::AsyncWriteExt as _;
    let payload = serde_json::to_vec(response)?;
    let frame = frame_payload(&payload, MAX_RESPONSE_BYTES)?;
    stream.write_all(&frame).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(unix)]
fn write_sync_frame(
    stream: &mut std::os::unix::net::UnixStream,
    payload: &[u8],
    maximum: usize,
) -> Result<()> {
    use std::io::Write as _;
    stream.write_all(&frame_payload(payload, maximum)?)?;
    stream.flush()?;
    Ok(())
}

#[cfg(unix)]
fn read_sync_json<T: serde::de::DeserializeOwned>(
    stream: &mut std::os::unix::net::UnixStream,
    maximum: usize,
) -> Result<T> {
    use std::io::Read as _;
    let mut length = [0; FRAME_LENGTH_BYTES];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > maximum {
        anyhow::bail!("frame exceeds the {maximum}-byte limit");
    }
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload)?;
    serde_json::from_slice(&payload).context("failed to decode endpoint response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn registration_for(process_id: u32) -> Registration {
        Registration {
            allocated_at: Instant::now(),
            terminal: Some(RegisteredTerminal {
                root_process_id: process_id,
                root_process_start_time: process_start_time(process_id)
                    .expect("the test process has a start time"),
                application_verified: false,
            }),
        }
    }

    #[test]
    fn client_disconnect_drops_the_in_flight_forward_request() {
        struct DropSignal(Arc<AtomicBool>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let signal = DropSignal(dropped.clone());
        let request = async move {
            let _signal = signal;
            futures::future::pending::<()>().await;
        };

        let result = smol::block_on(request_before_disconnect(request, async {}));

        assert!(result.is_none());
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn caller_resolves_through_a_registered_ancestor() {
        let id = RemoteTerminalRegistrationId("terminal".to_string());
        let mut registrations = Registrations::new();
        registrations.insert(id.clone(), registration_for(std::process::id()));
        let mut child = smol::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("failed to start the child process");

        let resolved = resolve_registration(child.id(), &registrations);
        child.kill().expect("failed to stop the child process");
        smol::block_on(child.status()).expect("failed to wait for the child process");

        assert_eq!(resolved, Some(id));
    }

    #[test]
    fn caller_with_a_reused_process_id_is_not_resolved() {
        let id = RemoteTerminalRegistrationId("terminal".to_string());
        let mut registration = registration_for(std::process::id());
        if let Some(terminal) = registration.terminal.as_mut() {
            terminal.root_process_start_time += 1;
        }
        let registrations = Registrations::from_iter([(id, registration)]);

        assert_eq!(
            resolve_registration(std::process::id(), &registrations),
            None
        );
    }

    #[test]
    fn caller_without_a_registered_ancestor_is_not_resolved() {
        let registrations = Registrations::from_iter([(
            RemoteTerminalRegistrationId("terminal".to_string()),
            Registration {
                allocated_at: Instant::now(),
                terminal: Some(RegisteredTerminal {
                    root_process_id: u32::MAX,
                    root_process_start_time: 0,
                    application_verified: false,
                }),
            },
        )]);

        assert_eq!(
            resolve_registration(std::process::id(), &registrations),
            None
        );
    }

    #[test]
    fn a_registration_is_claimed_one_time() {
        let id = RemoteTerminalRegistrationId("terminal".to_string());
        let mut registrations = Registrations::from_iter([(
            id.clone(),
            Registration {
                allocated_at: Instant::now(),
                terminal: None,
            },
        )]);

        assert!(claim_registration(
            &mut registrations,
            id.clone(),
            std::process::id()
        ));
        assert!(!claim_registration(
            &mut registrations,
            id,
            std::process::id()
        ));
        assert!(!claim_registration(
            &mut registrations,
            RemoteTerminalRegistrationId("unknown".to_string()),
            std::process::id()
        ));
    }

    #[test]
    fn expired_pending_registrations_are_pruned() {
        let id = RemoteTerminalRegistrationId("terminal".to_string());
        let mut registrations = Registrations::from_iter([(
            id,
            Registration {
                allocated_at: Instant::now() - PENDING_REGISTRATION_LIFETIME * 2,
                terminal: None,
            },
        )]);

        prune_registrations(&mut registrations);

        assert!(registrations.is_empty());
    }

    #[test]
    fn install_links_the_command_and_writes_the_marker() {
        let directory = tempfile::tempdir().expect("failed to create a temporary directory");
        let server = directory.path().join("remote-server");
        std::fs::write(&server, b"").expect("failed to write the fake server");
        let command_directory = directory.path().join("commands");
        let marker = directory.path().join("data").join("marker.json");

        install_command_at(&server, &command_directory, &marker).expect("failed to install");
        install_command_at(&server, &command_directory, &marker).expect("failed to reinstall");

        let location: AgentControlLocation =
            serde_json::from_slice(&std::fs::read(&marker).expect("failed to read the marker"))
                .expect("failed to decode the marker");
        assert_eq!(location.executable, command_directory.join("pentipctl"));
        assert_eq!(
            std::fs::read_link(&location.executable).expect("the command is not a link"),
            server
        );
    }

    #[test]
    fn discovery_ignores_dead_servers_and_flags_protocol_mismatch() {
        let directory = tempfile::tempdir().expect("failed to create a temporary directory");
        let record = |name: &str, process_id: u32, major: u16| {
            let record = DiscoveryRecord {
                endpoint: directory.path().join(format!("{name}.sock")),
                server_process_id: process_id,
                protocol_major: major,
                protocol_minor: 0,
            };
            std::fs::write(
                directory.path().join(format!("{name}.json")),
                serde_json::to_vec(&record).expect("failed to encode the record"),
            )
            .expect("failed to write the record");
        };
        record("live", std::process::id(), PROTOCOL_VERSION.major);
        record("dead", u32::MAX, PROTOCOL_VERSION.major);
        record("mismatch", std::process::id(), PROTOCOL_VERSION.major + 1);

        let discovery = discovery_records_in(directory.path()).expect("failed to discover");

        assert_eq!(discovery.records.len(), 1);
        assert!(discovery.records[0].endpoint.ends_with("live.sock"));
        assert!(discovery.version_mismatch);
    }
}
