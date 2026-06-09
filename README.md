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

[[group]]
name = "Servers"
hosts = [
    { name = "HOST01", ip = "10.0.0.10" },
    { name = "VM-DC1", ip = "10.0.0.11", parent = "HOST01" },
]
```

- `parent` displays the host indented beneath the named parent as a tree child. Children are grouped under their parent regardless of where they appear in the config. A `parent` that doesn't exist in the group, names itself, or is itself a child (one level of nesting only) logs a warning at startup — visible in the status bar and debug overlay (`d`) — and the host falls back to top-level display.
- Startup also warns about duplicate host names within a group (they make `parent` references ambiguous) and duplicate IPs anywhere in the config (the same machine would be probed more than once). Duplicates are kept and still monitored.
- A host is marked **Down** after two consecutive scans with no open ports, **Up** as soon as any port answers.

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
| r | Force full rescan |
| d | Toggle debug overlay (internal state + event log) |
| Shift-D | Clear the debug event log |
| ? | Toggle help |
| q / Esc | Quit |

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

## Building for Windows from WSL

`scripts/build-windows.sh` cross-compiles a native Windows executable (target `x86_64-pc-windows-gnu`) and deploys it to `C:\network-tools\netprobe.exe`. It is wired to a local `pre-push` git hook so every push rebuilds and redeploys; a failed build aborts the push.

Requirements inside WSL:

```
rustup target add x86_64-pc-windows-gnu
sudo apt install gcc-mingw-w64-x86-64
```
