mod app;
mod config;
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
                let mut host = Host::from_config(&h.name, &h.ip);
                if let Some(ref parent) = h.parent {
                    host.is_child = true;
                    host.parent = Some(parent.clone());
                }
                host
            })
            .collect();

        app.groups.push(Group {
            name: group_cfg.name.clone(),
            hosts,
        });
    }

    // Check for duplicate names/IPs, then validate parent references and
    // order children beneath their parents.
    let mut config_warnings: Vec<String> = cfg.validate();
    for group in &mut app.groups {
        config_warnings.extend(group.organise());
    }

    app.rebuild_rows();

    let host_count: usize = app.groups.iter().map(|g| g.hosts.len()).sum();
    let group_count = app.groups.len();
    eprintln!(
        "Loaded {host_count} hosts in {group_count} groups from {}",
        config_path.display()
    );
    for warning in &config_warnings {
        eprintln!("warning: {warning}");
        app.log_debug(format!("config warning: {warning}"));
    }
    if !config_warnings.is_empty() {
        app.set_status(format!(
            "{} config warning(s) — press d for details",
            config_warnings.len()
        ));
    }

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app).await;
    ratatui::restore();
    result
}

async fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<ScanEvent>();

    // Initial scan.
    spawn_scan(app, &tx);

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
                            spawn_scan(app, &tx);
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
                            // Defensive: a result for a host that no longer
                            // exists is dropped explicitly rather than applied
                            // to the wrong host.
                            app.log_debug(format!("PortResult id={host_id} DROPPED (no host)"));
                        }
                    }
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
            spawn_scan(app, &tx);
            last_scan = tokio::time::Instant::now();
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

/// Port scan all configured hosts.
fn spawn_scan(app: &mut App, tx: &mpsc::UnboundedSender<ScanEvent>) {
    app.is_scanning = true;
    let connect_timeout = Duration::from_millis(app.connect_timeout_ms);

    // Results route back by stable host id, so ordering of the host vectors
    // never matters to in-flight scans.
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
        app.log_debug(format!("spawn_scan: {scanned} hosts"));
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
