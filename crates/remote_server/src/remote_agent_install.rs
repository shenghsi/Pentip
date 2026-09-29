use anyhow::{Context as _, Result, bail};
use async_compression::futures::bufread::GzipDecoder;
use futures::{AsyncReadExt as _, TryStreamExt as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    fs,
    io::{Read as _, Write as _},
    path::{Component, Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
struct InstallationReceipt {
    version: String,
    sha256: String,
    #[serde(default)]
    executable_sha256: Option<String>,
    #[serde(default)]
    code_mode_host_sha256: Option<String>,
}

fn agent_directory(agent: &str) -> Result<PathBuf> {
    if !matches!(agent, "codex" | "claude" | "pi" | "agy") {
        bail!("unsupported managed agent");
    }
    Ok(paths::data_dir().join("managed_agents").join(agent))
}

fn validate_version(version: &str) -> Result<()> {
    if version.is_empty()
        || version.len() > 80
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        bail!("invalid managed agent version");
    }
    Ok(())
}

fn executable_name(agent: &str) -> Result<&'static str> {
    match (agent, cfg!(windows)) {
        ("codex", true) => Ok("codex.exe"),
        ("claude", true) => Ok("claude.exe"),
        ("codex", false) => Ok("codex"),
        ("claude", false) => Ok("claude"),
        ("pi", false) => Ok("pi"),
        ("agy", false) => Ok("agy"),
        _ => bail!("unsupported managed agent"),
    }
}

fn archive_executable_path(agent: &str) -> Result<&'static str> {
    match (agent, cfg!(windows)) {
        ("codex", true) => Ok("bin/codex.exe"),
        ("codex", false) => Ok("bin/codex"),
        ("pi", false) => Ok("pi/pi"),
        ("agy", false) => Ok("antigravity"),
        _ => bail!("unsupported managed agent archive"),
    }
}

fn codex_host_path() -> &'static str {
    if cfg!(windows) {
        "bin/codex-code-mode-host.exe"
    } else {
        "bin/codex-code-mode-host"
    }
}

fn archive_upload_name(agent: &str) -> Result<&'static str> {
    match agent {
        "codex" => Ok("codex-package.tar.gz"),
        "pi" => Ok("pi-package.tar.gz"),
        "agy" => Ok("agy-package.tar.gz"),
        _ => bail!("unsupported managed agent archive"),
    }
}

fn file_sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        digest.update(&buffer[..length]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub fn current(agent: &str) -> Result<proto::GetManagedAgentInstallationResponse> {
    let directory = agent_directory(agent)?;
    current_in_directory(agent, &directory)
}

fn current_in_directory(
    agent: &str,
    directory: &Path,
) -> Result<proto::GetManagedAgentInstallationResponse> {
    let receipt = match fs::read(directory.join("current.json")) {
        Ok(content) => serde_json::from_slice::<InstallationReceipt>(&content)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(proto::GetManagedAgentInstallationResponse::default());
        }
        Err(error) => return Err(error.into()),
    };
    validate_version(&receipt.version)?;
    let installed_directory = directory.join(format!("{}-{}", receipt.version, receipt.sha256));
    let executable = if matches!(agent, "codex" | "pi" | "agy") {
        installed_directory.join(archive_executable_path(agent)?)
    } else {
        installed_directory.join(executable_name(agent)?)
    };
    let executable_sha256 = if matches!(agent, "codex" | "pi" | "agy") {
        match receipt.executable_sha256.as_deref() {
            Some(sha256) => sha256,
            None => return Ok(proto::GetManagedAgentInstallationResponse::default()),
        }
    } else {
        &receipt.sha256
    };
    if !executable.is_file() || file_sha256(&executable)? != executable_sha256 {
        return Ok(proto::GetManagedAgentInstallationResponse::default());
    }
    if agent == "codex" {
        let host_sha256 = match receipt.code_mode_host_sha256.as_deref() {
            Some(sha256) => sha256,
            None => return Ok(proto::GetManagedAgentInstallationResponse::default()),
        };
        let host = installed_directory.join(codex_host_path());
        if !host.is_file() || file_sha256(&host)? != host_sha256 {
            return Ok(proto::GetManagedAgentInstallationResponse::default());
        }
    }
    Ok(proto::GetManagedAgentInstallationResponse {
        version: receipt.version,
        executable_path: executable.to_string_lossy().into_owned(),
    })
}

