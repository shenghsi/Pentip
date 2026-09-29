use anyhow::{Context as _, Result, bail};
use futures::{AsyncReadExt as _, AsyncWriteExt as _};
use gpui::{App, AppContext as _, Entity, SharedString};
use http_client::{AsyncBody, HttpClient};
use remote::{RemoteArch, RemoteOs, RemotePlatform};
use sha2::{Digest as _, Sha256, Sha512};
use std::{
    cell::Cell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};
use ui::{Color, Label, LabelSize, ProgressBar, SpinnerLabel, prelude::*};
use workspace::{
    Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

const MAX_METADATA_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;

pub struct AgentRelease {
    pub agent: &'static str,
    pub version: String,
    pub source_url: String,
    source_checksum: SourceChecksum,
    package_archive: bool,
}

enum SourceChecksum {
    Sha256(String),
    Sha512(String),
}

impl SourceChecksum {
    fn value(&self) -> &str {
        match self {
            Self::Sha256(value) | Self::Sha512(value) => value,
        }
    }
}

pub struct VerifiedAgentArtifact {
    pub path: PathBuf,
    pub sha256: String,
}

pub async fn latest_release(
    http_client: Arc<dyn HttpClient>,
    agent: &'static str,
    platform: RemotePlatform,
) -> Result<AgentRelease> {
    match agent {
        "codex" => codex_release(http_client, platform).await,
        "claude" => claude_release(http_client, platform).await,
        "pi" => pi_release(http_client, platform).await,
        "agy" => agy_release(http_client, platform).await,
        _ => bail!("unsupported managed agent"),
    }
}

async fn codex_release(
    http_client: Arc<dyn HttpClient>,
    platform: RemotePlatform,
) -> Result<AgentRelease> {
    let metadata = match get_json(
        &http_client,
        "https://releases.openai.com/codex/channels/latest",
    )
    .await
    {
        Ok(metadata) => metadata,
        Err(error) => {
            log::warn!("Codex release channel was unavailable: {error:#}");
            get_json(
                &http_client,
                "https://api.github.com/repos/openai/codex/releases/latest",
            )
            .await?
        }
    };
    let tag = metadata["tag_name"]
        .as_str()
        .context("Codex release has no tag")?;
    let version = tag
        .strip_prefix("rust-v")
        .context("unexpected Codex release tag")?;
    validate_version(version)?;
    let target = match (platform.os, platform.arch) {
        (RemoteOs::Linux, RemoteArch::X86_64) => "x86_64-unknown-linux-musl",
        (RemoteOs::Linux, RemoteArch::Aarch64) => "aarch64-unknown-linux-musl",
        (RemoteOs::MacOs, RemoteArch::X86_64) => "x86_64-apple-darwin",
        (RemoteOs::MacOs, RemoteArch::Aarch64) => "aarch64-apple-darwin",
        (RemoteOs::Windows, RemoteArch::X86_64) => "x86_64-pc-windows-msvc",
        (RemoteOs::Windows, RemoteArch::Aarch64) => "aarch64-pc-windows-msvc",
    };
    let asset_name = format!("codex-package-{target}.tar.gz");
    let asset = metadata["assets"]
        .as_array()
        .context("Codex release has no assets")?
        .iter()
        .find(|asset| asset["name"] == asset_name)
        .context("Codex release has no package for the remote platform")?;
    let source_url = asset["browser_download_url"]
        .as_str()
        .context("Codex asset has no URL")?;
    if !source_url.starts_with("https://releases.openai.com/codex/releases/")
        && !source_url.starts_with("https://github.com/openai/codex/releases/download/")
    {
        bail!("Codex asset URL is outside the official release host");
    }
    let source_sha256 = asset["digest"]
        .as_str()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .context("Codex asset has no SHA-256 digest")?;
    validate_digest(source_sha256)?;
    Ok(AgentRelease {
        agent: "codex",
        version: version.to_string(),
        source_url: source_url.to_string(),
        source_checksum: SourceChecksum::Sha256(source_sha256.to_ascii_lowercase()),
        package_archive: true,
    })
}

async fn claude_release(
    http_client: Arc<dyn HttpClient>,
    platform: RemotePlatform,
) -> Result<AgentRelease> {
    let version = get_text(
        &http_client,
        "https://downloads.claude.ai/claude-code-releases/latest",
    )
    .await?;
    let version = version.trim();
    validate_version(version)?;
    let manifest_url =
        format!("https://downloads.claude.ai/claude-code-releases/{version}/manifest.json");
    let manifest = get_json(&http_client, &manifest_url).await?;
    if manifest["version"] != version {
        bail!("Claude release manifest version does not match");
    }
    let target = match (platform.os, platform.arch) {
        (RemoteOs::Linux, RemoteArch::X86_64) => "linux-x64-musl",
        (RemoteOs::Linux, RemoteArch::Aarch64) => "linux-arm64-musl",
        (RemoteOs::MacOs, RemoteArch::X86_64) => "darwin-x64",
        (RemoteOs::MacOs, RemoteArch::Aarch64) => "darwin-arm64",
        (RemoteOs::Windows, RemoteArch::X86_64) => "win32-x64",
        (RemoteOs::Windows, RemoteArch::Aarch64) => "win32-arm64",
    };
    let asset = &manifest["platforms"][target];
    let binary = asset["binary"]
        .as_str()
        .context("Claude release has no platform binary")?;
    if !matches!(binary, "claude" | "claude.exe") {
        bail!("unexpected Claude release binary name");
    }
    let source_sha256 = asset["checksum"]
        .as_str()
        .context("Claude release has no checksum")?;
    validate_digest(source_sha256)?;
    Ok(AgentRelease {
        agent: "claude",
        version: version.to_string(),
        source_url: format!(
            "https://downloads.claude.ai/claude-code-releases/{version}/{target}/{binary}"
        ),
        source_checksum: SourceChecksum::Sha256(source_sha256.to_ascii_lowercase()),
        package_archive: false,
    })
}

async fn pi_release(
    http_client: Arc<dyn HttpClient>,
    platform: RemotePlatform,
) -> Result<AgentRelease> {
    let metadata = get_json(
        &http_client,
        "https://api.github.com/repos/earendil-works/pi/releases/latest",
    )
    .await?;
    let tag = metadata["tag_name"]
        .as_str()
        .context("Pi release has no tag")?;
    let version = tag.strip_prefix('v').context("unexpected Pi release tag")?;
    validate_version(version)?;
    let target = match (platform.os, platform.arch) {
        (RemoteOs::Linux, RemoteArch::X86_64) => "linux-x64",
        (RemoteOs::Linux, RemoteArch::Aarch64) => "linux-arm64",
        (RemoteOs::MacOs, RemoteArch::X86_64) => "darwin-x64",
        (RemoteOs::MacOs, RemoteArch::Aarch64) => "darwin-arm64",
        _ => bail!("Pi managed remote installation requires a Linux or macOS host"),
    };
    let asset_name = format!("pi-{target}.tar.gz");
    let asset = metadata["assets"]
        .as_array()
        .context("Pi release has no assets")?
        .iter()
        .find(|asset| asset["name"] == asset_name)
        .context("Pi release has no archive for the remote platform")?;
    let source_url = asset["browser_download_url"]
        .as_str()
        .context("Pi asset has no URL")?;
    if !source_url.starts_with("https://github.com/earendil-works/pi/releases/download/") {
        bail!("Pi asset URL is outside the official release host");
    }
    let source_sha256 = asset["digest"]
        .as_str()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .context("Pi asset has no SHA-256 digest")?;
    validate_digest(source_sha256)?;
    Ok(AgentRelease {
        agent: "pi",
        version: version.to_string(),
        source_url: source_url.to_string(),
        source_checksum: SourceChecksum::Sha256(source_sha256.to_ascii_lowercase()),
        package_archive: true,
    })
}

async fn agy_release(
    http_client: Arc<dyn HttpClient>,
    platform: RemotePlatform,
) -> Result<AgentRelease> {
    let target = match (platform.os, platform.arch) {
        (RemoteOs::Linux, RemoteArch::X86_64) => "linux_amd64",
        (RemoteOs::Linux, RemoteArch::Aarch64) => "linux_arm64",
        (RemoteOs::MacOs, RemoteArch::X86_64) => "darwin_amd64",
        (RemoteOs::MacOs, RemoteArch::Aarch64) => "darwin_arm64",
        _ => bail!("Antigravity managed remote installation requires a Linux or macOS host"),
    };
    let manifest_url = format!(
        "https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests/{target}.json"
    );
    let manifest = get_json(&http_client, &manifest_url).await?;
    let version = manifest["version"]
        .as_str()
        .context("Antigravity release has no version")?;
    validate_version(version)?;
    let source_url = manifest["url"]
        .as_str()
        .context("Antigravity release has no URL")?;
    if !source_url.starts_with("https://storage.googleapis.com/antigravity-public/antigravity-cli/")
    {
        bail!("Antigravity asset URL is outside the official release host");
    }
    let source_sha512 = manifest["sha512"]
        .as_str()
        .context("Antigravity release has no SHA-512 digest")?;
    if source_sha512.len() != 128 || !source_sha512.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid Antigravity release checksum");
    }
    Ok(AgentRelease {
        agent: "agy",
        version: version.to_string(),
        source_url: source_url.to_string(),
        source_checksum: SourceChecksum::Sha512(source_sha512.to_ascii_lowercase()),
        package_archive: true,
    })
}

