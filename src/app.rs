use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Local};
use ratatui::widgets::TableState;
use tokio::sync::Semaphore;

/// Monotonic source of stable per-host ids. Used to route async scan results
/// to the right host regardless of position in the host vectors.
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
    pub status: HostStatus,
    pub open_ports: Vec<u16>,
    pub latency_ms: Option<u64>,
    pub last_seen: Option<DateTime<Local>>,
    pub last_checked: Option<DateTime<Local>>,
    pub consecutive_failures: u32,
    /// Display this host indented under its parent (set from config).
    pub is_child: bool,
    /// Name of the parent host (cosmetic tree grouping from config).
    pub parent: Option<String>,
}

impl Host {
    pub fn from_config(name: &str, ip: &str) -> Self {
        Self {
            id: NEXT_HOST_ID.fetch_add(1, Ordering::Relaxed),
            name: name.to_string(),
            ip: ip.to_string(),
            status: HostStatus::Unknown,
            open_ports: Vec::new(),
            latency_ms: None,
            last_seen: None,
            last_checked: None,
            consecutive_failures: 0,
            is_child: false,
            parent: None,
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

    /// Infer a role label from the set of open ports.
    pub fn role_label(&self) -> &str {
        let has = |p: u16| self.open_ports.contains(&p);
        if has(88) && has(389) {
            "DC"
        } else if has(2179) {
            "Hyper-V"
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
    pub hosts: Vec<Host>,
}

impl Group {
    /// Validate parent references and order hosts so each child sits directly
    /// beneath its parent, regardless of config order. Only one level of
    /// nesting is supported. Returns human-readable warnings for invalid
    /// references; offending hosts fall back to top-level display.
    pub fn organise(&mut self) -> Vec<String> {
        let mut warnings = Vec::new();

        // Validate parent references first.
        for i in 0..self.hosts.len() {
            let Some(parent_name) = self.hosts[i].parent.clone() else {
                continue;
            };
            let host_name = self.hosts[i].name.clone();

            let invalid_reason = if parent_name.eq_ignore_ascii_case(&host_name) {
                Some("cannot be its own parent".to_string())
            } else {
                match self
                    .hosts
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case(&parent_name))
                {
                    None => Some(format!(
                        "parent '{parent_name}' not found in group '{}'",
                        self.name
                    )),
                    Some(p) if p.parent.is_some() => Some(format!(
                        "parent '{parent_name}' is itself a child (only one level of nesting is supported)"
                    )),
                    Some(_) => None,
                }
            };

            if let Some(reason) = invalid_reason {
                warnings.push(format!(
                    "host '{host_name}': {reason}; showing at top level"
                ));
                self.hosts[i].is_child = false;
                self.hosts[i].parent = None;
            }
        }

        // Order: parents in config order, each followed by its children in
        // config order.
        let mut slots: Vec<Option<Host>> = std::mem::take(&mut self.hosts)
            .into_iter()
            .map(Some)
            .collect();
        let mut ordered: Vec<Host> = Vec::with_capacity(slots.len());

        for i in 0..slots.len() {
            if slots[i].as_ref().is_none_or(|h| h.is_child) {
                continue;
            }
            let parent = slots[i].take().expect("slot checked non-empty");
            let parent_name = parent.name.clone();
            ordered.push(parent);
            for slot in slots.iter_mut() {
                let is_mine = slot.as_ref().is_some_and(|h| {
                    h.is_child
                        && h.parent
                            .as_deref()
                            .is_some_and(|p| p.eq_ignore_ascii_case(&parent_name))
                });
                if is_mine {
                    ordered.push(slot.take().expect("slot checked non-empty"));
                }
            }
        }

        // Defensive: anything unplaced (shouldn't happen after validation)
        // keeps its config position at the end rather than vanishing.
        ordered.extend(slots.into_iter().flatten());

        self.hosts = ordered;
        warnings
    }
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
    /// Caps simultaneous TCP connects across the whole scan.
    pub probe_limiter: Arc<Semaphore>,
    pub max_concurrent_probes: usize,
    pub last_poll: Option<DateTime<Local>>,
    pub is_scanning: bool,
    /// Generation counter, bumped each scan, so late results from a superseded
    /// scan don't affect completion tracking of the current one.
    pub current_scan: u64,
    /// Outstanding per-host results expected for the current scan.
    pub pending_results: usize,
    /// Backstop: force-finish a scan if results stop arriving past this point.
    pub scan_deadline: Option<Instant>,
    pub show_help: bool,
    pub status_message: Option<String>,
    /// When true, the debug overlay (`d`) is shown and event logging is verbose.
    pub show_debug: bool,
    /// Ring buffer of recent internal events, newest last.
    pub debug_log: VecDeque<String>,
}

impl App {
    pub fn new(poll_interval: u64, connect_timeout_ms: u64, max_concurrent_probes: usize) -> Self {
        // A zero cap would deadlock the semaphore; clamp to a sane minimum.
        let max_concurrent_probes = max_concurrent_probes.max(1);
        Self {
            groups: Vec::new(),
            table_state: TableState::default(),
            rows: Vec::new(),
            should_quit: false,
            poll_interval,
            connect_timeout_ms,
            probe_limiter: Arc::new(Semaphore::new(max_concurrent_probes)),
            max_concurrent_probes,
            last_poll: None,
            is_scanning: false,
            current_scan: 0,
            pending_results: 0,
            scan_deadline: None,
            show_help: false,
            status_message: None,
            show_debug: false,
            debug_log: VecDeque::with_capacity(DEBUG_LOG_CAP),
        }
    }

    /// Mark the in-progress scan finished and stamp the poll time.
    pub fn finish_scan(&mut self) {
        self.is_scanning = false;
        self.pending_results = 0;
        self.scan_deadline = None;
        self.last_poll = Some(Local::now());
        if self.status_message.as_deref() == Some("Rescanning...") {
            self.status_message = None;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, parent: Option<&str>) -> Host {
        let mut h = Host::from_config(name, "10.0.0.1");
        if let Some(p) = parent {
            h.is_child = true;
            h.parent = Some(p.to_string());
        }
        h
    }

    fn names(group: &Group) -> Vec<&str> {
        group.hosts.iter().map(|h| h.name.as_str()).collect()
    }

    #[test]
    fn children_move_under_parent_regardless_of_config_order() {
        let mut g = Group {
            name: "test".into(),
            hosts: vec![
                host("VM1", Some("HOST2")),
                host("HOST1", None),
                host("HOST2", None),
                host("VM2", Some("host1")), // parent match is case-insensitive
            ],
        };
        let warnings = g.organise();
        assert!(warnings.is_empty());
        assert_eq!(names(&g), vec!["HOST1", "VM2", "HOST2", "VM1"]);
    }

    #[test]
    fn missing_parent_warns_and_falls_back_to_top_level() {
        let mut g = Group {
            name: "test".into(),
            hosts: vec![host("HOST1", None), host("VM1", Some("NOPE"))],
        };
        let warnings = g.organise();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("NOPE"));
        assert!(!g.hosts[1].is_child);
        assert_eq!(names(&g), vec!["HOST1", "VM1"]);
    }

    #[test]
    fn nested_and_self_parents_are_rejected() {
        let mut g = Group {
            name: "test".into(),
            hosts: vec![
                host("A", None),
                host("B", Some("A")),
                host("C", Some("B")), // nested: B is itself a child
                host("D", Some("D")), // self-parent
            ],
        };
        let warnings = g.organise();
        assert_eq!(warnings.len(), 2);
        let by_name = |n: &str| g.hosts.iter().find(|h| h.name == n).unwrap();
        assert!(!by_name("C").is_child);
        assert!(!by_name("D").is_child);
        assert_eq!(names(&g), vec!["A", "B", "C", "D"]);
    }
}
