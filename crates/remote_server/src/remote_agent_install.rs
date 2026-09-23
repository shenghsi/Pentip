use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    fs,
    io::Read as _,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
struct InstallationReceipt {
    version: String,
    sha256: String,
}

fn agent_directory(agent: &str) -> Result<PathBuf> {
    if !matches!(agent, "codex" | "claude") {
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
        _ => bail!("unsupported managed agent"),
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
    let executable = directory
        .join(format!("{}-{}", receipt.version, receipt.sha256))
        .join(executable_name(agent)?);
    if !executable.is_file() || file_sha256(&executable)? != receipt.sha256 {
        return Ok(proto::GetManagedAgentInstallationResponse::default());
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
            .join(executable_name(agent)?)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_upload_does_not_replace_the_installed_agent() -> Result<()> {
        let directory = std::env::temp_dir().join(format!(
            "pentip-agent-install-test-{}",
            uuid::Uuid::new_v4()
        ));
        let first = stage_in_directory("codex", &directory)?;
        fs::write(&first.upload_path, b"first agent")?;
        let first_sha256 = file_sha256(Path::new(&first.upload_path))?;
        commit_in_directory("codex", "1.0.0", &first.stage_id, &first_sha256, &directory)?;

        let second = stage_in_directory("codex", &directory)?;
        fs::write(&second.upload_path, b"damaged upload")?;
        assert!(
            commit_in_directory(
                "codex",
                "2.0.0",
                &second.stage_id,
                &first_sha256,
                &directory
            )
            .is_err()
        );

        let installed = current_in_directory("codex", &directory)?;
        assert_eq!(installed.version, "1.0.0");
        assert_eq!(fs::read(installed.executable_path)?, b"first agent");
        fs::remove_dir_all(directory)?;
        Ok(())
    }
}
