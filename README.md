# netprobe

A simple config-driven TCP port monitoring TUI for school IT environments. Reads a list of hosts from `config.toml`, probes a fixed set of well-known ports on each, and shows up/down status, latency, open ports, and an inferred role in a terminal table.

No credentials, no remote execution, no network discovery — it only opens outbound TCP connections to the hosts you list.

## Quick start

```
cargo build --release
./target/release/netprobe [path/to/config.toml]
```

With no argument, netprobe looks for `config.toml` in the current directory, then next to the executable.

## Configuration

Copy `config.toml.sample` to `config.toml` and edit:

```toml
[settings]
poll_interval = 30          # seconds between automatic rescans
connect_timeout_ms = 2000   # TCP connect timeout per port
max_concurrent_probes = 64  # max simultaneous TCP connects across the whole scan
# ports = [22, 88, 135, 389, 445, 3389, 5985]   # optional: override the probed TCP ports (built-in default used if unset)
# default_tab = "All"                            # optional: tab to open on (a group name or "All")

[[group]]
name = "Servers"
hosts = [
    { name = "HOST01", ip = "10.0.0.10" },
    { name = "VM-DC1", ip = "10.0.0.11", parent = "HOST01" },
]
```

The full list of settings: `poll_interval`, `connect_timeout_ms`, `max_concurrent_probes`, `ports` (see [Probed ports](#probed-ports-and-role-inference) and [Liveness false positives](#liveness-false-positives-intercepted-ports)), and `default_tab`.

- Hosts are displayed alphabetically by name (case-insensitive) within each group, regardless of config order. Groups stay in config order.
- A tab bar (one tab per group, plus **All**) lets you focus on a single group with ←/→ or Tab. The header up/down/unknown counts reflect the active tab, so groups that are expected to be unreachable from your current network (e.g. DET-managed hosts when you're on the curriculum network) can be kept on their own tab instead of cluttering the view. Rescans still cover every host regardless of the active tab.
- `parent` displays the host indented beneath the named parent as a tree child. Children are grouped under their parent and sorted alphabetically among themselves, regardless of where they appear in the config. A `parent` that doesn't exist in the group, names itself, or is itself a child (one level of nesting only) logs a warning at startup — visible in the status bar and debug overlay (`d`) — and the host falls back to top-level display.
- Startup also warns about duplicate host names within a group (they make `parent` references ambiguous) and duplicate IPs anywhere in the config (the same machine would be probed more than once). Duplicates are kept and still monitored.
- A host is marked **Down** after two consecutive scans with no open ports, **Up** as soon as any port answers.
- `ports` (optional, under `[settings]`) overrides the list of TCP ports probed on every host — used for both the up/down decision and role inference. When unset, a built-in default list is used. The startup banner prints the active list.
- `default_tab` (optional, under `[settings]`) is the tab to open on — a group name or `"All"`. The last tab you were on is remembered across launches (stored in a `netprobe-state` file beside the executable) and takes precedence over `default_tab`.
- Press `e` to write a CSV snapshot of all hosts (`netprobe-snapshot-<timestamp>.csv`) into the executable's folder; the status bar shows the path. Each export is a new timestamped file. (JSON is intentionally not supported — it would add a dependency; CSV opens directly in Excel.)

### Liveness false positives (intercepted ports)

netprobe marks a host **Up** as soon as *any* probed port completes a TCP handshake. On some networks a security appliance accepts connections to a port for *every* address — a DNS firewall answering TCP/53 is the common one — so a shut-down host whose only "open" port is the intercepted one still looks online. You can confirm interception by probing a deliberately nonexistent IP: if `Test-NetConnection -Port 53 10.99.99.99` succeeds, that port is intercepted and is not a reliable liveness signal. Drop it from the probe list:

```toml
[settings]
# default list minus 53
ports = [80, 88, 135, 389, 443, 445, 636, 2179, 3389, 4660, 5985, 5986, 7001, 9100, 10123]
```

### Scanning over a VPN

Each scan opens one TCP connection per probed port per host. Fired all at once, that burst can overwhelm a constrained link such as a Citrix/SSL‑VPN tunnel, which drops connection attempts under load and makes hosts flap between up and down between scans. Two settings tame this:

- `max_concurrent_probes` caps how many connects are in flight at once across the entire scan. The default of 64 suits a LAN; drop it to **16–32** on a flaky VPN to trade a little scan speed for stable results.
- `connect_timeout_ms` should be raised (e.g. **3000–4000**) when VPN round‑trips are slow, so a real-but-slow response isn't counted as a closed port.

The latency (`ms`) column shows the fastest port that answered, so you can compare a host's responsiveness on the curriculum network versus the VPN directly.

Note that hosts only reachable from a specific network (e.g. DET‑managed servers behind the admin/VPN network) will always read **Down** from a network that can't route to them — that's expected, not a scan fault.

## Keybinds

| Key | Action |
|-----|--------|
| ↑/k, ↓/j | Navigate hosts |
| ←/h, →/l, Tab | Switch group tab |
| / | Search: filter by name or IP (Enter applies, Esc clears) |
| t | Triage: show only hosts that are not Up |
| e | Export a CSV snapshot next to the executable |
| r | Force full rescan |
| d | Toggle debug overlay (internal state + event log) |
| Shift-D | Clear the debug event log |
| ? | Toggle help |
| q / Esc | Quit (Esc clears an active search first) |

## Probed ports and role inference

Each host is probed on: 53, 80, 88, 135, 389, 443, 445, 636, 2179, 3389, 4660, 5985, 5986, 7001, 9100, 10123. A role label is inferred from what answers:

| Open ports | Role |
|------|------|
| 88 + 389 | Domain Controller |
| 2179 | Hyper-V Host |
| 10123 | SCCM Server |
| 7001 | NX Witness VMS |
| 9100 / 631 | Print Server |
| 80 / 443 | Web Server |
| 5985 | WinRM |
| 22 | SSH (fallback when nothing more specific matches) |

## Building for Windows from WSL

`scripts/build-windows.sh` cross-compiles a native Windows executable (target `x86_64-pc-windows-gnu`) and deploys it to `C:\network-tools\netprobe.exe`. It is wired to a local `pre-push` git hook so every push rebuilds and redeploys; a failed build aborts the push.

Requirements inside WSL:

```
rustup target add x86_64-pc-windows-gnu
sudo apt install gcc-mingw-w64-x86-64
```
