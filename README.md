# Blitzsuche

Ersetzt die Windows-Suche in der Taskleiste: Klick aufs Suchfeld oder `Win+S`
öffnet ein Flyout im Windows-11-Look, das jede Datei auf jedem Laufwerk findet.

- NTFS-Laufwerke: Dateitabelle direkt gelesen (FSCTL_ENUM_USN_DATA), danach
  live über das USN-Änderungsprotokoll. FAT32/exFAT: Ordnerscan beim Start.
- Apps wie im Startmenü (shell:AppsFolder).
- Läuft mit Adminrechten als Aufgabe „Blitzsuche“ (Autostart bei Anmeldung).
  Geöffnete Programme starten trotzdem mit normalen Rechten (über explorer.exe).

Tasten: Pfeile/Tab wählen, Enter öffnet, Strg+Enter öffnet den Speicherort,
Strg+Umschalt+Enter startet als Administrator, Esc schließt.

| Datei | Zweck |
|---|---|
| `deploy.sh` | neu bauen und die laufende Instanz ersetzen (ohne UAC) |
| `install.ps1` | Autostart-Aufgabe anlegen (einmalig, als Admin) |
| `uninstall.ps1` | Autostart entfernen und beenden (als Admin) |

Daten: `%LOCALAPPDATA%\Blitzsuche` (zuletzt geöffnet, Log, WebView-Cache).
