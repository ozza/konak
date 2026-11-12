# Konak's only elevated step (one UAC prompt). Everything that needs admin is batched here:
#  - install WSL when missing (needs a reboot afterwards),
#  - firewall: Private-profile allow + Public-profile block for Konak's LAN programs,
#  - optionally switch the LAN network to Private, only when the user ticked it.
# Parameters come from a JSON file; the result is written back as JSON.
param([Parameter(Mandatory)][string]$ParamFile)
$p = Get-Content -Raw -LiteralPath $ParamFile | ConvertFrom-Json
$result = [ordered]@{ reboot_required = $false; errors = @(); wsl_output = '' }

if ($p.enable_wsl) {
    try {
        $env:WSL_UTF8 = '1'
        $result.wsl_output = (& wsl.exe --install --no-distribution 2>&1 | Out-String) -replace "`0", ''
        foreach ($f in 'VirtualMachinePlatform', 'Microsoft-Windows-Subsystem-Linux') {
            $feature = Get-WindowsOptionalFeature -Online -FeatureName $f
            if ($feature.RestartNeeded -or $feature.State -eq 'EnablePending') { $result.reboot_required = $true }
        }
        if ($result.wsl_output -match 'reboot|restart|yeniden ba') { $result.reboot_required = $true }
    } catch { $result.errors += "wsl: $($_.Exception.Message)" }
}

try {
    Get-NetFirewallRule -Group 'Konak' -ErrorAction SilentlyContinue | Remove-NetFirewallRule
    foreach ($prog in @($p.programs)) {
        $leaf = Split-Path $prog -Leaf
        New-NetFirewallRule -Group 'Konak' -DisplayName "Konak $leaf (Private allow)" -Direction Inbound -Action Allow -Profile Private -Program $prog | Out-Null
        New-NetFirewallRule -Group 'Konak' -DisplayName "Konak $leaf (Public block)" -Direction Inbound -Action Block -Profile Public -Program $prog | Out-Null
    }
} catch { $result.errors += "firewall: $($_.Exception.Message)" }

if ($p.set_private_interface -gt 0) {
    try { Set-NetConnectionProfile -InterfaceIndex $p.set_private_interface -NetworkCategory Private -ErrorAction Stop }
    catch { $result.errors += "network profile: $($_.Exception.Message)" }
}

$result | ConvertTo-Json -Compress | Set-Content -Encoding utf8 -LiteralPath $p.result_file
