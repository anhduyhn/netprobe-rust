use serde::Deserialize;
use std::process::Stdio;

#[derive(Debug, Clone, Deserialize)]
pub struct VmInfo {
    #[serde(alias = "Name")]
    pub name: String,
    #[serde(alias = "State")]
    pub state: String,
    #[serde(alias = "IP")]
    pub ip: String,
}

/// Query a Hyper-V host for its VM inventory.
/// Creds are pre-resolved from env vars at startup.
pub async fn query_host(
    ip: &str,
    username: &str,
    password: &str,
) -> color_eyre::Result<Vec<VmInfo>> {
    let script = r#"
        $ErrorActionPreference = 'Stop'
        $ConfirmPreference = 'None'
        $pw = ConvertTo-SecureString $env:NETPROBE_HYPERV_PASSWORD -AsPlainText -Force
        $cred = New-Object System.Management.Automation.PSCredential($env:NETPROBE_HYPERV_USERNAME, $pw)
        $result = Invoke-Command -ComputerName $env:NETPROBE_HYPERV_HOST -Credential $cred -Authentication Negotiate -ErrorAction Stop -ScriptBlock {
            Get-VM | Select-Object `
                @{N='Name';E={$_.Name}}, `
                @{N='State';E={$_.State.ToString()}}, `
                @{N='MAC';E={($_.NetworkAdapters | ForEach-Object { $_.MacAddress }) -join ','}}, `
                @{N='IP';E={($_.NetworkAdapters | ForEach-Object { $_.IPAddresses }) -join ','}}
        }
        $result | ConvertTo-Json -Compress
        "#;

    let output = run_powershell(script, ip, username, password).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = [stderr.trim(), stdout.trim()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        return Err(color_eyre::eyre::eyre!(
            "WinRM failed for {ip} using configured credentials: {}",
            if detail.is_empty() {
                "PowerShell exited without an error message"
            } else {
                &detail
            }
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        return Ok(Vec::new());
    }

    let vms: Vec<VmInfo> = if stdout.starts_with('[') {
        serde_json::from_str(&stdout)?
    } else {
        let single: VmInfo = serde_json::from_str(&stdout)?;
        vec![single]
    };

    Ok(vms)
}

/// Add discovered VMs as child hosts in a group, positioned after their parent.
pub fn merge_vms_into_group(parent_name: &str, vms: &[VmInfo], hosts: &mut Vec<crate::app::Host>) {
    // Collect VM names to process, preserving order.
    let vm_entries: Vec<(String, String, String)> = vms
        .iter()
        .map(|vm| {
            let ip = vm
                .ip
                .split(',')
                .find(|s| s.contains('.') && !s.contains(':'))
                .unwrap_or("")
                .trim()
                .to_string();
            (vm.name.clone(), ip, vm.state.clone())
        })
        .collect();

    // First pass: update fields on existing hosts and collect their indices for relocation.
    let mut to_relocate: Vec<usize> = Vec::new();
    for (vm_name, _, vm_state) in &vm_entries {
        if let Some(idx) = hosts
            .iter()
            .position(|h| h.name.eq_ignore_ascii_case(vm_name))
        {
            hosts[idx].vm_state = Some(vm_state.clone());
            hosts[idx].is_child_vm = true;
            hosts[idx].parent_host = Some(parent_name.to_string());
            to_relocate.push(idx);
        }
    }

    // Remove existing matches (in reverse order to preserve indices).
    to_relocate.sort();
    to_relocate.dedup();
    let mut removed: Vec<crate::app::Host> = Vec::new();
    for &idx in to_relocate.iter().rev() {
        removed.push(hosts.remove(idx));
    }
    removed.reverse();

    // Find parent position (may have shifted after removals).
    let parent_idx = hosts
        .iter()
        .position(|h| h.name.eq_ignore_ascii_case(parent_name))
        .unwrap_or(hosts.len().saturating_sub(1));

    let mut insert_at = parent_idx + 1;

    // Skip past any existing children of this parent.
    while insert_at < hosts.len() && hosts[insert_at].parent_host.as_deref() == Some(parent_name) {
        insert_at += 1;
    }

    // Re-insert relocated hosts after parent.
    for host in removed {
        hosts.insert(insert_at, host);
        insert_at += 1;
    }

    // Insert new VMs (not in config) after parent.
    for (vm_name, ip, vm_state) in &vm_entries {
        if hosts.iter().any(|h| h.name.eq_ignore_ascii_case(vm_name)) {
            continue; // Already handled above.
        }

        let mut host = crate::app::Host::from_config(vm_name, ip, None);
        host.is_child_vm = true;
        host.parent_host = Some(parent_name.to_string());
        host.vm_state = Some(vm_state.clone());

        if vm_state != "Running" {
            host.status = crate::app::HostStatus::Down;
        }

        hosts.insert(insert_at, host);
        insert_at += 1;
    }
}

async fn run_powershell(
    script: &str,
    host: &str,
    username: &str,
    password: &str,
) -> color_eyre::Result<std::process::Output> {
    let mut last_not_found = None;

    for exe in ["powershell.exe", "powershell", "pwsh"] {
        match tokio::process::Command::new(exe)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-InputFormat",
                "None",
                "-Command",
                script,
            ])
            .env("NETPROBE_HYPERV_HOST", host)
            .env("NETPROBE_HYPERV_USERNAME", username)
            .env("NETPROBE_HYPERV_PASSWORD", password)
            .stdin(Stdio::null())
            .output()
            .await
        {
            Ok(output) => return Ok(output),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                last_not_found = Some(err);
            }
            Err(err) => return Err(err.into()),
        }
    }

    Err(color_eyre::eyre::eyre!(
        "PowerShell was not found. Run netprobe from Windows, or install PowerShell as 'pwsh' in this environment. Last error: {}",
        last_not_found
            .map(|err| err.to_string())
            .unwrap_or_else(|| "not found".to_string())
    ))
}
