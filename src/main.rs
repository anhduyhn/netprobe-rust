mod app;
mod config;
mod network;
mod ui;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use app::{App, Group, Host};
use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use tokio::sync::mpsc;

enum ScanEvent {
    /// Port scan result for a host, routed by stable host id.
    PortResult {
        scan_id: u64,
        host_id: u64,
        result: network::ProbeResult,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let config_path = resolve_config_path();
    let cfg = config::Config::load(&config_path)?;

    // Resolve the probe port list: an explicit non-empty `ports` from config,
    // otherwise the built-in default.
    let ports_overridden = cfg
        .settings
        .ports
        .as_ref()
        .is_some_and(|p| !p.is_empty());
    let probe_ports = cfg
        .settings
        .ports
        .clone()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(network::default_ports);

    // Build app state from config.
    let mut app = App::new(
        cfg.settings.poll_interval,
        cfg.settings.connect_timeout_ms,
        cfg.settings.max_concurrent_probes,
        probe_ports,
    );

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

    app.build_tabs();
    app.restore_active_tab(cfg.settings.default_tab.as_deref());
    app.rebuild_rows();

    let host_count: usize = app.groups.iter().map(|g| g.hosts.len()).sum();
    let group_count = app.groups.len();
    eprintln!(
        "Loaded {host_count} hosts in {group_count} groups from {}",
        config_path.display()
    );
    eprintln!(
        "Concurrency cap: {} simultaneous connects; connect timeout {} ms",
        app.max_concurrent_probes, app.connect_timeout_ms
    );
    eprintln!(
        "Probing {} ports{}: {}",
        app.probe_ports.len(),
        if ports_overridden {
            " (from config)"
        } else {
            " (default)"
        },
        app.probe_ports
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",")
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

    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    if app.input_mode == app::InputMode::Filter {
                        // While editing the filter, all keys edit the query.
                        match key.code {
                            KeyCode::Esc => app.cancel_filter(),
                            KeyCode::Enter => app.commit_filter(),
                            KeyCode::Backspace => app.pop_filter_char(),
                            KeyCode::Char(c) => app.push_filter_char(c),
                            _ => {}
                        }
                    } else {
                        match key.code {
                            KeyCode::Char('q') => app.should_quit = true,
                            KeyCode::Esc => {
                                // Esc clears an active search first; quits only
                                // when there's no filter to clear.
                                if !app.filter_query.is_empty() {
                                    app.clear_filter();
                                } else {
                                    app.should_quit = true;
                                }
                            }
                            KeyCode::Down | KeyCode::Char('j') => app.next_row(),
                            KeyCode::Up | KeyCode::Char('k') => app.prev_row(),
                            KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => app.next_tab(),
                            KeyCode::Left | KeyCode::Char('h') | KeyCode::BackTab => app.prev_tab(),
                            KeyCode::Char('/') => app.enter_filter(),
                            KeyCode::Char('t') => app.toggle_triage(),
                            KeyCode::Char('e') => match app.export_csv() {
                                Ok(path) => app.set_status(format!("Exported {}", path.display())),
                                Err(err) => {
                                    app.set_status(format!("Export failed: {err}"));
                                    app.log_debug(format!("export error: {err}"));
                                }
                            },
                            KeyCode::Char('r') => {
                                app.set_status("Rescanning...");
                                spawn_scan(app, &tx);
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
        }

        // Drain events.
        while let Ok(event) = rx.try_recv() {
            match event {
                ScanEvent::PortResult {
                    scan_id,
                    host_id,
                    result,
                } => {
                    let debug = app.show_debug;
                    match app.find_host_mut(host_id) {
                        Some(host) => {
                            let summary = if debug {
                                Some((host.name.clone(), result.open_ports.clone()))
                            } else {
                                None
                            };
                            // Late results from a superseded scan still carry
                            // valid data, so always apply by stable host id.
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

                    // Only the current scan's results count toward completion.
                    if scan_id == app.current_scan && app.pending_results > 0 {
                        app.pending_results -= 1;
                        if app.pending_results == 0 {
                            app.finish_scan();
                        }
                    }
                }
            }
        }

        // Backstop: if results stall (a dropped task, a hung tunnel), don't
        // leave the scan flagged as running forever — that would block every
        // future periodic rescan.
        if app.is_scanning {
            if let Some(deadline) = app.scan_deadline {
                if Instant::now() >= deadline {
                    let missing = app.pending_results;
                    app.log_debug(format!(
                        "scan watchdog fired: {missing} result(s) never arrived"
                    ));
                    app.finish_scan();
                }
            }
        }

        // Auto-clear a stale status message (e.g. the "Exported …" confirmation).
        app.expire_status();

        // Periodic rescan.
        if !app.is_scanning && app.last_scan.elapsed() >= poll_interval {
            spawn_scan(app, &tx);
        }

        if app.should_quit {
            app.save_active_tab();
            return Ok(());
        }
    }
}

/// Port scan all configured hosts.
fn spawn_scan(app: &mut App, tx: &mpsc::UnboundedSender<ScanEvent>) {
    app.current_scan += 1;
    let scan_id = app.current_scan;
    app.is_scanning = true;
    app.last_scan = Instant::now();
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
            let limiter = app.probe_limiter.clone();
            let ports = app.probe_ports.clone();
            scanned += 1;
            tokio::spawn(async move {
                let result = network::probe_host(&ip, connect_timeout, limiter, ports).await;
                let _ = tx.send(ScanEvent::PortResult {
                    scan_id,
                    host_id,
                    result,
                });
            });
        }
    }

    app.pending_results = scanned;

    // Generous backstop only — a real scan finishes when the last result lands.
    // Worst case is every probe hitting the full timeout, run in concurrency-
    // capped batches; double that and add slack so the watchdog never cuts a
    // legitimately slow VPN scan short.
    let total_probes = scanned * app.probe_ports.len();
    let batches = total_probes.div_ceil(app.max_concurrent_probes.max(1)) as u32;
    let budget = connect_timeout
        .saturating_mul(batches.max(1))
        .saturating_mul(2)
        + Duration::from_secs(5);
    app.scan_deadline = Some(Instant::now() + budget);

    if app.show_debug {
        app.log_debug(format!(
            "spawn_scan #{scan_id}: {scanned} hosts, cap={}, budget={}s",
            app.max_concurrent_probes,
            budget.as_secs()
        ));
    }

    // Nothing to scan: finish immediately so we don't sit flagged as scanning.
    if scanned == 0 {
        app.finish_scan();
    }
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
