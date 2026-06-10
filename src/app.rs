use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Local};
use ratatui::widgets::TableState;
use tokio::sync::Semaphore;

/// Filename (in the executable's own folder) used to remember the last tab.
const STATE_FILE: &str = "netprobe-state";

/// Input focus: normal navigation vs editing the search filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Filter,
}

/// Monotonic source of stable per-host ids. Used to route async scan results
/// to the right host regardless of position in the host vectors.
static NEXT_HOST_ID: AtomicU64 = AtomicU64::new(1);

/// Max number of debug log lines retained in the ring buffer.
const DEBUG_LOG_CAP: usize = 200;

/// How long a transient status-bar message stays before auto-clearing.
const STATUS_TTL: std::time::Duration = std::time::Duration::from_secs(6);

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
            Self::Down => "✗",
        }
    }

    /// Plain-text label, used in CSV export.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Up => "up",
            Self::Down => "down",
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
        } else if has(22) {
            // Low priority: only when nothing more specific matched (e.g. the
            // Armis collectors, whose only honest port is SSH).
            "SSH"
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

        // Order: top-level hosts alphabetically, each immediately followed by
        // its children alphabetically. Case-insensitive, so neither config
        // order nor casing affects how the table reads.
        let name_key = |h: &Host| h.name.to_ascii_lowercase();

        let (mut top, mut children): (Vec<Host>, Vec<Host>) =
            std::mem::take(&mut self.hosts)
                .into_iter()
                .partition(|h| !h.is_child);
        top.sort_by_key(name_key);
        children.sort_by_key(name_key);

        let mut ordered: Vec<Host> = Vec::with_capacity(top.len() + children.len());
        for parent in top {
            let parent_key = name_key(&parent);
            ordered.push(parent);
            // Children are already in name order; partition keeps that order.
            let (mine, rest): (Vec<Host>, Vec<Host>) = children.into_iter().partition(|c| {
                c.parent
                    .as_deref()
                    .is_some_and(|p| p.eq_ignore_ascii_case(&parent_key))
            });
            ordered.extend(mine);
            children = rest;
        }

        // Defensive: any child whose parent disappeared (shouldn't happen after
        // validation) is appended rather than dropped.
        ordered.extend(children);

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
    /// TCP ports probed on each host (from config, or the built-in default).
    pub probe_ports: Arc<Vec<u16>>,
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
    /// When the current status message was set (for auto-expiry).
    pub status_set_at: Option<Instant>,
    /// Tab labels: "All" followed by each group name.
    pub tabs: Vec<String>,
    /// Active tab: 0 = All, otherwise group index + 1.
    pub active_tab: usize,
    /// When true, the debug overlay (`d`) is shown and event logging is verbose.
    pub show_debug: bool,
    /// Ring buffer of recent internal events, newest last.
    pub debug_log: VecDeque<String>,
    /// Folder the executable lives in; CSV snapshots + the state file go here.
    pub base_dir: PathBuf,
    /// When the most recent scan was kicked off (drives the idle countdown).
    pub last_scan: Instant,
    /// Normal navigation vs editing the search filter.
    pub input_mode: InputMode,
    /// Committed search filter (matches host name or ip, case-insensitive).
    pub filter_query: String,
    /// Filter text currently being edited (before Enter commits it).
    pub filter_draft: String,
    /// When true, only hosts that are not Up are shown.
    pub triage: bool,
}

