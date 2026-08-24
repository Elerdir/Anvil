# Anvil

Desktopový asistent na programování, který **běží celý u tebe na stroji**.
Žádné API klíče do cloudu, žádné odesílání kódu ven — model se stáhne
z HuggingFace a počítá lokálně.

Windows a macOS (Apple Silicon i Intel).

## Co umí teď

- Chat s lokálním modelem, odpovědi česky
- **Dva sloty na modely** — jeden laděný na kód, druhý na češtinu, přepínatelné
- Výběr složky projektu rovnou nad polem pro dotaz
- Stahování modelů (8 spojení, resume po přerušení, ~10 MB/s místo 2)
- Modely už na disku se najdou a znovu nestahují
- **Automatické slučování kontextu** — délka konverzace nenaráží na limit okna
- Token HuggingFace v systémovém úložišti, ukládá se až po ověření

## Co se chystá

| Fáze | Co |
|---|---|
| 2 | Nástroje pro čtení souborů + agentní smyčka → **code review** |
| 3 | Historie konverzací v SQLite, větvení |
| 4 | Úpravy souborů s náhledem diffu a potvrzením |
| 5 | Vytvoření projektu od nuly |

## Spuštění

Potřebuješ [Rust](https://rustup.rs), [Node](https://nodejs.org) a `pnpm`.

### Nejjednodušší cesta

Dokud není instalátor, na tohle stačí jeden soubor:

```bash
run.bat
```

Na macOS `scripts/run-mac.sh`. Skript ověří prostředí, postaví release
binárku s vestavěným frontendem a spustí ji. První build trvá ~10 minut
(llama.cpp a Vulkan shadery), další spuštění jednotky sekund. `run.bat
--rebuild` vynutí čistý build.

Když Anvil už běží, skript to pozná a nabídne, že běžící instanci ukončí —
linker by jinak nepřepsal zamčenou binárku a cargo by hlásilo jen
nesrozumitelné „Přístup byl odepřen".

### Vývoj

```bash
pnpm install
```

#### Windows

Navíc [VS Build Tools](https://visualstudio.microsoft.com/downloads/?q=build+tools)
s workloadem „Desktop development with C++" a [Vulkan SDK](https://vulkan.lunarg.com/sdk/home).

```bash
scripts\dev-vulkan.bat
```

#### macOS

Stačí Xcode Command Line Tools a CMake (`brew install cmake`).

```bash
./scripts/dev-metal.sh
```

Oba skripty prostředí nejdřív ověří a řeknou, co případně chybí. První build
llama.cpp trvá několik minut, další spuštění do minuty.

#### Bez akcelerace

`pnpm tauri dev` se spustí, ale bez enginu — appka to řekne v pruhu nahoře
a model nenačte.

## Testy

```bash
cargo test
```

Běží kdekoli, bez CMake a bez llama.cpp — engine je za volitelnou feature.

```bash
cargo test -p anvil-infrastructure --features engine   # + build llama.cpp
cargo test -p anvil-infrastructure -- --ignored        # sahá na síť a keychain
pnpm build                                             # typecheck + build UI
```

## Modely

Katalog obsahuje jen **řídké (MoE) modely** — o rychlosti na běžném stroji
rozhodují aktivní parametry, ne celková velikost. Hustý model nad 30 B jede
jednotky tokenů za sekundu a je nepoužitelný.

| Model | Role | Velikost | Aktivních |
|---|---|---|---|
| Qwen3-Coder 30B-A3B | kód | 16,5 GB | 3,3 B z 30,5 B |
| Gemma 4 26B-A4B it | čeština | 15,8 GB | 4 B z 26 B |

Menší kvant Qwen3-Coderu (12,9 GB) je v katalogu pro stroje s 16 GB paměti.

Složku pro modely si vybereš v nastavení. Anvil hledá i ve složkách jiných
nástrojů a v cache HuggingFace Hubu, takže co už na disku máš, se nestahuje
znovu.

## Poznámka k očekáváním

Lokální model se ~3 miliardami aktivních parametrů není Claude. Naměřeno na
RTX 4070 Laptop 8 GB s Gemmou 4 26B-A4B:

| | |
|---|---|
| generování | ~20 tok/s |
| další tah konverzace | první token do 2 s |
| **první tah nad novým obsahem** | ~27 tok/s zpracování promptu |

To poslední je ten limit, o který jde. Otevřít model souboru o 4 000 tokenech
znamená **zhruba dvě a půl minuty**, než začne odpovídat — u hybridního běhu
(experti v RAM) je zpracování promptu stejně pomalé jako generování, což je
proti běžnému chování anomálie. Další tahy už jsou rychlé, protože si Anvil
zachovává prefix KV cache.

Prakticky: na code review a úpravy existujícího kódu to stačí, ale počítej
s tím, že první otázka nad velkým souborem chvíli trvá. U generování většího
celku od nuly počítej s dohledem.

## Dokumentace

- [ARCHITECTURE.md](ARCHITECTURE.md) — vrstvy, rozhodnutí a proč jsou taková
- [CONTRIBUTING.md](CONTRIBUTING.md) — postup přes větev a PR, co musí projít
