# Entfernt den Autostart und beendet Blitzsuche. Muss als Administrator laufen.
Stop-ScheduledTask -TaskName 'Blitzsuche' -ErrorAction SilentlyContinue
Stop-Process -Name blitzsuche -Force -ErrorAction SilentlyContinue
Unregister-ScheduledTask -TaskName 'Blitzsuche' -Confirm:$false -ErrorAction SilentlyContinue
