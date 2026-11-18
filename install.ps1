# Richtet Blitzsuche als Autostart-Aufgabe mit hoechsten Rechten ein (ohne UAC bei jedem Start).
# Muss als Administrator laufen.
$ErrorActionPreference = 'Stop'
$exe  = Join-Path $PSScriptRoot 'bin\blitzsuche.exe'
$user = "$env:USERDOMAIN\$env:USERNAME"
$a = New-ScheduledTaskAction -Execute $exe -WorkingDirectory (Join-Path $PSScriptRoot 'bin')
$t = New-ScheduledTaskTrigger -AtLogOn -User $user
$p = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Highest
$s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
     -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -Priority 4 `
     -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
Register-ScheduledTask -TaskName 'Blitzsuche' -Action $a -Trigger $t -Principal $p -Settings $s -Force | Out-Null
Start-ScheduledTask -TaskName 'Blitzsuche'
'ok' | Set-Content (Join-Path $PSScriptRoot 'bin\installed.txt')