pub fn stage(agent: &str, version: &str) -> Result<proto::StageManagedAgentInstallationResponse> {
    validate_version(version)?;
    let directory = agent_directory(agent)?;
    stage_in_directory(agent, &directory)
}

fn stage_in_directory(
    agent: &str,
    directory: &Path,
) -> Result<proto::StageManagedAgentInstallationResponse> {
    fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    let stage_id = uuid::Uuid::new_v4().to_string();
    let stage_directory = directory.join(format!(".stage-{stage_id}"));
    fs::create_dir(&stage_directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&stage_directory, fs::Permissions::from_mode(0o700))?;
    }
    Ok(proto::StageManagedAgentInstallationResponse {
        stage_id,
        upload_path: stage_directory
            .join(if matches!(agent, "codex" | "pi" | "agy") {
                archive_upload_name(agent)?
            } else {
                executable_name(agent)?
            })
            .to_string_lossy()
            .into_owned(),
    })
}

pub fn commit(
    agent: &str,
    version: &str,
    stage_id: &str,
    expected_sha256: &str,
) -> Result<proto::CommitManagedAgentInstallationResponse> {
    let directory = agent_directory(agent)?;
    commit_in_directory(agent, version, stage_id, expected_sha256, &directory)
}

fn commit_in_directory(
    agent: &str,
    version: &str,
    stage_id: &str,
    expected_sha256: &str,
    directory: &Path,
) -> Result<proto::CommitManagedAgentInstallationResponse> {
    validate_version(version)?;
    uuid::Uuid::parse_str(stage_id).context("invalid managed agent stage ID")?;
    if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("invalid managed agent checksum");
    }
    let staged_directory = directory.join(format!(".stage-{stage_id}"));
    if matches!(agent, "codex" | "pi" | "agy") {
        return commit_agent_archive(
            agent,
            version,
            expected_sha256,
            directory,
            &staged_directory,
        );
    }
    let staged_executable = staged_directory.join(executable_name(agent)?);
    if file_sha256(&staged_executable)? != expected_sha256.to_ascii_lowercase() {
        bail!("uploaded managed agent checksum did not match");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&staged_executable, fs::Permissions::from_mode(0o700))?;
    }
    let installed_directory = directory.join(format!(
        "{version}-{}",
        expected_sha256.to_ascii_lowercase()
    ));
    if installed_directory.exists() {
        let installed_executable = installed_directory.join(executable_name(agent)?);
        if file_sha256(&installed_executable)? != expected_sha256.to_ascii_lowercase() {
            bail!("existing managed agent installation has a different checksum");
        }
        fs::remove_dir_all(&staged_directory)?;
    } else {
        fs::rename(&staged_directory, &installed_directory)?;
    }
    let receipt = InstallationReceipt {
        version: version.to_string(),
        sha256: expected_sha256.to_ascii_lowercase(),
        executable_sha256: None,
        code_mode_host_sha256: None,
    };
    let receipt_path = directory.join(format!(".current-{stage_id}.json"));
    fs::write(&receipt_path, serde_json::to_vec(&receipt)?)?;
    fs::rename(receipt_path, directory.join("current.json"))?;
    Ok(proto::CommitManagedAgentInstallationResponse {
        executable_path: installed_directory
            .join(executable_name(agent)?)
            .to_string_lossy()
            .into_owned(),
    })
}