impl App {
    pub fn new(
        poll_interval: u64,
        connect_timeout_ms: u64,
        max_concurrent_probes: usize,
        probe_ports: Vec<u16>,
    ) -> Self {
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
            probe_ports: Arc::new(probe_ports),
            last_poll: None,
            is_scanning: false,
            current_scan: 0,
            pending_results: 0,
            scan_deadline: None,
            show_help: false,
            status_message: None,
            status_set_at: None,
            tabs: vec!["All".to_string()],
            active_tab: 0,
            show_debug: false,
            debug_log: VecDeque::with_capacity(DEBUG_LOG_CAP),
            base_dir: resolve_base_dir(),
            last_scan: Instant::now(),
            input_mode: InputMode::Normal,
            filter_query: String::new(),
            filter_draft: String::new(),
            triage: false,
        }
    }

    // ── Search filter ────────────────────────────────────────────────────

    pub fn enter_filter(&mut self) {
        self.filter_draft = self.filter_query.clone();
        self.input_mode = InputMode::Filter;
    }

    /// Discard the in-progress edit, keep the previously committed filter.
    pub fn cancel_filter(&mut self) {
        self.filter_draft.clear();
        self.input_mode = InputMode::Normal;
    }

    pub fn commit_filter(&mut self) {
        self.filter_query = self.filter_draft.trim().to_string();
        self.input_mode = InputMode::Normal;
        self.rebuild_rows();
    }

    /// Clear an active committed filter (used by Esc in Normal mode).
    pub fn clear_filter(&mut self) {
        self.filter_query.clear();
        self.filter_draft.clear();
        self.rebuild_rows();
    }

    pub fn push_filter_char(&mut self, c: char) {
        self.filter_draft.push(c);
        self.rebuild_rows();
    }

    pub fn pop_filter_char(&mut self) {
        self.filter_draft.pop();
        self.rebuild_rows();
    }

    /// The filter text in effect for display/matching: the live draft while
    /// editing, otherwise the committed query.
    pub fn effective_query(&self) -> &str {
        if self.input_mode == InputMode::Filter {
            &self.filter_draft
        } else {
            &self.filter_query
        }
    }

    // ── Triage ───────────────────────────────────────────────────────────

    pub fn toggle_triage(&mut self) {
        self.triage = !self.triage;
        self.rebuild_rows();
    }

    // ── Scan schedule ────────────────────────────────────────────────────

    /// Seconds until the next periodic scan, or None while scanning.
    pub fn seconds_until_next_scan(&self) -> Option<u64> {
        if self.is_scanning {
            return None;
        }
        let elapsed = self.last_scan.elapsed().as_secs();
        Some(self.poll_interval.saturating_sub(elapsed))
    }

    /// Build the tab list ("All" + each group name). Call after groups load.
    pub fn build_tabs(&mut self) {
        self.tabs = std::iter::once("All".to_string())
            .chain(self.groups.iter().map(|g| g.name.clone()))
            .collect();
        if self.active_tab >= self.tabs.len() {
            self.active_tab = 0;
        }
    }

    pub fn next_tab(&mut self) {
        if self.tabs.len() < 2 {
            return;
        }
        self.active_tab = (self.active_tab + 1) % self.tabs.len();
        self.table_state.select(None);
        self.rebuild_rows();
    }

    pub fn prev_tab(&mut self) {
        if self.tabs.len() < 2 {
            return;
        }
        self.active_tab = if self.active_tab == 0 {
            self.tabs.len() - 1
        } else {
            self.active_tab - 1
        };
        self.table_state.select(None);
        self.rebuild_rows();
    }

    /// Hosts shown under the active tab.
    pub fn visible_host_count(&self) -> usize {
        let active = self.active_tab;
        self.groups
            .iter()
            .enumerate()
            .filter(|(gi, _)| active == 0 || active == gi + 1)
            .map(|(_, g)| g.hosts.len())
            .sum()
    }

    /// Mark the in-progress scan finished and stamp the poll time.
    pub fn finish_scan(&mut self) {
        self.is_scanning = false;
        self.pending_results = 0;
        self.scan_deadline = None;
        self.last_poll = Some(Local::now());
        if self.status_message.as_deref() == Some("Rescanning...") {
            self.status_message = None;
            self.status_set_at = None;
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

    /// Rebuild the flat row list from groups, applying the active tab, search
    /// filter, and triage toggle. Call after any structural or filter change.
    pub fn rebuild_rows(&mut self) {
        // Snapshot the predicate inputs into locals so the loop borrows only
        // self.groups (immutable) and self.rows (mutable) — disjoint fields.
        let query = self.effective_query().trim().to_ascii_lowercase();
        let triage = self.triage;
        let active_tab = self.active_tab;
        // On a single-group tab the tab label already names the group, so the
        // header row is redundant; keep headers only on the "All" tab.
        let show_headers = active_tab == 0;

        self.rows.clear();
        for (gi, group) in self.groups.iter().enumerate() {
            if !(active_tab == 0 || active_tab == gi + 1) {
                continue;
            }
            let mut group_rows: Vec<TableRow> = Vec::new();
            for (hi, host) in group.hosts.iter().enumerate() {
                let matches_query = query.is_empty()
                    || host.name.to_ascii_lowercase().contains(&query)
                    || host.ip.to_ascii_lowercase().contains(&query);
                let passes_triage = !triage || host.status != HostStatus::Up;
                if matches_query && passes_triage {
                    group_rows.push(TableRow::HostEntry {
                        group_idx: gi,
                        host_idx: hi,
                    });
                }
            }
            // Suppress a group whose hosts were all filtered out.
            if group_rows.is_empty() {
                continue;
            }
            if show_headers {
                self.rows.push(TableRow::GroupHeader {
                    name: group.name.clone(),
                });
            }
            self.rows.extend(group_rows);
        }

        self.clamp_selection();
    }

    /// Keep the selection on a real host row after the row set changes.
    /// MUST run on every rebuild — otherwise a stale index can point past the
    /// end (or at a header) and panic when `draw_table` indexes the host.
    fn clamp_selection(&mut self) {
        let valid = self
            .table_state
            .selected()
            .and_then(|i| self.rows.get(i))
            .is_some_and(|r| matches!(r, TableRow::HostEntry { .. }));
        if !valid {
            let first_host = self
                .rows
                .iter()
                .position(|r| matches!(r, TableRow::HostEntry { .. }));
            self.table_state.select(first_host);
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

    /// Up / down / unknown counts for the hosts under the active tab.
    pub fn counts(&self) -> (usize, usize, usize) {
        let active = self.active_tab;
        let visible = self
            .groups
            .iter()
            .enumerate()
            .filter(move |(gi, _)| active == 0 || active == gi + 1)
            .flat_map(|(_, g)| g.hosts.iter());
        let up = visible
            .clone()
            .filter(|h| h.status == HostStatus::Up)
            .count();
        let down = visible
            .clone()
            .filter(|h| h.status == HostStatus::Down)
            .count();
        let unknown = visible.filter(|h| h.status == HostStatus::Unknown).count();
        (up, down, unknown)
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some(msg.into());
        self.status_set_at = Some(Instant::now());
    }

    /// Clear the status message once it has been shown long enough. Call each
    /// loop tick.
    pub fn expire_status(&mut self) {
        if let Some(t) = self.status_set_at {
            if t.elapsed() >= STATUS_TTL {
                self.status_message = None;
                self.status_set_at = None;
            }
        }
    }

    // ── CSV export ───────────────────────────────────────────────────────

    /// Write a snapshot of ALL hosts to a timestamped CSV in the exe's folder.
    /// Returns the path written.
    pub fn export_csv(&self) -> std::io::Result<PathBuf> {
        let stamp = Local::now().format("%Y%m%d-%H%M%S");
        let path = self.base_dir.join(format!("netprobe-snapshot-{stamp}.csv"));
        std::fs::write(&path, self.build_csv())?;
        Ok(path)
    }

    /// Build the CSV snapshot text for ALL hosts (pure; no I/O).
    fn build_csv(&self) -> String {
        let mut out = String::from(
            "group,host,ip,status,role,open_ports,latency_ms,last_seen,last_checked\n",
        );
        for group in &self.groups {
            for host in &group.hosts {
                let ports = host
                    .open_ports
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(";");
                let latency = host.latency_ms.map(|m| m.to_string()).unwrap_or_default();
                let last_seen = host
                    .last_seen
                    .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_default();
                let last_checked = host
                    .last_checked
                    .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_default();
                let fields = [
                    group.name.as_str(),
                    host.name.as_str(),
                    host.ip.as_str(),
                    host.status.label(),
                    host.role_label(),
                    ports.as_str(),
                    latency.as_str(),
                    last_seen.as_str(),
                    last_checked.as_str(),
                ];
                let line = fields.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(",");
                out.push_str(&line);
                out.push('\n');
            }
        }
        out
    }

    // ── Tab persistence ──────────────────────────────────────────────────

    fn state_path(&self) -> PathBuf {
        self.base_dir.join(STATE_FILE)
    }

    /// Resolve the initial tab from the saved state file and config default.
    pub fn restore_active_tab(&mut self, default_tab: Option<&str>) {
        let saved = std::fs::read_to_string(self.state_path())
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self.active_tab = self.resolve_tab(saved.as_deref(), default_tab);
    }

    /// Pure precedence logic: saved last tab > config default_tab > "All" (0).
    /// Any label that no longer matches a tab falls through to the next.
    fn resolve_tab(&self, saved: Option<&str>, default_tab: Option<&str>) -> usize {
        for label in [saved, default_tab].into_iter().flatten() {
            if let Some(idx) = self.tabs.iter().position(|t| t.eq_ignore_ascii_case(label)) {
                return idx;
            }
        }
        0
    }

    /// Best-effort persist of the active tab label; never fails the caller.
    pub fn save_active_tab(&self) {
        if let Some(label) = self.tabs.get(self.active_tab) {
            let _ = std::fs::write(self.state_path(), label);
        }
    }
}

/// The folder the executable lives in (CSV snapshots + state file go here),
/// falling back to the current directory.
fn resolve_base_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Quote a CSV field if it contains a comma, quote, or newline (RFC 4180).
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
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
    fn hosts_are_name_sorted_with_children_under_parents() {
        let mut g = Group {
            name: "test".into(),
            hosts: vec![
                host("zeta", None),
                host("Alpha", None),
                host("mike", Some("zeta")),
                host("bravo", Some("Zeta")), // case-insensitive parent + sort
            ],
        };
        let warnings = g.organise();
        assert!(warnings.is_empty());
        // Top level alphabetical (Alpha, zeta); children alphabetical under zeta.
        assert_eq!(names(&g), vec!["Alpha", "zeta", "bravo", "mike"]);
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

    fn app_with(groups: &[(&str, &[(&str, &str, HostStatus)])]) -> App {
        let mut app = App::new(30, 2000, 64, vec![80]);
        for (gname, hosts) in groups {
            let hs = hosts
                .iter()
                .map(|(n, ip, st)| {
                    let mut h = Host::from_config(n, ip);
                    h.status = *st;
                    h
                })
                .collect();
            app.groups.push(Group {
                name: (*gname).into(),
                hosts: hs,
            });
        }
        app.build_tabs();
        app.rebuild_rows();
        app
    }

    fn host_row_count(app: &App) -> usize {
        app.rows
            .iter()
            .filter(|r| matches!(r, TableRow::HostEntry { .. }))
            .count()
    }

    #[test]
    fn filter_matches_name_or_ip_case_insensitively() {
        let mut app = app_with(&[(
            "G",
            &[
                ("KP-DC01", "10.0.0.1", HostStatus::Up),
                ("KP-PS01", "10.0.0.2", HostStatus::Up),
                ("CC-DNS", "10.0.0.99", HostStatus::Up),
            ],
        )]);
        app.filter_query = "dc".into();
        app.rebuild_rows();
        assert_eq!(host_row_count(&app), 1); // KP-DC01

        app.filter_query = "10.0.0.".into();
        app.rebuild_rows();
        assert_eq!(host_row_count(&app), 3); // all by ip

        app.filter_query = ".99".into();
        app.rebuild_rows();
        assert_eq!(host_row_count(&app), 1); // CC-DNS by ip
    }

    #[test]
    fn triage_hides_up_hosts_and_suppresses_empty_groups() {
        let mut app = app_with(&[
            (
                "AllUp",
                &[("a", "10.0.0.1", HostStatus::Up), ("b", "10.0.0.2", HostStatus::Up)],
            ),
            (
                "HasDown",
                &[("c", "10.0.0.3", HostStatus::Down), ("d", "10.0.0.4", HostStatus::Up)],
            ),
        ]);
        app.triage = true;
        app.rebuild_rows();
        // Only the Down host 'c' remains; the all-Up group's header is suppressed.
        assert_eq!(host_row_count(&app), 1);
        let headers = app
            .rows
            .iter()
            .filter(|r| matches!(r, TableRow::GroupHeader { .. }))
            .count();
        assert_eq!(headers, 1); // only "HasDown"
    }

    #[test]
    fn csv_export_has_header_row_per_host_and_quotes_ports() {
        let mut app = app_with(&[(
            "Grp,A",
            &[("h1", "10.0.0.1", HostStatus::Up)],
        )]);
        // Give the host multiple open ports so the field needs quoting.
        app.groups[0].hosts[0].open_ports = vec![80, 443];
        let csv = app.build_csv();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 2); // header + 1 host
        assert!(lines[0].starts_with("group,host,ip,status,role,open_ports"));
        // group name with a comma is quoted; ports joined with ';' (no comma,
        // so no quoting needed).
        assert!(lines[1].contains("\"Grp,A\""));
        assert!(lines[1].contains("80;443"));
        assert!(!lines[1].contains("\"80;443\""));
        assert!(lines[1].contains("10.0.0.1"));
        assert!(lines[1].contains("up"));
    }

    #[test]
    fn csv_field_quotes_only_when_needed() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn resolve_tab_precedence_and_fallback() {
        let mut app = App::new(30, 2000, 64, vec![80]);
        app.tabs = ["All", "DET", "Infra"].iter().map(|s| s.to_string()).collect();

        assert_eq!(app.resolve_tab(Some("Infra"), Some("DET")), 2); // saved wins
        assert_eq!(app.resolve_tab(None, Some("DET")), 1); // default
        assert_eq!(app.resolve_tab(Some("infra"), None), 2); // case-insensitive
        assert_eq!(app.resolve_tab(Some("Gone"), Some("DET")), 1); // invalid saved → default
        assert_eq!(app.resolve_tab(Some("Gone"), Some("AlsoGone")), 0); // → All
        assert_eq!(app.resolve_tab(None, None), 0); // → All
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
        // A (top) + its child B, then demoted-to-top C and D, all alphabetical.
        assert_eq!(names(&g), vec!["A", "B", "C", "D"]);
    }
}
