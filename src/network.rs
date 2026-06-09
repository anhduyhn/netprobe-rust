use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::time::timeout;

use crate::app::{Host, HostStatus};

/// Default ports to probe for liveness and role detection. Overridable via the
/// `ports` setting in config.toml (e.g. to drop 53 on networks where a DNS
/// firewall accepts TCP/53 for every address and makes dead hosts look up).
const DEFAULT_PROBE_PORTS: &[u16] = &[
    53,    // DNS
    80,    // HTTP
    88,    // Kerberos
    135,   // RPC
    389,   // LDAP
    443,   // HTTPS
    445,   // SMB
    636,   // LDAPS
    2179,  // Hyper-V vmconnect
    3389,  // RDP
    4660,  // CommBox
    5985,  // WinRM HTTP
    5986,  // WinRM HTTPS
    7001,  // NX Witness
    9100,  // Raw printing
    10123, // SCCM
];

/// The built-in probe port list, used when config.toml does not set `ports`.
pub fn default_ports() -> Vec<u16> {
    DEFAULT_PROBE_PORTS.to_vec()
}

/// Try to connect to one port. Returns the connect duration if it succeeded.
async fn probe_port(ip: &str, port: u16, connect_timeout: Duration) -> Option<Duration> {
    let addr: SocketAddr = format!("{ip}:{port}").parse().ok()?;
    let start = Instant::now();
    match timeout(connect_timeout, TcpStream::connect(addr)).await {
        Ok(Ok(_stream)) => Some(start.elapsed()),
        _ => None,
    }
}

pub struct ProbeResult {
    pub open_ports: Vec<u16>,
    /// Fastest successful connect, or None if nothing answered.
    pub latency_ms: Option<u64>,
}

pub async fn probe_host(
    ip: &str,
    connect_timeout: Duration,
    limiter: Arc<Semaphore>,
    ports: Arc<Vec<u16>>,
) -> ProbeResult {
    let mut handles = Vec::with_capacity(ports.len());
    for &port in ports.iter() {
        let ip = ip.to_string();
        let limiter = Arc::clone(&limiter);
        handles.push(tokio::spawn(async move {
            // Hold a permit only for the duration of the connect attempt, so
            // the total number of simultaneous TCP connects across the whole
            // scan never exceeds the configured concurrency cap.
            let _permit = limiter.acquire_owned().await.ok()?;
            probe_port(&ip, port, connect_timeout).await.map(|d| (port, d))
        }));
    }

    let mut open_ports = Vec::new();
    let mut fastest: Option<Duration> = None;
    for handle in handles {
        if let Ok(Some((port, elapsed))) = handle.await {
            open_ports.push(port);
            fastest = Some(fastest.map_or(elapsed, |f| f.min(elapsed)));
        }
    }
    open_ports.sort();

    ProbeResult {
        open_ports,
        latency_ms: fastest.map(|d| d.as_millis() as u64),
    }
}

impl ProbeResult {
    pub fn apply(self, host: &mut Host) {
        let now = chrono::Local::now();
        host.last_checked = Some(now);
        host.open_ports = self.open_ports;
        host.latency_ms = self.latency_ms;

        if !host.open_ports.is_empty() {
            host.status = HostStatus::Up;
            host.last_seen = Some(now);
            host.consecutive_failures = 0;
        } else {
            host.consecutive_failures += 1;
            if host.consecutive_failures >= 2 {
                host.status = HostStatus::Down;
            }
        }
    }
}