fn commit_agent_archive(
    agent: &str,
    version: &str,
    expected_sha256: &str,
    directory: &Path,
    staged_directory: &Path,
) -> Result<proto::CommitManagedAgentInstallationResponse> {
    let archive = staged_directory.join(archive_upload_name(agent)?);
    let archive_sha256 = expected_sha256.to_ascii_lowercase();
    if file_sha256(&archive)? != archive_sha256 {
        bail!("uploaded managed agent checksum did not match");
    }
    let staged_package = staged_directory.join("package");
    fs::create_dir(&staged_package)?;
    extract_agent_archive(&archive, &staged_package)?;
    let staged_executable = staged_package.join(archive_executable_path(agent)?);
    let executable_sha256 =
        file_sha256(&staged_executable).context("agent package does not contain its executable")?;
    let staged_host = staged_package.join(codex_host_path());
    let code_mode_host_sha256 = if agent == "codex" {
        Some(
            file_sha256(&staged_host)
                .context("Codex package does not contain its code-mode host")?,
        )
    } else {
        None
    };
    #[cfg(unix)]
    for executable in [
        &staged_executable,
        &staged_host,
        &staged_package.join("pi/pi"),
        &staged_package.join("antigravity"),
        &staged_package.join("codex-path/rg"),
        &staged_package.join("codex-resources/bwrap"),
        &staged_package.join("codex-resources/zsh/bin/zsh"),
    ] {
        if executable.is_file() {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(executable, fs::Permissions::from_mode(0o700))?;
        }
    }
    let installed_directory = directory.join(format!("{version}-{archive_sha256}"));
    if installed_directory.exists() {
        let old_directory = directory.join(format!(".old-{}", uuid::Uuid::new_v4()));
        fs::rename(&installed_directory, &old_directory)?;
        if let Err(error) = fs::rename(&staged_package, &installed_directory) {
            fs::rename(&old_directory, &installed_directory)
                .context("could not restore the previous managed agent installation")?;
            return Err(error.into());
        }
        if let Err(error) = fs::remove_dir_all(&old_directory) {
            log::warn!("could not remove previous {agent} installation: {error}");
        }
    } else {
        fs::rename(&staged_package, &installed_directory)?;
    }
    fs::remove_dir_all(staged_directory)?;
    let receipt = InstallationReceipt {
        version: version.to_string(),
        sha256: archive_sha256,
        executable_sha256: Some(executable_sha256),
        code_mode_host_sha256,
    };
    let receipt_path = directory.join(format!(".current-{}.json", uuid::Uuid::new_v4()));
    fs::write(&receipt_path, serde_json::to_vec(&receipt)?)?;
    fs::rename(receipt_path, directory.join("current.json"))?;
    Ok(proto::CommitManagedAgentInstallationResponse {
        executable_path: installed_directory
            .join(archive_executable_path(agent)?)
            .to_string_lossy()
            .into_owned(),
    })
}

