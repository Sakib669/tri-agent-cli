use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::providers::ProviderId;

pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("triagent")
}

fn credentials_path() -> PathBuf {
    data_dir().join("credentials.json")
}

pub fn sessions_dir() -> PathBuf {
    data_dir().join("sessions")
}

/// Token payloads per provider. Stored together in one 0600 file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeCreds {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: u64,
    pub refresh_token_expires_at_ms: Option<u64>,
    pub scopes: Vec<String>,
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatGptCreds {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: Option<String>,
    pub last_refresh: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiCreds {
    pub access_token: String,
    pub token_type: String,
    pub refresh_token: String,
    pub expiry: Option<String>,
    pub id_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", content = "creds")]
pub enum ProviderCreds {
    #[serde(rename = "claude")]
    Claude(ClaudeCreds),
    #[serde(rename = "chatgpt")]
    ChatGpt(ChatGptCreds),
    #[serde(rename = "gemini")]
    Gemini(GeminiCreds),
}

impl ProviderCreds {
    pub fn provider(&self) -> ProviderId {
        match self {
            ProviderCreds::Claude(_) => ProviderId::Claude,
            ProviderCreds::ChatGpt(_) => ProviderId::ChatGpt,
            ProviderCreds::Gemini(_) => ProviderId::Gemini,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CredentialsFile {
    pub providers: BTreeMap<String, ProviderCreds>,
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// Open (create dirs) the data store at the standard XDG location.
    pub fn open() -> Result<Self> {
        Self::at(data_dir())
    }

    /// Open a store rooted at an explicit directory (used by tests).
    pub fn at(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(dir.join("sessions")).context("creating sessions dir")?;
        ensure_secure_perms_at(&dir.join("credentials.json"))?;
        Ok(Self { dir })
    }

    fn credentials_file(&self) -> PathBuf {
        self.dir.join("credentials.json")
    }

    fn read_file(&self) -> Result<CredentialsFile> {
        let path = self.credentials_file();
        match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).context("parsing credentials.json"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CredentialsFile::default()),
            Err(e) => Err(e).context("reading credentials.json"),
        }
    }

    /// Write atomically with mode 0600.
    fn write_file(&self, file: &CredentialsFile) -> Result<()> {
        let path = self.credentials_file();
        let tmp = path.with_extension("json.tmp");
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(serde_json::to_string_pretty(file)?.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn get(&self, provider: ProviderId) -> Result<Option<ProviderCreds>> {
        Ok(self.read_file()?.providers.get(provider.as_str()).cloned())
    }

    pub fn put(&self, creds: ProviderCreds) -> Result<()> {
        let mut file = self.read_file()?;
        file.providers
            .insert(creds.provider().as_str().into(), creds);
        self.write_file(&file)
    }

    pub fn remove(&self, provider: ProviderId) -> Result<Option<ProviderCreds>> {
        let mut file = self.read_file()?;
        let old = file.providers.remove(provider.as_str());
        self.write_file(&file)?;
        Ok(old)
    }

    pub fn all(&self) -> Result<BTreeMap<String, ProviderCreds>> {
        Ok(self.read_file()?.providers)
    }

    pub fn credentials_path() -> PathBuf {
        credentials_path()
    }
}

fn ensure_secure_perms_at(path: &std::path::Path) -> Result<()> {
    if let Ok(md) = fs::metadata(path) {
        let mode = md.permissions().mode() & 0o777;
        if mode != 0o600 {
            return Err(anyhow!(
                "{} has insecure permissions {:o}; expected 600",
                path.display(),
                mode
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_roundtrip_and_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::at(tmp.path().to_path_buf()).unwrap();

        let creds = ProviderCreds::Claude(ClaudeCreds {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at_ms: 123456,
            refresh_token_expires_at_ms: None,
            scopes: vec!["user:inference".into()],
            subscription_type: Some("pro".into()),
            rate_limit_tier: None,
        });
        store.put(creds.clone()).unwrap();

        let file = tmp.path().join("credentials.json");
        let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "credentials file must be 0600");

        let got = store.get(ProviderId::Claude).unwrap().unwrap();
        assert_eq!(format!("{:?}", got), format!("{:?}", creds));

        store.remove(ProviderId::Claude).unwrap();
        assert!(store.get(ProviderId::Claude).unwrap().is_none());
    }

    #[test]
    fn insecure_perms_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("credentials.json");
        std::fs::write(&file, "{}").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = Store::at(tmp.path().to_path_buf());
        assert!(err.is_err(), "0644 credentials file must be rejected");
    }
}
