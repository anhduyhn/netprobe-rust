mod app;
mod config;
mod hyperv;
mod network;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use app::{App, Group, Host};
use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use tokio::sync::mpsc;

enum ScanEvent {
    /// Port scan result for a host, routed by stable host id.
    PortResult {
        host_id: u64,
        result: network::ProbeResult,
    },
    /// Hyper-V VM inventory for a group.
    VmInventory {
        group_idx: usize,
        parent_name: String,
        vms: Vec<hyperv::VmInfo>,
    },
    /// Error message.
    Error(String),
    /// Scan cycle complete.
    ScanDone,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let config_path = resolve_config_path();

    let cfg = config::Config::load(&config_path)?;

    // Build app state from config.
    let mut app = App::new(cfg.settings.poll_interval, cfg.settings.connect_timeout_ms);

    for group_cfg in &cfg.group {
        let hosts: Vec<Host> = group_cfg
            .hosts
            .iter()
            .map(|h| {
                let mut host = Host::from_config(&h.name, &h.ip, h.role.as_deref());
                if let Some(ref parent) = h.parent {
                    host.is_child_vm = true;
                    host.parent_host = Some(parent.clone());
                }
                host
            })
            .collect();

        app.groups.push(Group {
            name: group_cfg.name.clone(),
            credential_key: group_cfg.credential.clone(),
            hosts,
            vm_query_done: false,
        });
    }

    app.rebuild_rows();

    let host_count: usize = app.groups.iter().map(|g| g.hosts.len()).sum();
    let group_count = app.groups.len();
    eprintln!(
        "Loaded {host_count} hosts in {group_count} groups from {}",
        config_path.display()
    );