fn validate_version(version: &str) -> Result<()> {
    if version.is_empty()
        || version.len() > 80
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        bail!("invalid agent release version");
    }
    Ok(())
}

fn validate_digest(digest: &str) -> Result<()> {
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid agent release checksum");
    }
    Ok(())
}

async fn get_json(http_client: &Arc<dyn HttpClient>, url: &str) -> Result<serde_json::Value> {
    Ok(serde_json::from_str(&get_text(http_client, url).await?)?)
}

async fn get_text(http_client: &Arc<dyn HttpClient>, url: &str) -> Result<String> {
    let response = http_client.get(url, AsyncBody::empty(), true).await?;
    if !response.status().is_success() {
        bail!("agent release metadata returned HTTP {}", response.status());
    }
    let mut body = response.into_body().take(MAX_METADATA_BYTES + 1);
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > MAX_METADATA_BYTES {
        bail!("agent release metadata is too large");
    }
    Ok(String::from_utf8(bytes)?)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagedAgentProgress {
    Preparing,
    Downloading {
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
    },
    Uploading,
    Installing,
}

pub struct ManagedAgentProgressNotification {
    id: NotificationId,
    state: Rc<Cell<ManagedAgentProgress>>,
    notification: Entity<MessageNotification>,
}

impl ManagedAgentProgressNotification {
    pub fn new(agent: &str, version: &str, cx: &mut App) -> Self {
        let state = Rc::new(Cell::new(ManagedAgentProgress::Preparing));
        let notification = cx.new(|cx| {
            let state = state.clone();
            MessageNotification::new_from_builder(cx, move |_, cx| render_progress(state.get(), cx))
                .with_title(format!("Installing {agent} {version} on the remote device"))
                .show_close_button(false)
                .show_suppress_button(false)
        });
        Self {
            id: NotificationId::composite::<Self>(SharedString::from(format!("{agent}-{version}"))),
            state,
            notification,
        }
    }

    pub fn show(&self, workspace: &mut Workspace, cx: &mut gpui::Context<Workspace>) {
        let notification = self.notification.clone();
        workspace.show_notification(self.id.clone(), cx, |_| notification);
    }

    pub fn dismiss(&self, workspace: &mut Workspace, cx: &mut gpui::Context<Workspace>) {
        workspace.dismiss_notification(&self.id, cx);
    }

    pub fn set_state(&self, state: ManagedAgentProgress, cx: &mut App) {
        self.state.set(state);
        self.notification.update(cx, |_, cx| cx.notify());
    }
}

fn render_progress(state: ManagedAgentProgress, cx: &App) -> gpui::AnyElement {
    let spinner = |label: &'static str| {
        h_flex()
            .gap_2()
            .child(SpinnerLabel::new().size(LabelSize::Small))
            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
            .into_any_element()
    };
    match state {
        ManagedAgentProgress::Preparing => spinner("Preparing download"),
        ManagedAgentProgress::Uploading => spinner("Uploading to the remote device"),
        ManagedAgentProgress::Installing => spinner("Installing and verifying"),
        ManagedAgentProgress::Downloading {
            downloaded_bytes,
            total_bytes: Some(total_bytes),
        } => v_flex()
            .gap_1()
            .child(ProgressBar::new(
                "managed-agent-download-progress",
                downloaded_bytes as f32,
                total_bytes.max(1) as f32,
                cx,
            ))
            .child(
                Label::new(format!(
                    "Downloading: {} / {}",
                    format_bytes(downloaded_bytes),
                    format_bytes(total_bytes)
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .into_any_element(),
        ManagedAgentProgress::Downloading {
            downloaded_bytes,
            total_bytes: None,
        } => h_flex()
            .gap_2()
            .child(SpinnerLabel::new().size(LabelSize::Small))
            .child(
                Label::new(format!("Downloaded {}", format_bytes(downloaded_bytes)))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element(),
    }
}

fn format_bytes(bytes: u64) -> String {
    const KILOBYTE: f64 = 1_000.0;
    const MEGABYTE: f64 = 1_000_000.0;
    const GIGABYTE: f64 = 1_000_000_000.0;

    let bytes = bytes as f64;
    if bytes >= GIGABYTE {
        format!("{:.1} GB", bytes / GIGABYTE)
    } else if bytes >= MEGABYTE {
        format!("{:.1} MB", bytes / MEGABYTE)
    } else if bytes >= KILOBYTE {
        format!("{:.1} KB", bytes / KILOBYTE)
    } else {
        format!("{bytes:.0} B")
    }
}

pub async fn acquire_artifact(
    http_client: Arc<dyn HttpClient>,
    release: &AgentRelease,
    mut report_download_progress: impl FnMut(u64, Option<u64>),
) -> Result<VerifiedAgentArtifact> {
    let cache_directory = paths::data_dir()
        .join("managed_agent_cache")
        .join(release.source_checksum.value());
    smol::fs::create_dir_all(&cache_directory).await?;
    let executable_name = if cfg!(windows) {
        format!("{}.exe", release.agent)
    } else {
        release.agent.to_string()
    };
    let executable_path = cache_directory.join(executable_name);
    let source_path = cache_directory.join("source");
    if !source_path.exists()
        || !file_matches_checksum(&source_path, &release.source_checksum).await?
    {
        let response = http_client
            .get(&release.source_url, AsyncBody::empty(), true)
            .await?;
        if !response.status().is_success() {
            bail!("agent download returned HTTP {}", response.status());
        }
        let total_bytes = response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let partial_path = cache_directory.join(format!("source-{}.partial", uuid::Uuid::new_v4()));
        let result = write_verified_response(
            response.into_body(),
            &partial_path,
            &release.source_checksum,
            total_bytes,
            &mut report_download_progress,
        )
        .await;
        if let Err(error) = result {
            if let Err(cleanup_error) = smol::fs::remove_file(&partial_path).await {
                log::warn!("failed to remove partial agent download: {cleanup_error}");
            }
            return Err(error);
        }
        smol::fs::rename(&partial_path, &source_path).await?;
    }
    if release.package_archive {
        let sha256 = file_sha256(&source_path).await?;
        return Ok(VerifiedAgentArtifact {
            path: source_path,
            sha256,
        });
    }
    let temporary_executable =
        cache_directory.join(format!("executable-{}.partial", uuid::Uuid::new_v4()));
    smol::fs::copy(&source_path, &temporary_executable).await?;
    let sha256 = file_sha256(&temporary_executable).await?;
    if sha256 != file_sha256(&source_path).await? {
        bail!("cached agent executable checksum did not match the release");
    }
    if executable_path.exists() {
        smol::fs::remove_file(&executable_path).await?;
    }
    smol::fs::rename(&temporary_executable, &executable_path).await?;
    Ok(VerifiedAgentArtifact {
        path: executable_path,
        sha256,
    })
}

async fn write_verified_response(
    mut body: AsyncBody,
    destination: &Path,
    expected_checksum: &SourceChecksum,
    total_bytes: Option<u64>,
    report_progress: &mut impl FnMut(u64, Option<u64>),
) -> Result<()> {
    const UNKNOWN_TOTAL_REPORT_INTERVAL: u64 = 1024 * 1024;

    let mut file = smol::fs::File::create(destination).await?;
    let mut last_reported_bucket = None;
    let mut sha256_digest = Sha256::new();
    let mut sha512_digest = Sha512::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut bytes = 0_u64;
    loop {
        let length = body.read(&mut buffer).await?;
        if length == 0 {
            break;
        }
        bytes = bytes
            .checked_add(length as u64)
            .context("agent download size overflowed")?;
        if bytes > MAX_ARTIFACT_BYTES {
            bail!("agent download is too large");
        }
        sha256_digest.update(&buffer[..length]);
        sha512_digest.update(&buffer[..length]);
        file.write_all(&buffer[..length]).await?;
        // Report once per percentage point (or per MiB when the size is unknown) so that
        // the notification does not rerender for every 64 KiB chunk.
        let bucket = match total_bytes {
            Some(0) => 100,
            Some(total_bytes) => (bytes as u128 * 100 / total_bytes as u128).min(100) as u64,
            None => bytes / UNKNOWN_TOTAL_REPORT_INTERVAL,
        };
        if last_reported_bucket != Some(bucket) {
            last_reported_bucket = Some(bucket);
            report_progress(bytes, total_bytes);
        }
    }
    file.flush().await?;
    file.sync_all().await?;
    let sha256 = format!("{:x}", sha256_digest.finalize());
    let sha512 = format!("{:x}", sha512_digest.finalize());
    let matches = match expected_checksum {
        SourceChecksum::Sha256(expected) => &sha256 == expected,
        SourceChecksum::Sha512(expected) => &sha512 == expected,
    };
    if !matches {
        bail!("downloaded agent checksum did not match release metadata");
    }
    Ok(())
}

async fn file_sha256(path: &Path) -> Result<String> {
    let mut file = smol::fs::File::open(path).await?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let length = file.read(&mut buffer).await?;
        if length == 0 {
            break;
        }
        digest.update(&buffer[..length]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

async fn file_matches_checksum(path: &Path, checksum: &SourceChecksum) -> Result<bool> {
    match checksum {
        SourceChecksum::Sha256(expected) => Ok(file_sha256(path).await? == *expected),
        SourceChecksum::Sha512(expected) => {
            let mut file = smol::fs::File::open(path).await?;
            let mut digest = Sha512::new();
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let length = file.read(&mut buffer).await?;
                if length == 0 {
                    break;
                }
                digest.update(&buffer[..length]);
            }
            Ok(format!("{:x}", digest.finalize()) == *expected)
        }
    }
}
