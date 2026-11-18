#!/usr/bin/env bash
# Neu bauen und die laufende Blitzsuche ersetzen (kein UAC noetig, nutzt die Aufgabe).
set -e
cd "$(dirname "$0")"
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release
schtasks //End //TN Blitzsuche >/dev/null 2>&1 || true
for i in 1 2 3 4 5; do cp target/release/blitzsuche.exe bin/blitzsuche.exe 2>/dev/null && break; sleep 1; done
schtasks //Run //TN Blitzsuche >/dev/null
echo "Blitzsuche neu gestartet"
