use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub settings: Settings,
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

#[derive(Debug, Deserialize)]
pub struct GroupConfig {
    pub name: String,
    #[serde(default)]
    pub hosts: Vec<HostConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct HostConfig {
    pub name: String,
    pub ip: String,
    /// Name of another host this one sits under. Purely cosmetic: displays
    /// this host indented beneath its parent in the table.
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
