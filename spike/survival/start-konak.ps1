# Spike step 3 supervisor stub. Runs at logon and on resume from sleep; safe to run repeatedly.
# Starts the konak-podman distro with a keepalive, then Caddy and the mDNS responder on the LAN IP.
$ErrorActionPreference = 'Continue'
$base = "$env:LOCALAPPDATA\Konak-spike"
$bin = "$base\bin"
$mdnsExe = 'J:\Dev\self_hosting_project\spike\mdns-responder\target\release\mdns-responder.exe'
$log = "$base\supervisor.log"
function Log($m) { Add-Content $log ("{0:yyyy-MM-dd HH:mm:ss} {1}" -f (Get-Date), $m) }

# 1. Keepalive: WSL stops an idle distro ~100 s after its last session closes.
$keepalive = Get-CimInstance Win32_Process -Filter "Name='wsl.exe'" | Where-Object { $_.CommandLine -like '*konak-podman*sleep infinity*' }
if (-not $keepalive) {
    Start-Process -WindowStyle Hidden wsl.exe -ArgumentList '-d','konak-podman','-u','root','--','sleep','infinity'
    Log 'keepalive started'
}

# 2. LAN IP of the physical Ethernet/Wi-Fi adapter (never VMware, WSL or Hyper-V adapters).
$ip = (Get-NetIPAddress -AddressFamily IPv4 | Where-Object {
        $_.PrefixOrigin -in 'Dhcp','Manual' -and $_.InterfaceAlias -notmatch 'vEthernet|VMware|VirtualBox|Loopback'
      } | Select-Object -First 1).IPAddress
if (-not $ip) { Log 'no LAN IP yet'; exit 0 }

# 3. Caddy on the LAN IP; restart it if the IP changed.
$caddyfile = "$bin\Caddyfile.lan"
$c = "{`n  admin off`n  auto_https off`n}`n"
foreach ($p in @(@('jellyfin',8096),@('paperless',8000),@('immich',2283))) {
    $c += "http://konak-$($p[0]).local:80 {`n  bind $ip`n  reverse_proxy localhost:$($p[1])`n}`n"
}
$old = if (Test-Path $caddyfile) { [IO.File]::ReadAllText($caddyfile) } else { '' }
$caddy = Get-Process caddy -ErrorAction SilentlyContinue
if ($old -ne $c -or -not $caddy) {
    [IO.File]::WriteAllText($caddyfile, $c)
    $caddy | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Process "$bin\caddy.exe" -ArgumentList 'run','--config',$caddyfile,'--adapter','caddyfile' -WindowStyle Hidden -RedirectStandardError "$bin\caddy-lan.log"
    Log "caddy (re)started on $ip"
}

# 4. mDNS responder; restart it if the IP changed (it announces a fixed IP).
$ipFile = "$base\announced-ip.txt"
$announced = if (Test-Path $ipFile) { Get-Content $ipFile } else { '' }
$mdns = Get-Process mdns-responder -ErrorAction SilentlyContinue
if ($announced -ne $ip -or -not $mdns) {
    $mdns | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Process $mdnsExe -ArgumentList $ip,'86400','konak-jellyfin,konak-paperless,konak-immich' -WindowStyle Hidden -RedirectStandardOutput "$bin\mdns.log"
    Set-Content $ipFile $ip
    Log "mdns (re)started on $ip"
}
