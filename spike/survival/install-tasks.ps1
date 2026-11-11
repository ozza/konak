# Registers the two spike step 3 tasks for the current user (no admin needed):
#  - "Konak spike supervisor": at logon and on resume from sleep, runs start-konak.ps1
#  - "Konak spike probe": at logon, then every minute, runs probe.ps1
# Remove them with uninstall-tasks.ps1.
$dir = 'J:\Dev\self_hosting_project\spike\survival'
$user = "$env:USERDOMAIN\$env:USERNAME"
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Minutes 5) -MultipleInstances IgnoreNew
$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited

function Action($script) {
    New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$dir\$script`""
}

# Resume from sleep: Power-Troubleshooter event 1 in the System log.
$cls = Get-CimClass -ClassName MSFT_TaskEventTrigger -Namespace Root/Microsoft/Windows/TaskScheduler
$resume = New-CimInstance -CimClass $cls -ClientOnly
$resume.Enabled = $true
$resume.Subscription = '<QueryList><Query Id="0" Path="System"><Select Path="System">*[System[Provider[@Name=''Microsoft-Windows-Power-Troubleshooter''] and EventID=1]]</Select></Query></QueryList>'

Register-ScheduledTask -TaskName 'Konak spike supervisor' -Action (Action 'start-konak.ps1') `
    -Trigger @((New-ScheduledTaskTrigger -AtLogOn -User $user), $resume) -Settings $settings -Principal $principal -Force | Out-Null

$every = New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(1) -RepetitionInterval (New-TimeSpan -Minutes 1)
Register-ScheduledTask -TaskName 'Konak spike probe' -Action (Action 'probe.ps1') `
    -Trigger @((New-ScheduledTaskTrigger -AtLogOn -User $user), $every) -Settings $settings -Principal $principal -Force | Out-Null

Get-ScheduledTask -TaskName 'Konak spike *' | Select-Object TaskName, State