    // Pre-resolve credentials once so every Hyper-V query reuses the same values.
    let mut resolved_creds: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    let config_dir = config_path.parent();
    for (key, entry) in &cfg.credentials {
        match entry.resolve_password(config_dir)? {
            Some(resolved) => {
                resolved_creds.insert(key.clone(), (entry.username.clone(), resolved.password));
                eprintln!("Credential '{key}': loaded from {}", resolved.source);
            }
            None => {
                eprintln!(
                    "Credential '{key}': no password source found, Hyper-V queries will be skipped"
                );
            }
        }
    }

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app, &resolved_creds).await;
    ratatui::restore();
    result
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    creds: &std::collections::HashMap<String, (String, String)>,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<ScanEvent>();

    // Initial scan.
    spawn_scan(app, &tx, creds, true);

    let poll_interval = Duration::from_secs(app.poll_interval);
    let mut last_scan = tokio::time::Instant::now();

    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
                        KeyCode::Down | KeyCode::Char('j') => app.next_row(),
                        KeyCode::Up | KeyCode::Char('k') => app.prev_row(),
                        KeyCode::Char('r') => {
                            app.set_status("Rescanning...");
                            spawn_scan(app, &tx, creds, true);
                            last_scan = tokio::time::Instant::now();
                        }
                        KeyCode::Char('?') => app.show_help = !app.show_help,
                        KeyCode::Char('d') => {
                            app.show_debug = !app.show_debug;
                            app.log_debug(if app.show_debug {
                                "debug overlay enabled"
                            } else {
                                "debug overlay disabled"
                            });
                        }
                        KeyCode::Char('D') => {
                            // Shift-D clears the debug log.
                            app.debug_log.clear();
                        }
                        _ => {}
                    }
                }
            }
        }

        // Drain events.
        while let Ok(event) = rx.try_recv() {
            match event {
                ScanEvent::PortResult { host_id, result } => {
                    let debug = app.show_debug;
                    match app.find_host_mut(host_id) {
                        Some(host) => {
                            let summary = if debug {
                                Some((host.name.clone(), result.open_ports.clone()))
                            } else {
                                None
                            };
                            result.apply(host);
                            if let Some((name, ports)) = summary {
                                let status = host.status.symbol();
                                app.log_debug(format!(
                                    "PortResult id={host_id} {name} {status} ports=[{}]",
                                    ports
                                        .iter()
                                        .map(|p| p.to_string())
                                        .collect::<Vec<_>>()
                                        .join(",")
                                ));
                            }
                        }
                        None => {
                            // A result arrived for a host that no longer exists
                            // (e.g. a VM that dropped out of inventory). Routing
                            // by id makes this explicit instead of silently
                            // corrupting whichever host now sits at that index.
                            app.log_debug(format!("PortResult id={host_id} DROPPED (no host)"));
                        }
                    }
                }
                ScanEvent::VmInventory {
                    group_idx,
                    parent_name,
                    vms,
                } => {
                    let (group_name, count) = {
                        if let Some(group) = app.groups.get_mut(group_idx) {
                            let count = vms.len();
                            hyperv::merge_vms_into_group(&parent_name, &vms, &mut group.hosts);
                            group.vm_query_done = true;
                            (group.name.clone(), count)
                        } else {
                            continue;
                        }
                    };
                    app.set_status(format!("{group_name}: {count} VMs discovered"));
                    if app.show_debug {
                        app.log_debug(format!(
                            "VmInventory group={group_idx} parent={parent_name} vms={count}"
                        ));
                    }
                    app.rebuild_rows();

                    // Port scan newly discovered VMs.
                    let connect_timeout = Duration::from_millis(app.connect_timeout_ms);
                    for host in app.groups[group_idx].hosts.iter() {
                        if host.is_child_vm && host.last_checked.is_none() && !host.ip.is_empty() {
                            let ip = host.ip.clone();
                            let tx = tx.clone();
                            let host_id = host.id;
                            tokio::spawn(async move {
                                let result = network::probe_host(&ip, connect_timeout).await;
                                let _ = tx.send(ScanEvent::PortResult { host_id, result });
                            });
                        }
                    }
                }
                ScanEvent::Error(msg) => {
                    if app.show_debug {
                        app.log_debug(format!("Error: {msg}"));
                    }
                    app.set_status(msg);
                }
                ScanEvent::ScanDone => {
                    if app.show_debug {
                        app.log_debug("ScanDone");
                    }
                    app.is_scanning = false;
                    app.last_poll = Some(chrono::Local::now());
                    if app.status_message.as_deref() == Some("Rescanning...") {
                        app.status_message = None;
                    }
                }
            }
        }

        // Periodic rescan.
        if !app.is_scanning && last_scan.elapsed() >= poll_interval {
            spawn_scan(app, &tx, creds, false);
            last_scan = tokio::time::Instant::now();
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

/// Scan all configured hosts and query Hyper-V hosts for VMs.
fn spawn_scan(
    app: &mut App,
    tx: &mpsc::UnboundedSender<ScanEvent>,
    creds: &std::collections::HashMap<String, (String, String)>,
    refresh_vm_inventory: bool,
) {
    app.is_scanning = true;
    let connect_timeout = Duration::from_millis(app.connect_timeout_ms);

    // Port scan every host. Results route back by stable host id, so it does
    // not matter if the host vector is reordered by a VM merge meanwhile.
    let mut scanned = 0usize;
    for group in app.groups.iter() {
        for host in group.hosts.iter() {
            if host.ip.is_empty() {
                continue;
            }
            let ip = host.ip.clone();
            let tx = tx.clone();
            let host_id = host.id;
            scanned += 1;
            tokio::spawn(async move {
                let result = network::probe_host(&ip, connect_timeout).await;
                let _ = tx.send(ScanEvent::PortResult { host_id, result });
            });
        }
    }
    if app.show_debug {
        app.log_debug(format!(
            "spawn_scan: {scanned} hosts, refresh_vm={refresh_vm_inventory}"
        ));
    }

    // Query Hyper-V hosts for VM inventory once per session, or on forced rescan.
    for (gi, group) in app.groups.iter_mut().enumerate() {
        if group.vm_query_done && !refresh_vm_inventory {
            continue;
        }

        let cred_key = match &group.credential_key {
            Some(k) => k.clone(),
            None => continue,
        };
        let (username, password) = match creds.get(&cred_key) {
            Some(c) => c.clone(),
            None => continue,
        };

        let hyperv_hosts: Vec<(String, String)> = group
            .hosts
            .iter()
            .filter(|host| host.is_hyperv)
            .map(|host| (host.ip.clone(), host.name.clone()))
            .collect();

        if hyperv_hosts.is_empty() {
            continue;
        }

        group.vm_query_done = true;

        for (ip, host_name) in hyperv_hosts {
            let tx = tx.clone();
            let user = username.clone();
            let pass = password.clone();
            let group_idx = gi;

            tokio::spawn(async move {
                match hyperv::query_host(&ip, &user, &pass).await {
                    Ok(vms) => {
                        let _ = tx.send(ScanEvent::VmInventory {
                            group_idx,
                            parent_name: host_name,
                            vms,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(ScanEvent::Error(format!("Hyper-V {ip}: {e}")));
                    }
                }
            });
        }
    }

    // Signal completion after port scans finish.
    let tx = tx.clone();
    let host_count: usize = app.groups.iter().map(|g| g.hosts.len()).sum();
    tokio::spawn(async move {
        let wait = Duration::from_millis(2500 + (host_count as u64 * 30));
        tokio::time::sleep(wait).await;
        let _ = tx.send(ScanEvent::ScanDone);
    });
}

fn resolve_config_path() -> PathBuf {
    if let Some(arg) = std::env::args().nth(1) {
        return PathBuf::from(arg);
    }

    let cwd_config = PathBuf::from("config.toml");
    if cwd_config.exists() {
        return cwd_config;
    }

    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("config.toml")))
        .unwrap_or(cwd_config)
}
