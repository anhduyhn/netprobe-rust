use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Local};
use ratatui::widgets::TableState;

/// Monotonic source of stable per-host ids. Used to route async scan results to
/// the right host even after `merge_vms_into_group` reorders the host vectors.
static NEXT_HOST_ID: AtomicU64 = AtomicU64::new(1);

/// Max number of debug log lines retained in the ring buffer.
const DEBUG_LOG_CAP: usize = 200;

// ── Host status ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostStatus {
    Unknown,
    Up,
    Down,
}

impl HostStatus {
    pub fn symbol(&self) -> &'static str {
        match self {
            Self::Unknown => "?",
            Self::Up => "●",
            Self::Down => "✖",
        }
    }
}

// ── Host ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Host {
    /// Stable identity, independent of position in the group's host vector.
    pub id: u64,
    pub name: String,
    pub ip: String,
    pub is_hyperv: bool,
    pub status: HostStatus,
    pub open_ports: Vec<u16>,
    pub latency_ms: Option<u64>,
    pub last_seen: Option<DateTime<Local>>,
    pub last_checked: Option<DateTime<Local>>,
    pub consecutive_failures: u32,
    /// If this host is a VM discovered from a Hyper-V query.
    pub is_child_vm: bool,
    /// Name of the parent Hyper-V host (for tree ordering).
    pub parent_host: Option<String>,
    /// Hyper-V state string (Running, Off, etc.) from VM query.
    pub vm_state: Option<String>,
}

impl Host {
    pub fn from_config(name: &str, ip: &str, role: Option<&str>) -> Self {
        Self {
            id: NEXT_HOST_ID.fetch_add(1, Ordering::Relaxed),
            name: name.to_string(),
            ip: ip.to_string(),
            is_hyperv: role == Some("hyperv"),
            status: HostStatus::Unknown,
            open_ports: Vec::new(),
            latency_ms: None,
            last_seen: None,
            last_checked: None,
            consecutive_failures: 0,
            is_child_vm: false,
            parent_host: None,
            vm_state: None,
        }
    }

    pub fn ports_display(&self) -> String {
        if self.open_ports.is_empty() {
            "—".into()
        } else if self.open_ports.len() > 6 {
            let shown: Vec<String> = self
                .open_ports
                .iter()
                .take(6)
                .map(|p| p.to_string())
                .collect();
            format!("{}..+{}", shown.join(","), self.open_ports.len() - 6)
        } else {
            self.open_ports
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(",")
        }
    }

    pub fn role_label(&self) -> &str {
        if self.is_hyperv {
            return "Hyper-V";
        }
        let has = |p: u16| self.open_ports.contains(&p);
        if has(88) && has(389) {
            "DC"
        } else if has(10123) {
            "SCCM"
        } else if has(7001) {
            "NxWitness"
        } else if has(9100) || has(631) {
            "Print"
        } else if has(80) || has(443) {
            "Web"
        } else if has(5985) {
            "WinRM"
        } else {
            "—"
        }
    }
}

// ── Group ───────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct Group {
    pub name: String,
    pub credential_key: Option<String>,
    pub hosts: Vec<Host>,
    pub vm_query_done: bool,
}

// ── Row reference for the flat table ────────────────────────────────────

/// A row in the TUI table. Either a group header or a host entry.
#[derive(Debug, Clone)]
pub enum TableRow {
    GroupHeader { name: String },
    HostEntry { group_idx: usize, host_idx: usize },
}

// ── App ─────────────────────────────────────────────────────────────────

pub struct App {
    pub groups: Vec<Group>,
    pub table_state: TableState,
    pub rows: Vec<TableRow>,
    pub should_quit: bool,
    pub poll_interval: u64,
    pub connect_timeout_ms: u64,
    pub last_poll: Option<DateTime<Local>>,
    pub is_scanning: bool,
    pub show_help: bool,
    pub status_message: Option<String>,
    /// When true, the debug overlay (`d`) is shown and event logging is verbose.
    pub show_debug: bool,
    /// Ring buffer of recent internal events, newest last.
    pub debug_log: VecDeque<String>,
}

impl App {
    pub fn new(poll_interval: u64, connect_timeout_ms: u64) -> Self {
        Self {
            groups: Vec::new(),
            table_state: TableState::default(),
            rows: Vec::new(),
            should_quit: false,
            poll_interval,
            connect_timeout_ms,
            last_poll: None,
            is_scanning: false,
            show_help: false,
            status_message: None,
            show_debug: false,
            debug_log: VecDeque::with_capacity(DEBUG_LOG_CAP),
        }
    }

    /// Append a timestamped line to the debug ring buffer.
    pub fn log_debug(&mut self, msg: impl Into<String>) {
        let line = format!("{} {}", Local::now().format("%H:%M:%S%.3f"), msg.into());
        if self.debug_log.len() >= DEBUG_LOG_CAP {
            self.debug_log.pop_front();
        }
        self.debug_log.push_back(line);
    }

    /// Find a host anywhere in any group by its stable id.
    pub fn find_host_mut(&mut self, id: u64) -> Option<&mut Host> {
        self.groups
            .iter_mut()
            .flat_map(|g| g.hosts.iter_mut())
            .find(|h| h.id == id)
    }

    /// Rebuild the flat row list from groups. Call after any structural change.
    pub fn rebuild_rows(&mut self) {
        self.rows.clear();
        for (gi, group) in self.groups.iter().enumerate() {
            self.rows.push(TableRow::GroupHeader {
                name: group.name.clone(),
            });
            for (hi, _host) in group.hosts.iter().enumerate() {
                self.rows.push(TableRow::HostEntry {
                    group_idx: gi,
                    host_idx: hi,
                });
            }
        }
        if self.table_state.selected().is_none() && !self.rows.is_empty() {
            // Select the first host, not the group header.
            let first_host = self
                .rows
                .iter()
                .position(|r| matches!(r, TableRow::HostEntry { .. }));
            self.table_state.select(first_host.or(Some(0)));
        }
    }

    /// Get the host at the selected row, if any.
    pub fn selected_host(&self) -> Option<&Host> {
        let idx = self.table_state.selected()?;
        match self.rows.get(idx)? {
            TableRow::HostEntry {
                group_idx,
                host_idx,
            } => self.groups.get(*group_idx)?.hosts.get(*host_idx),
            _ => None,
        }
    }

    pub fn next_row(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => (i + 1) % self.rows.len(),
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub fn prev_row(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.rows.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub fn counts(&self) -> (usize, usize, usize) {
        let all_hosts = self.groups.iter().flat_map(|g| &g.hosts);
        let up = all_hosts
            .clone()
            .filter(|h| h.status == HostStatus::Up)
            .count();
        let down = all_hosts
            .clone()
            .filter(|h| h.status == HostStatus::Down)
            .count();
        let unknown = all_hosts
            .filter(|h| h.status == HostStatus::Unknown)
            .count();
        (up, down, unknown)
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some(msg.into());
    }
}
