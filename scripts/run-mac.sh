#!/usr/bin/env bash
# ========================================================================
#  Anvil — spuštění aplikace na macOS bez instalátoru.
#
#  Protějšek run.bat: postaví release binárku s vestavěným frontendem
#  a spustí ji. Dev server není potřeba.
#
#  Použití:
#    scripts/run-mac.sh            spustí (a případně postaví)
#    scripts/run-mac.sh --rebuild  vynutí čistý build
# ========================================================================

set -euo pipefail
cd "$(dirname "$0")/.."

EXE="target/release/anvil"

echo "=== Anvil ==="
echo

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "[CHYBA] Tenhle skript je pro macOS. Na Windows použij run.bat."
    exit 1
fi

for nastroj in cmake cargo pnpm; do
    if ! command -v "$nastroj" >/dev/null 2>&1; then
        echo "[CHYBA] $nastroj není v PATH."
        case "$nastroj" in
            cmake) echo "  brew install cmake" ;;
            cargo) echo "  https://rustup.rs" ;;
            pnpm)  echo "  corepack enable && corepack prepare pnpm@latest --activate" ;;
        esac
        exit 1
    fi
done

if [[ "${1:-}" == "--rebuild" ]]; then
    echo "Vynucený čistý build…"
    rm -f "$EXE"
    cargo clean -p anvil >/dev/null 2>&1 || true
fi

if [[ ! -d node_modules ]]; then
    echo "Instaluji závislosti frontendu…"
    pnpm install
fi

# Staví se vždycky. Když se nic nezměnilo, je to otázka sekund, a je to
# levnější než riskovat, že poběží stará binárka.
if [[ ! -f "$EXE" ]]; then
    echo "První build — llama.cpp, počítej s několika minutami."
else
    echo "Kontroluji změny…"
fi
echo

pnpm tauri build --no-bundle -- --features engine-metal

if [[ ! -f "$EXE" ]]; then
    echo "[CHYBA] Binárka $EXE nevznikla."
    exit 1
fi

echo
echo "Spouštím $EXE"
exec "$EXE"
