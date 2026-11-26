# Konak's only elevated step (one UAC prompt). Everything that needs admin is batched here:
#  - install WSL when missing (needs a reboot afterwards),
#  - firewall: Private-profile allow + Public-profile block for Konak's LAN programs,
#  - optionally switch the LAN network to Private, only when the user ticked it.
# It reads no file and writes none: Konak passes this script with -EncodedCommand and the parameters
# as inline base64 JSON, and the result goes back through a named pipe Konak created before the
# prompt (only elevated administrators may open it). Only programs directly in the install bin
# folder get rules.
# Exit code: 3 when the parameters are refused; otherwise 4096 + flags (1 reboot required, 2 WSL
# error, 4 firewall error, 8 network profile error), so Konak knows the outcome without the pipe.
param([Parameter(Mandatory)][string]$ParamsB64)

try {
    $p = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($ParamsB64)) | ConvertFrom-Json -ErrorAction Stop
    $pipe = [string]$p.pipe
    if ($pipe -cnotmatch '^konak-elevated-[0-9a-f]{32}$') { exit 3 }
    $bin = [IO.Path]::GetFullPath([string]$p.bin).TrimEnd('\')
    $programs = @($p.programs | ForEach-Object { [string]$_ })
    $outside = @($programs | Where-Object { [IO.Path]::GetFullPath($_) -ne $_ -or (Split-Path $_ -Parent) -ne $bin -or -not $_.EndsWith('.exe', 'OrdinalIgnoreCase') })
} catch { exit 3 }
$result = [ordered]@{ reboot_required = $false; errors = @(); wsl_output = '' }

if ($p.enable_wsl -eq $true) {
    try {
        $env:WSL_UTF8 = '1'
        $w = (& wsl.exe --install --no-distribution 2>&1 | Out-String) -replace "`0", ''
        foreach ($f in 'VirtualMachinePlatform', 'Microsoft-Windows-Subsystem-Linux') {
            $feature = Get-WindowsOptionalFeature -Online -FeatureName $f
            if ($feature.RestartNeeded -or $feature.State -eq 'EnablePending') { $result.reboot_required = $true }
        }
        if ($w -match 'reboot|restart|yeniden ba') { $result.reboot_required = $true }
        if ($w.Length -gt 4000) { $w = $w.Substring($w.Length - 4000) }
        $result.wsl_output = $w
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

$flags = 0
if ($result.reboot_required) { $flags += 1 }
foreach ($e in @(@('wsl:', 2), @('firewall:', 4), @('network profile:', 8))) {
    if (@($result.errors | Where-Object { $_.StartsWith($e[0]) }).Count -gt 0) { $flags += $e[1] }
}
try {
    $bytes = [Text.Encoding]::UTF8.GetBytes(($result | ConvertTo-Json -Compress))
    $c = New-Object IO.Pipes.NamedPipeClientStream('.', $pipe, [IO.Pipes.PipeDirection]::Out)
    try { $c.Connect(5000); $c.Write($bytes, 0, $bytes.Length); $c.Flush() } finally { $c.Dispose() }
} catch { }
exit (4096 + $flags)
