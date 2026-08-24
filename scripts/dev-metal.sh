#!/usr/bin/env bash
# ========================================================================
#  Anvil — dev režim na macOS s akcelerací přes Metal.
#
#  Na rozdíl od Windows se tu nic nedoinstalovává: Metal je součást systému
#  a CMake přijde s Xcode Command Line Tools. Skript hlavně ověří, že je
#  prostředí kompletní, a spustí dev server se správnou feature.
#
#  Na Apple Silicon je paměť sjednocená, takže plánovač offloadu volí jinou
#  strategii než na Windows — všechno jde na GPU a experti se do RAM
#  nepřesouvají (viz crates/anvil-infrastructure/src/ai/offload_plan.rs).
# ========================================================================

set -euo pipefail
cd "$(dirname "$0")/.."

echo "=== Anvil dev (Metal) ==="
echo

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "[CHYBA] Tenhle skript je pro macOS. Na Windows použij scripts\\dev-vulkan.bat."
    exit 1
fi

# ------ Xcode Command Line Tools ------
if ! xcode-select -p >/dev/null 2>&1; then
    echo "[CHYBA] Chybí Xcode Command Line Tools. Nainstaluj je:"
    echo "  xcode-select --install"
    exit 1
fi
echo "[OK] Xcode CLT: $(xcode-select -p)"

# ------ CMake ------
if ! command -v cmake >/dev/null 2>&1; then
    echo "[CHYBA] cmake není v PATH — llama.cpp se bez něj nepřeloží."
    echo "  brew install cmake"
    exit 1
fi
echo "[OK] CMake: $(cmake --version | head -1)"

# ------ Rust ------
if ! command -v cargo >/dev/null 2>&1; then
    echo "[CHYBA] cargo není v PATH. Nainstaluj Rust: https://rustup.rs"
    exit 1
fi
echo "[OK] $(cargo --version)"

# ------ pnpm ------
if ! command -v pnpm >/dev/null 2>&1; then
    echo "[CHYBA] pnpm není v PATH."
    echo "  corepack enable && corepack prepare pnpm@latest --activate"
    exit 1
fi

if [[ "$(uname -m)" == "arm64" ]]; then
    RAM_GB=$(( $(sysctl -n hw.memsize) / 1024 / 1024 / 1024 ))
    echo "[INFO] Apple Silicon, ${RAM_GB} GB sjednocené paměti."
    if (( RAM_GB < 24 )); then
        echo "[INFO] Metal si vezme zhruba 75 % paměti. Doporučený model při"
        echo "       téhle velikosti je Qwen3-Coder v kvantu UD-Q3_K_XL (~13 GB)."
    fi
else
    echo "[INFO] Intel Mac — Metal funguje, ale bez sjednocené paměti bude"
    echo "       velký model výrazně pomalejší."
fi

echo
echo "=== Spouštím Tauri dev s --features engine-metal ==="
echo "První build llama.cpp trvá několik minut, další spuštění do minuty."
echo

exec pnpm tauri dev -- --features engine-metal
