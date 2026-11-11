# Spike step 3 probe. Runs every minute; appends one CSV line per run to survival.csv.
# Columns: time, distro state, HTTP code per app via localhost and via the LAN name route.
$base = "$env:LOCALAPPDATA\Konak-spike"
$csv = "$base\survival.csv"
if (-not (Test-Path $csv)) { Add-Content $csv 'time,distro,jellyfin_local,paperless_local,immich_local,jellyfin_lan,paperless_lan,immich_lan,lan_ip' }

$state = ((wsl.exe -l -v | Out-String) -replace "`0", '' -split "`n" | Where-Object { $_ -match 'konak-podman' }) -replace '^\*?\s*konak-podman\s+(\w+).*$', '$1'
$ip = (Get-NetIPAddress -AddressFamily IPv4 | Where-Object {
        $_.PrefixOrigin -in 'Dhcp','Manual' -and $_.InterfaceAlias -notmatch 'vEthernet|VMware|VirtualBox|Loopback'
      } | Select-Object -First 1).IPAddress

function Code($url, $resolve) {
    $curlArgs = @('-s','-o','NUL','-w','%{http_code}','--max-time','4')
    if ($resolve) { $curlArgs += @('--resolve', $resolve) }
    & curl.exe @curlArgs $url
}
$apps = @(@('jellyfin',8096,'/health'),@('paperless',8000,'/'),@('immich',2283,'/api/server/ping'))
$local = foreach ($a in $apps) { Code "http://localhost:$($a[1])$($a[2])" $null }
$lan = foreach ($a in $apps) { if ($ip) { Code "http://konak-$($a[0]).local$($a[2])" "konak-$($a[0]).local:80:$ip" } else { 'noip' } }
Add-Content $csv ((@((Get-Date -Format 'yyyy-MM-dd HH:mm:ss'), $state.Trim()) + $local + $lan + @($ip)) -join ',')
