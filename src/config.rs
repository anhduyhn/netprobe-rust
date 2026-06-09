use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub settings: Settings,
    #[serde(default)]
    pub credentials: HashMap<String, CredentialEntry>,
    #[serde(default)]
    pub group: Vec<GroupConfig>,
}

#[derive(Debug, Deserialize)]
pub struct Settings {
    #[serde(default = "default_poll")]
    pub poll_interval: u64,
    #[serde(default = "default_timeout")]
    pub connect_timeout_ms: u64,
}

fn default_poll() -> u64 {
    30
}
fn default_timeout() -> u64 {
    2000
}

#[derive(Debug, Deserialize, Clone)]
pub struct CredentialEntry {
    pub username: String,
    /// Name of the environment variable holding the password.
    #[serde(default)]
    pub password_env: Option<String>,
    /// Plain-text password loaded directly from config.
    #[serde(default)]
    pub password: Option<String>,
    /// Path to a file containing only the password.
    #[serde(default)]
    pub password_file: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ResolvedPassword {
    pub password: String,
    pub source: String,
}

impl CredentialEntry {
    /// Resolve the password from the configured sources.
    pub fn resolve_password(
        &self,
        config_dir: Option<&Path>,
    ) -> color_eyre::Result<Option<ResolvedPassword>> {
        if let Some(env_name) = self.password_env.as_deref() {
            if let Ok(password) = std::env::var(env_name) {
                if !password.is_empty() {
                    return Ok(Some(ResolvedPassword {
                        password,
                        source: format!("${env_name}"),
                    }));
                }
            }
        }

        if let Some(password_file) = &self.password_file {
            let path = if password_file.is_absolute() {
                password_file.clone()
            } else {
                config_dir
                    .map(|dir| dir.join(password_file))
                    .unwrap_or_else(|| password_file.clone())
            };

            let password = std::fs::read_to_string(&path)?;
            let password = password.trim_end_matches(&['\r', '\n'][..]).to_string();
            if !password.is_empty() {
                return Ok(Some(ResolvedPassword {
                    password,
                    source: format!("password_file {}", path.display()),
                }));
            }
        }

        if let Some(password) = self.password.as_deref() {
            if !password.is_empty() {
                return Ok(Some(ResolvedPassword {
                    password: password.to_string(),
                    source: "config password".to_string(),
                }));
            }
        }

        Ok(None)
    }
}

#[derive(Debug, Deserialize)]
pub struct GroupConfig {
    pub name: String,
    /// Key into [credentials] table. Required for Hyper-V VM queries.
    pub credential: Option<String>,
    #[serde(default)]
    pub hosts: Vec<HostConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct HostConfig {
    pub name: String,
    pub ip: String,
    /// Set to "hyperv" to trigger VM inventory query.
    #[serde(default)]
    pub role: Option<String>,
    /// Name of the parent Hyper-V host. Makes this host display as a child VM.
    #[serde(default)]
    pub parent: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> color_eyre::Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|err| {
            color_eyre::eyre::eyre!("failed to read config {}: {err}", path.display())
        })?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }
}
