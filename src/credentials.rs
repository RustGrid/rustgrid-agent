use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use keyring::Entry;
use sha2::{Digest, Sha256};

const KEYRING_SERVICE: &str = "com.rustgrid.agent";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialSource {
    Keychain,
    FallbackFile,
    Environment,
    Missing,
}

impl CredentialSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Keychain => "os_keychain",
            Self::FallbackFile => "private_file_fallback",
            Self::Environment => "environment",
            Self::Missing => "missing",
        }
    }
}

#[derive(Clone, Debug)]
pub struct CredentialStore {
    account: String,
    fallback_path: PathBuf,
    force_fallback: bool,
}

impl CredentialStore {
    pub fn new(instance_api_url: &str, installation_id: &str) -> Result<Self> {
        let account = account_name(instance_api_url, installation_id);
        let fallback_path = credentials_directory()?.join(format!("{account}.token"));
        Ok(Self {
            account,
            fallback_path,
            force_fallback: env::var("RUSTGRID_CREDENTIAL_STORE")
                .is_ok_and(|value| value.eq_ignore_ascii_case("file")),
        })
    }

    pub fn load(&self) -> Result<(Option<String>, CredentialSource)> {
        if !self.force_fallback {
            match self
                .entry()
                .and_then(|entry| entry.get_password().map_err(Into::into))
            {
                Ok(secret) if !secret.trim().is_empty() => {
                    return Ok((Some(secret), CredentialSource::Keychain));
                }
                Ok(_) => bail!("the OS keychain returned an empty RustGrid credential"),
                Err(error) => {
                    if self.fallback_path.exists() {
                        eprintln!(
                            "[warning] OS keychain is unavailable ({error}); using the private-file credential fallback"
                        );
                    }
                }
            }
        }

        match read_private_file(&self.fallback_path)? {
            Some(secret) if !secret.trim().is_empty() => {
                Ok((Some(secret), CredentialSource::FallbackFile))
            }
            Some(_) => bail!("empty credential in {}", self.fallback_path.display()),
            None => Ok((None, CredentialSource::Missing)),
        }
    }

    pub fn save(&self, secret: &str) -> Result<CredentialSource> {
        if secret.trim().is_empty() {
            bail!("refusing to store an empty RustGrid credential");
        }
        if !self.force_fallback {
            match self
                .entry()
                .and_then(|entry| entry.set_password(secret).map_err(Into::into))
            {
                Ok(()) => {
                    remove_fallback_if_present(&self.fallback_path)?;
                    return Ok(CredentialSource::Keychain);
                }
                Err(error) => eprintln!(
                    "[warning] OS keychain is unavailable ({error}); storing the credential in a private owner-only file"
                ),
            }
        }

        write_private_file_atomic(&self.fallback_path, secret.as_bytes())?;
        Ok(CredentialSource::FallbackFile)
    }

    pub fn delete(&self) -> Result<()> {
        if !self.force_fallback {
            let fallback_exists = self.fallback_path.exists();
            match self.entry()?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(error) if fallback_exists => eprintln!(
                    "[warning] OS keychain cleanup failed ({error}); removing the active private-file fallback"
                ),
                Err(error) => {
                    return Err(error)
                        .context("could not remove the RustGrid credential from the OS keychain");
                }
            }
        }
        remove_fallback_if_present(&self.fallback_path)
    }

    pub fn fallback_path(&self) -> &Path {
        &self.fallback_path
    }

    fn entry(&self) -> Result<Entry> {
        Entry::new(KEYRING_SERVICE, &self.account).context("could not initialize the OS keychain")
    }
}

fn account_name(instance_api_url: &str, installation_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(instance_api_url.trim_end_matches('/').as_bytes());
    hasher.update(b"\0");
    hasher.update(installation_id.as_bytes());
    format!("worker-{}", hex::encode(hasher.finalize()))
}

fn credentials_directory() -> Result<PathBuf> {
    if let Some(path) = env::var_os("RUSTGRID_CREDENTIALS_DIR") {
        return Ok(PathBuf::from(path));
    }
    #[cfg(target_os = "windows")]
    if let Some(path) = env::var_os("APPDATA") {
        return Ok(PathBuf::from(path)
            .join("RustGrid")
            .join("Agent")
            .join("credentials"));
    }
    #[cfg(target_os = "macos")]
    if let Some(path) = env::var_os("HOME") {
        return Ok(PathBuf::from(path)
            .join("Library")
            .join("Application Support")
            .join("RustGrid Agent")
            .join("credentials"));
    }
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path)
            .join("rustgrid-agent")
            .join("credentials"));
    }
    if let Some(path) = env::var_os("HOME") {
        return Ok(PathBuf::from(path)
            .join(".config")
            .join("rustgrid-agent")
            .join("credentials"));
    }
    bail!("cannot locate a user credential directory; set RUSTGRID_CREDENTIALS_DIR")
}

fn read_private_file(path: &Path) -> Result<Option<String>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("could not inspect {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "credential fallback path is not a regular file: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!(
                "credential fallback permissions are too broad: {}",
                path.display()
            );
        }
    }
    fs::read_to_string(path)
        .map(Some)
        .with_context(|| format!("could not read {}", path.display()))
}

fn write_private_file_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().context("credential path has no parent")?;
    create_private_directory(parent)?;
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        bail!(
            "refusing to replace non-regular credential path {}",
            path.display()
        );
    }
    let temporary = parent.join(format!(".credential-{}.tmp", uuid::Uuid::new_v4()));
    write_private_file(&temporary, contents)?;
    fs::rename(&temporary, path)
        .with_context(|| format!("could not save credential to {}", path.display()))?;
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("could not create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

fn remove_fallback_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("could not delete {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_names_do_not_expose_instance_or_installation() {
        let account = account_name(
            "https://app.rustgrid.com/api/v1",
            "00000000-0000-4000-8000-000000000001",
        );
        assert!(account.starts_with("worker-"));
        assert!(!account.contains("rustgrid.com"));
        assert!(!account.contains("00000000"));
    }
}