fn extract_agent_archive(archive_path: &Path, destination: &Path) -> Result<()> {
    const MAX_PACKAGE_BYTES: u64 = 1024 * 1024 * 1024;

    let archive_file = fs::File::open(archive_path)?;
    let archive_file = futures::io::AllowStdIo::new(archive_file);
    let decompressed = GzipDecoder::new(futures::io::BufReader::new(archive_file));
    let mut entries = async_tar::Archive::new(decompressed).entries()?;
    smol::block_on(async {
        let mut total_bytes = 0_u64;
        while let Some(mut entry) = entries.try_next().await? {
            let path = entry.path()?.into_owned();
            if path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            {
                bail!("managed agent package contains an unsafe path");
            }
            let output_path = destination.join(path);
            if entry.header().entry_type().is_dir() {
                fs::create_dir_all(&output_path)?;
            } else if entry.header().entry_type().is_file() {
                let parent = output_path
                    .parent()
                    .context("managed agent package file has no parent")?;
                fs::create_dir_all(parent)?;
                let mut output = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&output_path)?;
                let mut buffer = [0_u8; 128 * 1024];
                loop {
                    let length = entry.read(&mut buffer).await?;
                    if length == 0 {
                        break;
                    }
                    total_bytes = total_bytes
                        .checked_add(length as u64)
                        .context("managed agent package size overflowed")?;
                    if total_bytes > MAX_PACKAGE_BYTES {
                        bail!("managed agent package is too large");
                    }
                    output.write_all(&buffer[..length])?;
                }
                output.sync_all()?;
            } else {
                bail!("managed agent package contains an unsupported entry");
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_upload_does_not_replace_the_installed_agent() -> Result<()> {
        let directory = std::env::temp_dir().join(format!(
            "pentip-agent-install-test-{}",
            uuid::Uuid::new_v4()
        ));
        let first = stage_in_directory("claude", &directory)?;
        fs::write(&first.upload_path, b"first agent")?;
        let first_sha256 = file_sha256(Path::new(&first.upload_path))?;
        commit_in_directory(
            "claude",
            "1.0.0",
            &first.stage_id,
            &first_sha256,
            &directory,
        )?;

        let second = stage_in_directory("claude", &directory)?;
        fs::write(&second.upload_path, b"damaged upload")?;
        assert!(
            commit_in_directory(
                "claude",
                "2.0.0",
                &second.stage_id,
                &first_sha256,
                &directory
            )
            .is_err()
        );

        let installed = current_in_directory("claude", &directory)?;
        assert_eq!(installed.version, "1.0.0");
        assert_eq!(fs::read(installed.executable_path)?, b"first agent");
        fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn codex_package_requires_its_code_mode_host() -> Result<()> {
        let directory = std::env::temp_dir().join(format!(
            "pentip-codex-package-test-{}",
            uuid::Uuid::new_v4()
        ));
        let stage = stage_in_directory("codex", &directory)?;
        let archive = smol::block_on(async {
            let buffer = futures::io::Cursor::new(Vec::new());
            let mut builder = async_tar::Builder::new(buffer);
            for (name, content) in [
                (archive_executable_path("codex")?, b"codex".as_slice()),
                (codex_host_path(), b"host".as_slice()),
            ] {
                let mut header = async_tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder.append_data(&mut header, name, content).await?;
            }
            let buffer = builder.into_inner().await?;
            anyhow::Ok(buffer.into_inner())
        })?;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&archive)?;
        let archive_bytes = encoder.finish()?;
        fs::write(&stage.upload_path, &archive_bytes)?;
        let archive_sha256 = file_sha256(Path::new(&stage.upload_path))?;
        let installed = commit_in_directory(
            "codex",
            "1.0.0",
            &stage.stage_id,
            &archive_sha256,
            &directory,
        )?;
        assert_eq!(fs::read(&installed.executable_path)?, b"codex");
        let host = Path::new(&installed.executable_path).with_file_name(if cfg!(windows) {
            "codex-code-mode-host.exe"
        } else {
            "codex-code-mode-host"
        });
        assert_eq!(fs::read(&host)?, b"host");
        assert!(
            !current_in_directory("codex", &directory)?
                .executable_path
                .is_empty()
        );
        fs::remove_file(&host)?;
        assert!(
            current_in_directory("codex", &directory)?
                .executable_path
                .is_empty()
        );
        let repair_stage = stage_in_directory("codex", &directory)?;
        fs::write(&repair_stage.upload_path, &archive_bytes)?;
        commit_in_directory(
            "codex",
            "1.0.0",
            &repair_stage.stage_id,
            &archive_sha256,
            &directory,
        )?;
        assert_eq!(fs::read(host)?, b"host");
        assert!(
            !current_in_directory("codex", &directory)?
                .executable_path
                .is_empty()
        );
        fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[cfg(not(windows))]
    #[test]
    fn pi_and_agy_archives_install_their_executables() -> Result<()> {
        for (agent, executable_path) in [("pi", "pi/pi"), ("agy", "antigravity")] {
            let directory = std::env::temp_dir().join(format!(
                "pentip-{agent}-package-test-{}",
                uuid::Uuid::new_v4()
            ));
            let stage = stage_in_directory(agent, &directory)?;
            let archive = smol::block_on(async {
                let buffer = futures::io::Cursor::new(Vec::new());
                let mut builder = async_tar::Builder::new(buffer);
                let mut header = async_tar::Header::new_gnu();
                header.set_size(agent.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder
                    .append_data(&mut header, executable_path, agent.as_bytes())
                    .await?;
                let buffer = builder.into_inner().await?;
                anyhow::Ok(buffer.into_inner())
            })?;
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&archive)?;
            fs::write(&stage.upload_path, encoder.finish()?)?;
            let archive_sha256 = file_sha256(Path::new(&stage.upload_path))?;
            let installed =
                commit_in_directory(agent, "1.0.0", &stage.stage_id, &archive_sha256, &directory)?;
            assert_eq!(fs::read(&installed.executable_path)?, agent.as_bytes());
            assert_eq!(
                current_in_directory(agent, &directory)?.executable_path,
                installed.executable_path
            );
            fs::remove_dir_all(directory)?;
        }
        Ok(())
    }
}
