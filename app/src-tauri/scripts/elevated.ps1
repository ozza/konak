# Konak's only elevated step (one UAC prompt). Everything that needs admin is batched here:
#  - install WSL when missing (needs a reboot afterwards),
#  - firewall: Private-profile allow + Public-profile block for Konak's LAN programs,
#  - optionally switch the LAN network to Private, only when the user ticked it.
# It reads no file: Konak passes this script with -EncodedCommand and the parameters as inline
# base64 JSON. Only programs directly in the install bin folder get rules, and the result goes to a
# fixed name under the Konak home, never through a reparse point.
param([Parameter(Mandatory)][string]$ParamsB64)

function Test-Reparse([string]$path) {
    (Test-Path -LiteralPath $path) -and [bool]((Get-Item -LiteralPath $path -Force -ErrorAction Stop).Attributes -band [IO.FileAttributes]::ReparsePoint)
}

# Exit 3, with no result file, when the parameters are unusable or the result path is unsafe.
try {
    $p = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($ParamsB64)) | ConvertFrom-Json -ErrorAction Stop
    # The result file: <home>\scripts\elevated-result.json. Every part from the drive down must be a
    # plain folder, so the elevated write cannot be redirected elsewhere.
    $home_dir = [IO.Path]::GetFullPath([string]$p.home).TrimEnd('\')
    if (-not [IO.Path]::IsPathRooted([string]$p.home) -or $home_dir -ne ([string]$p.home).TrimEnd('\')) { exit 3 }
    $out_dir = Join-Path $home_dir 'scripts'
    $out_file = Join-Path $out_dir 'elevated-result.json'
    for ($d = $out_dir; $d; $d = Split-Path $d -Parent) { if (Test-Reparse $d) { exit 3 } }
    if (Test-Reparse $out_file) { exit 3 }
    $bin = [IO.Path]::GetFullPath([string]$p.bin).TrimEnd('\')
    $programs = @($p.programs | ForEach-Object { [string]$_ })
    $outside = @($programs | Where-Object { [IO.Path]::GetFullPath($_) -ne $_ -or (Split-Path $_ -Parent) -ne $bin -or -not $_.EndsWith('.exe', 'OrdinalIgnoreCase') })
} catch { exit 3 }
$result = [ordered]@{ reboot_required = $false; errors = @(); wsl_output = '' }

if ($p.enable_wsl -eq $true) {
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

if ($outside.Count -gt 0 -or $programs.Count -eq 0) {
    # Nothing is changed: the existing rules stay as they are.
    $result.errors += "firewall: refused, not a program in $bin`: $($outside -join ', ')"
} else {
    try {
        Get-NetFirewallRule -Group 'Konak' -ErrorAction SilentlyContinue | Remove-NetFirewallRule
        foreach ($prog in $programs) {
            $leaf = Split-Path $prog -Leaf
            New-NetFirewallRule -Group 'Konak' -DisplayName "Konak $leaf (Private allow)" -Direction Inbound -Action Allow -Profile Private -Program $prog | Out-Null
            New-NetFirewallRule -Group 'Konak' -DisplayName "Konak $leaf (Public block)" -Direction Inbound -Action Block -Profile Public -Program $prog | Out-Null
        }
    } catch { $result.errors += "firewall: $($_.Exception.Message)" }
}

$iface = 0
if ([int]::TryParse([string]$p.set_private_interface, [ref]$iface) -and $iface -gt 0) {
    try { Set-NetConnectionProfile -InterfaceIndex $iface -NetworkCategory Private -ErrorAction Stop }
    catch { $result.errors += "network profile: $($_.Exception.Message)" }
}

# CreateNew: fails rather than write through anything that appeared under that name meanwhile.
$bytes = [Text.Encoding]::UTF8.GetBytes(($result | ConvertTo-Json -Compress))
$s = [IO.File]::Open($out_file, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
try { $s.Write($bytes, 0, $bytes.Length) } finally { $s.Dispose() }
