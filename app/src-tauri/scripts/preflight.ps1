# Read-only checks before install; runs without admin and prints one JSON object.
# The LAN interface is the one holding the default IPv4 route (never vEthernet/WSL adapters).
$ErrorActionPreference = 'SilentlyContinue'
$route = Get-NetRoute -DestinationPrefix '0.0.0.0/0' | Where-Object { $_.InterfaceAlias -notmatch 'vEthernet|WSL|Loopback' } |
    Sort-Object RouteMetric | Select-Object -First 1
$profile = if ($route) { Get-NetConnectionProfile -InterfaceIndex $route.InterfaceIndex } else { $null }
$rx = (Get-NetAdapter -Physical | Get-NetAdapterStatistics | Measure-Object -Property ReceivedBytes -Sum).Sum
[pscustomobject]@{
    interface_index   = if ($route) { [int]$route.InterfaceIndex } else { 0 }
    interface_alias   = if ($route) { $route.InterfaceAlias } else { '' }
    network_category  = if ($profile) { [string]$profile.NetworkCategory } else { 'Unknown' }
    virtualization_fw = [bool](Get-CimInstance Win32_Processor | Select-Object -First 1).VirtualizationFirmwareEnabled
    hypervisor_present = [bool](Get-CimInstance Win32_ComputerSystem).HypervisorPresent
    rx_bytes          = [int64]$rx
} | ConvertTo-Json -Compress
