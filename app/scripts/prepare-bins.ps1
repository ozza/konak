# Puts the Windows binaries Konak bundles into src-tauri/resources/bin (git-ignored):
#  - caddy.exe: Caddy v2.11.9 from the official release, SHA-512 checked against the release checksums.
#  - mdns-responder.exe: built from spike/mdns-responder.
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$bin = Join-Path $root 'src-tauri\resources\bin'
New-Item -ItemType Directory -Force $bin | Out-Null

$v = '2.11.9'
$zipName = "caddy_${v}_windows_amd64.zip"
$base = "https://github.com/caddyserver/caddy/releases/download/v$v"
$tmp = Join-Path $env:TEMP "konak-caddy-$v"
New-Item -ItemType Directory -Force $tmp | Out-Null
Invoke-WebRequest "$base/$zipName" -OutFile "$tmp\$zipName"
Invoke-WebRequest "$base/caddy_${v}_checksums.txt" -OutFile "$tmp\checksums.txt"
$expected = (Select-String -Path "$tmp\checksums.txt" -Pattern ([regex]::Escape($zipName))).Line.Split(' ')[0]
$actual = (Get-FileHash "$tmp\$zipName" -Algorithm SHA512).Hash.ToLower()
if ($expected -ne $actual) { throw "caddy checksum mismatch" }
Expand-Archive "$tmp\$zipName" -DestinationPath $tmp -Force
Copy-Item "$tmp\caddy.exe" $bin -Force

$crate = Join-Path (Split-Path $root -Parent) 'spike\mdns-responder'
# cargo reports progress on stderr, which Windows PowerShell 5.1 turns into errors under 'Stop'.
$ErrorActionPreference = 'Continue'
cargo build --release --quiet --manifest-path "$crate\Cargo.toml"
if ($LASTEXITCODE -ne 0) { throw "mdns-responder build failed" }
$ErrorActionPreference = 'Stop'
Copy-Item "$crate\target\release\mdns-responder.exe" $bin -Force
Get-ChildItem $bin | Select-Object Name, Length
