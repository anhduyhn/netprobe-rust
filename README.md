# netprobe

Network discovery and monitoring TUI for school IT environments. Designed for roaming support technicians who need fast visibility into unfamiliar networks.

## Quick start

```
cargo build --release
./target/release/netprobe
```

No configuration needed. The tool auto-detects the local subnet, sweeps for devices, and classifies everything it finds.

## Credentials

Hyper-V VM inventory uses the credential assigned to each group in `config.toml`.
This project keeps separate credential sets for school-managed infrastructure
and DET-managed infrastructure, so the right account is used for the right
hosts. Each credential can load its password from `password_env`,
`password_file`, or `password`. Environment variables take precedence, then
password files, then the plain config value.

Hyper-V inventory also requires PowerShell and WinRM access from the machine
running `netprobe`.
Credential prompts are not used; Hyper-V queries only use the configured
credential values and report an error if those do not work.

```toml
[credentials.school]
username = "DOMAIN\\admin"
password_env = "NETPROBE_SCHOOL_PW"
# password_file = "school.password"
# password = "password"

[credentials.det]
username = "DETNSW\\svc-techname"
password_env = "NETPROBE_DET_PW"
# password_file = "det.password"
# password = "password"
```

## What it does

1. **Detects your local subnet** from the active NIC
2. **ARP sweep** to discover all devices on the segment
3. **MAC OUI classification** to separate VMs, switches, APs, printers, and workstations
4. **TCP port fingerprinting** to identify DCs, Hyper-V hosts, SCCM, NX Witness, web servers
5. **NetBIOS name query** (UDP 137, no authentication) to get hostnames and domain membership
6. **Credential realm detection** to flag DET-managed hosts (####HOST## pattern) so you don't fire school creds at the wrong domain

## Keybinds

| Key | Action |
|-----|--------|
| ↑/k, ↓/j | Navigate devices |
| v | Cycle view: All → Infra → VMs |
| r | Force full rescan |
| s | Save current state as site profile |
| d | Toggle debug overlay (internal state + event log) |
| Shift-D | Clear the debug event log |
| ? | Toggle help |
| q / Esc | Quit |

## Site profiles

Press `s` to save the current network state. Next time you visit the same school, netprobe auto-matches your subnet to the saved profile and loads your labels, notes, and pinned devices.

Profiles are stored in:
- Linux/Mac: `~/.config/netprobe/sites/`
- Windows: `%APPDATA%/netprobe/sites/`

## Role detection ports

| Port | Role |
|------|------|
| 88 + 389 | Domain Controller |
| 2179 | Hyper-V Host |
| 10123 | SCCM Server |
| 7001 | NX Witness VMS |
| 80/443 | Web Server |
| 9100/631 | Print Server |
| 5985/5986 | WinRM |

## Device classes (via MAC OUI)

| Code | Meaning |
|------|---------|
| VM | Hyper-V or VMware virtual machine |
| SW | Network switch (Cisco, Aruba, Ubiquiti) |
| AP | Wireless access point |
| PRT | Printer (Ricoh, Canon, Xerox, Kyocera, Brother) |
| WS | Physical workstation (Dell, HP, Lenovo) |
| SRV | Physical server (HPE) |
| FW | Firewall (FortiNet, SonicWall) |
