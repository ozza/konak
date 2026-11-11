# Removes the spike step 3 scheduled tasks. Logs in %LOCALAPPDATA%\Konak-spike are kept.
Get-ScheduledTask -TaskName 'Konak spike *' -ErrorAction SilentlyContinue | Unregister-ScheduledTask -Confirm:$false
Get-ScheduledTask -TaskName 'Konak spike *' -ErrorAction SilentlyContinue | Measure-Object | Select-Object Count
