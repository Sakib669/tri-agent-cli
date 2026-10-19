use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::providers::ProviderId;

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("triagent")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub defaults: Defaults,
    pub providers: BTreeMap<String, ProviderCfg>,
    pub permissions: PermissionsCfg,
    pub ui: UiCfg,
    pub skills: SkillsCfg,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillsCfg {
    pub dirs: Vec<String>,
}

impl SkillsCfg {
    /// Expanded skill roots. Always includes ~/.config/triagent/skills and,
    /// for repo checkouts / `cargo build` runs, any `skills/` folder next to
    /// the binary — so skills work without running the install script.
    pub fn expanded(&self) -> Vec<std::path::PathBuf> {
        let mut v: Vec<std::path::PathBuf> = self
            .dirs
            .iter()
            .map(|d| {
                if let Some(rest) = d.strip_prefix("~/") {
                    dirs::home_dir()
                        .map(|h| h.join(rest))
                        .unwrap_or_else(|| std::path::PathBuf::from(d))
                } else {
                    std::path::PathBuf::from(d)
                }
            })
            .collect();
        v.push(config_dir().join("skills"));
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                for rel in ["skills", "../skills", "../../skills"] {
                    let p = dir.join(rel);
                    if p.is_dir() {
                        v.push(p);
                    }
                }
            }
        }
        v.sort();
        v.dedup();
        v
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiCfg {
    pub theme: String,
}

impl Default for UiCfg {
    fn default() -> Self {
        Self {
            theme: "claude".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Defaults {
    pub provider: ProviderId,
    pub model: Option<String>,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            provider: ProviderId::Claude,
            model: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderCfg {
    pub default_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionsCfg {
    pub default_mode: String, // "default" | "accept-edits" | "bypass"
}

impl Default for PermissionsCfg {
    fn default() -> Self {
        Self {
            default_mode: "default".into(),
        }
    }
}

impl Config {
    /// Load config from disk, or return defaults (and write a starter file).
    pub fn load_or_default() -> Result<Self> {
        let path = config_path();
        if let Ok(raw) = fs::read_to_string(&path) {
            if let Ok(cfg) = toml::from_str::<Config>(&raw) {
                return Ok(cfg);
            }
            // Fall through to defaults on parse error rather than dying.
        }
        let cfg = Config::default();
        let _ = fs::create_dir_all(config_dir());
        let _ = fs::write(&path, toml::to_string_pretty(&cfg)?);
        Ok(cfg)
    }

    pub fn default_provider(&self) -> ProviderId {
        self.defaults.provider
    }

    pub fn default_model(&self, provider: ProviderId) -> Option<String> {
        self.defaults
            .model
            .clone()
            .or_else(|| {
                self.providers
                    .get(provider.as_str())
                    .and_then(|p| p.default_model.clone())
            })
            .or_else(|| provider.builtin_default_model().map(str::to_string))
    }
}
