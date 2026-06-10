use std::collections::HashMap;
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
    /// Maximum number of TCP connect attempts in flight at once, across all
    /// hosts and ports. Caps the burst of simultaneous connections so a single
    /// scan doesn't overwhelm a constrained link (e.g. a Citrix/SSL VPN tunnel),
    /// which drops SYNs under load and makes hosts flap between up and down.
    /// Lower it (16-32) on flaky VPNs; raise it on a fast LAN for quicker scans.
    #[serde(default = "default_max_concurrent_probes")]
    pub max_concurrent_probes: usize,
    /// TCP ports probed on every host, for both liveness and role detection.
    /// When unset, a built-in default list is used. Set this to override it —
    /// e.g. to drop port 53 on networks where a DNS firewall accepts TCP/53 for
    /// every address, which otherwise makes shut-down hosts appear online.
    #[serde(default)]
    pub ports: Option<Vec<u16>>,
    /// Tab to open on at startup: a group name or "All". A remembered last tab
    /// (from the state file) takes precedence over this. When unset, opens "All".
    #[serde(default)]
    pub default_tab: Option<String>,
}

fn default_poll() -> u64 {
    30
}
fn default_timeout() -> u64 {
    2000
}
fn default_max_concurrent_probes() -> usize {
    64
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

    /// Check for duplicate host names within a group and duplicate IPs across
    /// the whole config. Returns human-readable warnings; duplicate hosts are
    /// kept and still monitored.
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        // Duplicate names within a group make parent references ambiguous.
        // The same name in different groups is fine.
        for group in &self.group {
            let mut seen_names: HashMap<String, &str> = HashMap::new();
            for host in &group.hosts {
                let key = host.name.trim().to_ascii_lowercase();
                if let Some(first) = seen_names.get(key.as_str()) {
                    warnings.push(format!(
                        "group '{}': duplicate host name '{}' (already defined as '{first}')",
                        group.name, host.name
                    ));
                } else {
                    seen_names.insert(key, &host.name);
                }
            }
        }

        // Duplicate IPs anywhere mean the same machine is probed more than once.
        let mut seen_ips: HashMap<&str, (&str, &str)> = HashMap::new();
        for group in &self.group {
            for host in &group.hosts {
                let ip = host.ip.trim();
                if ip.is_empty() {
                    continue;
                }
                if let Some((first_host, first_group)) = seen_ips.get(ip) {
                    warnings.push(format!(
                        "duplicate IP {ip}: '{}' in group '{}' (already used by '{first_host}' in group '{first_group}')",
                        host.name, group.name
                    ));
                } else {
                    seen_ips.insert(ip, (&host.name, &group.name));
                }
            }
        }

        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_str: &str) -> Config {
        toml::from_str(toml_str).expect("test config should parse")
    }

    #[test]
    fn clean_config_has_no_warnings() {
        let cfg = parse(
            r#"
            [settings]

            [[group]]
            name = "A"
            hosts = [
                { name = "H1", ip = "10.0.0.1" },
                { name = "H2", ip = "10.0.0.2" },
            ]

            [[group]]
            name = "B"
            hosts = [
                { name = "H1", ip = "10.0.0.3" },
            ]
            "#,
        );
        // "H1" appearing in both groups is allowed.
        assert!(cfg.validate().is_empty());
    }

    #[test]
    fn duplicate_name_in_group_warns_case_insensitively() {
        let cfg = parse(
            r#"
            [settings]

            [[group]]
            name = "A"
            hosts = [
                { name = "Host1", ip = "10.0.0.1" },
                { name = "HOST1", ip = "10.0.0.2" },
            ]
            "#,
        );
        let warnings = cfg.validate();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("duplicate host name 'HOST1'"));
        assert!(warnings[0].contains("'Host1'"));
    }

    #[test]
    fn duplicate_ip_warns_across_groups() {
        let cfg = parse(
            r#"
            [settings]

            [[group]]
            name = "A"
            hosts = [{ name = "H1", ip = "10.0.0.1" }]

            [[group]]
            name = "B"
            hosts = [{ name = "H2", ip = "10.0.0.1" }]
            "#,
        );
        let warnings = cfg.validate();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("duplicate IP 10.0.0.1"));
        assert!(warnings[0].contains("'H2'"));
        assert!(warnings[0].contains("'H1'"));
    }
}
