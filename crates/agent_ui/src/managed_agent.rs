use anyhow::{Context as _, Result, bail};
use async_compression::futures::bufread::GzipDecoder;
use futures::{AsyncReadExt as _, AsyncWriteExt as _, TryStreamExt as _};
use http_client::{AsyncBody, HttpClient};
use remote::{RemoteArch, RemoteOs, RemotePlatform};
use sha2::{Digest as _, Sha256};
use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
};

const MAX_METADATA_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;

pub struct AgentRelease {
    pub agent: &'static str,
    pub version: String,
    pub source_url: String,
    pub source_sha256: String,
    archive_executable: Option<String>,
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
    let executable_name = format!("codex-{target}");
    let asset_name = if platform.os.is_windows() {
        format!("{executable_name}.exe")
    } else {
        format!("{executable_name}.tar.gz")
    };
    let asset = metadata["assets"]
        .as_array()
        .context("Codex release has no assets")?
        .iter()
        .find(|asset| asset["name"] == asset_name)
        .context("Codex release has no asset for the remote platform")?;
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
        source_sha256: source_sha256.to_ascii_lowercase(),
        archive_executable: (!platform.os.is_windows()).then_some(executable_name),
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
        source_sha256: source_sha256.to_ascii_lowercase(),
        archive_executable: None,
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

pub async fn acquire_artifact(
    http_client: Arc<dyn HttpClient>,
    release: &AgentRelease,
) -> Result<VerifiedAgentArtifact> {
    let cache_directory = paths::data_dir()
        .join("managed_agent_cache")
        .join(&release.source_sha256);
    smol::fs::create_dir_all(&cache_directory).await?;
    let executable_name = if cfg!(windows) {
        format!("{}.exe", release.agent)
    } else {
        release.agent.to_string()
    };
    let executable_path = cache_directory.join(executable_name);
    let source_path = cache_directory.join("source");
    if !source_path.exists() || file_sha256(&source_path).await? != release.source_sha256 {
        let response = http_client
            .get(&release.source_url, AsyncBody::empty(), true)
            .await?;
        if !response.status().is_success() {
            bail!("agent download returned HTTP {}", response.status());
        }
        let partial_path = cache_directory.join(format!("source-{}.partial", uuid::Uuid::new_v4()));
        let result =
            write_verified_response(response.into_body(), &partial_path, &release.source_sha256)
                .await;
        if let Err(error) = result {
            if let Err(cleanup_error) = smol::fs::remove_file(&partial_path).await {
                log::warn!("failed to remove partial agent download: {cleanup_error}");
            }
            return Err(error);
        }
        smol::fs::rename(&partial_path, &source_path).await?;
    }
    let temporary_executable =
        cache_directory.join(format!("executable-{}.partial", uuid::Uuid::new_v4()));
    if let Some(archive_executable) = &release.archive_executable {
        extract_tar_executable(&source_path, &temporary_executable, archive_executable).await?;
    } else {
        smol::fs::copy(&source_path, &temporary_executable).await?;
    }
    let sha256 = file_sha256(&temporary_executable).await?;
    if release.archive_executable.is_none() && sha256 != release.source_sha256 {
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
    expected_sha256: &str,
) -> Result<()> {
    let mut file = smol::fs::File::create(destination).await?;
    let mut digest = Sha256::new();
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
        digest.update(&buffer[..length]);
        file.write_all(&buffer[..length]).await?;
    }
    file.flush().await?;
    file.sync_all().await?;
    if format!("{:x}", digest.finalize()) != expected_sha256 {
        bail!("downloaded agent checksum did not match release metadata");
    }
    Ok(())
}

async fn extract_tar_executable(
    source: &Path,
    destination: &Path,
    executable_name: &str,
) -> Result<()> {
    let source = smol::fs::File::open(source).await?;
    let decompressed = GzipDecoder::new(futures::io::BufReader::new(source));
    let mut entries = async_tar::Archive::new(decompressed).entries()?;
    let mut found = false;
    while let Some(entry) = entries.try_next().await? {
        let path = entry.path()?.into_owned();
        if path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            bail!("agent archive contains an unsafe path");
        }
        if path.to_string_lossy() == executable_name {
            if found || !entry.header().entry_type().is_file() {
                bail!("agent archive has an invalid executable entry");
            }
            let mut output = smol::fs::File::create(destination).await?;
            let mut bounded_entry = entry.take(MAX_ARTIFACT_BYTES + 1);
            if futures::io::copy(&mut bounded_entry, &mut output).await? > MAX_ARTIFACT_BYTES {
                bail!("agent executable is too large");
            }
            output.flush().await?;
            output.sync_all().await?;
            found = true;
        }
    }
    if !found {
        bail!("agent archive does not contain its executable");
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
