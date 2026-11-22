# Read-only network facts for the MCP network_check and diagnose tools: the adapter that holds the
# LAN IP and its network category, Konak's firewall rules per program, and who listens on port 80.
# Prints one JSON line. Changes nothing.
param([string]$Ip, [string[]]$Programs)
$ErrorActionPreference = 'SilentlyContinue'
$out = [ordered]@{ adapter = $null; firewall = @(); port80 = @() }
if ($Ip) {
    $a = Get-NetIPAddress -IPAddress $Ip -AddressFamily IPv4 | Select-Object -First 1
    if ($a) {
        $n = Get-NetAdapter -InterfaceIndex $a.InterfaceIndex
        $c = Get-NetConnectionProfile -InterfaceIndex $a.InterfaceIndex
        $out.adapter = [ordered]@{ alias = "$($a.InterfaceAlias)"; description = "$($n.InterfaceDescription)"; index = [int]$a.InterfaceIndex; category = "$($c.NetworkCategory)" }
    }
}
$rules = @(Get-NetFirewallRule -Group 'Konak' -ErrorAction SilentlyContinue)
foreach ($prog in $Programs) {
    $found = @()
    foreach ($r in $rules) {
        $f = $r | Get-NetFirewallApplicationFilter
        if ($f.Program -ieq $prog) {
            $found += [ordered]@{ name = "$($r.DisplayName)"; enabled = ("$($r.Enabled)" -eq 'True'); action = "$($r.Action)"; profile = "$($r.Profile)"; direction = "$($r.Direction)" }
        }
    }
    $out.firewall += [ordered]@{ program = $prog; rules = $found }
}
$out.port80 = @(Get-NetTCPConnection -LocalPort 80 -State Listen | ForEach-Object { [ordered]@{ address = "$($_.LocalAddress)"; pid = [int]$_.OwningProcess } })
$out | ConvertTo-Json -Compress -Depth 5
