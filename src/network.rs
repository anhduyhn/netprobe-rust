use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::app::{Host, HostStatus};

/// Ports to probe for role detection.
const PROBE_PORTS: &[u16] = &[
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

async fn probe_port(ip: &str, port: u16, connect_timeout: Duration) -> bool {
    let addr: SocketAddr = match format!("{ip}:{port}").parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    timeout(connect_timeout, TcpStream::connect(addr))
        .await
        .is_ok_and(|r| r.is_ok())
}

pub struct ProbeResult {
    pub open_ports: Vec<u16>,
    pub latency_ms: u64,
}

pub async fn probe_host(ip: &str, connect_timeout: Duration) -> ProbeResult {
    let start = Instant::now();

    let mut handles = Vec::with_capacity(PROBE_PORTS.len());
    for &port in PROBE_PORTS {
        let ip = ip.to_string();
        handles.push(tokio::spawn(async move {
            (port, probe_port(&ip, port, connect_timeout).await)
        }));
    }

    let mut open_ports = Vec::new();
    for handle in handles {
        if let Ok((port, is_open)) = handle.await {
            if is_open {
                open_ports.push(port);
            }
        }
    }
    open_ports.sort();

    ProbeResult {
        open_ports,
        latency_ms: start.elapsed().as_millis() as u64,
    }
}

impl ProbeResult {
    pub fn apply(self, host: &mut Host) {
        let now = chrono::Local::now();
        host.last_checked = Some(now);
        host.open_ports = self.open_ports;
        host.latency_ms = Some(self.latency_ms);

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
