# Architektura Anvilu

Anvil je desktopová aplikace, která pouští jazykový model **lokálně** a pomáhá
s programováním nad vybranou složkou projektu. Nic neodchází na cloud.

## Vrstvy

Rust workspace v clean architecture, závislosti jdou jedním směrem:

```
┌────────────────────────────────────────────────────────┐
│  src-tauri            (presentation)                   │
│  Tauri příkazy, AppState, logování, události do UI     │
│        │ volá služby                                   │
│        ▼                                               │
│  anvil-application    (use cases)                       │
│  ChatService, CompactionService, systémové prompty      │
│        │ závisí na                                      │
│        ▼                                               │
│  anvil-domain         (jádro)                           │
│  entity, hodnotové objekty, PORTY (traity)              │
│        ▲ implementuje porty                             │
│        │                                                │
│  anvil-infrastructure (adaptéry)                        │
│  llama.cpp, HuggingFace, keychain, disk                 │
└────────────────────────────────────────────────────────┘
```

**Pravidlo domény:** žádné I/O. Nesmí tam být `tokio::fs`, `reqwest`, `sqlx`
ani práce se souborovým systémem. `async-trait` a `tokio-util` jsou jediná
výjimka — porty popisují asynchronní operace a jejich zrušení, ale samy nic
neprovádějí.

### `anvil-domain`

| Modul | Co obsahuje |
|---|---|
| `conversation` | `Conversation`, `Message`, `Role`, sloučení kontextu |
| `model` | `ModelSpec`, `ModelId`, `ModelRole`, `InferenceSettings`, `Sampling` |
| `workspace` | `Workspace`, `RelativePath` — **hranice sandboxu** |
| `settings` | `AppSettings` (`#[non_exhaustive]`, jen přes `with_*`) |
| `ports` | `ChatEngine`, `ModelProvisioner`, `SecretStore`, `SettingsStore`, … |

### `anvil-application`

- `chat` — `ChatService::send`: přidá dotaz, případně sloučí kontext, zavolá
  model, připojí odpověď
- `compaction` — kdy a co sloučit (`plan_compaction` je čistá funkce)
- `prompts` — systémové instrukce podle role a otevřené složky
- `testing` — `ScriptedEngine`, dvojník modelu se skriptovaným scénářem

### `anvil-infrastructure`

- `ai/offload_plan` — **co poslat na GPU a co nechat v RAM**
- `ai/gguf_meta` — čtení hlavičky GGUF (počet vrstev, expertů, rozměry attention)
- `ai/device_catalog` — detekce zařízení a sjednocené paměti
- `ai/chat_template` — skládání promptů a čištění výstupu podle rodiny modelu
- `ai/llama_engine` — `ChatEngine` nad llama.cpp
- `ai/model_downloader`, `ai/chunk_plan` — paralelní stahování s resume
- `model_provisioner` — najít / zkopírovat / stáhnout model
- `conversation_store` — historie v SQLite
- `secrets`, `settings_store`, `paths`, `huggingface`

## Rozhodnutí, která stojí za vysvětlení

### Rozhodují aktivní parametry, ne velikost modelu

Hustý model nad 30 B jede na běžném notebooku jednotky tokenů za sekundu
a je nepoužitelný. Řídký MoE s ~3 B aktivními parametry běží řádově rychleji
při srovnatelné kvalitě. Katalog proto obsahuje **jen řídké modely** a test
`vsechny_modely_jsou_ridke` hlídá, aby to tak zůstalo.

### Offload se plánuje, ne hádá

Naivní `-ngl 99` u modelu většího než VRAM skončí buď OOM, nebo (na Windows
přes WDDM) přetečením do RAM přes PCIe — a to je pomalejší než čisté CPU.
Naivní „offloadni N vrstev" je u MoE taky špatně: do VRAM se dostanou i experti,
kteří se pro každý token stejně mění.

`offload_plan::plan_offload` proto z velikosti souboru, GGUF metadat, volné
paměti a zvoleného kontextu spočítá jednu ze čtyř strategií:

| Plán | Kdy | Co udělá |
|---|---|---|
| `FullGpu` | model se vejde | všechny vrstvy na GPU |
| `HybridMoe` | MoE větší než VRAM | vrstvy na GPU, **tenzory expertů v RAM** |
| `UnifiedGpu` | Apple Silicon | všechno na GPU, žádné dělení |
| `PartialLayers` | hustý model větší než VRAM | tolik vrstev, kolik se vejde |
| `Cpu` | není GPU nebo je vypnutá | vše na CPU |

Naměřeno na Gemma 4 26B A4B (16 GB model / 8 GB karta): čisté CPU 11,2 tok/s,
naivní offload 12 vrstev **7,1** tok/s, hybrid **17,8** tok/s.

### Apple Silicon má vlastní větev

Na sjednocené paměti neexistuje sběrnice, přes kterou by se dalo přetéct.
„Přesunout experty do RAM" by tam nic nepřesunulo — jen přidalo přechod mezi
backendy na každý token. `MachineProfile::unified_memory` proto vede na
`UnifiedGpu` a `cpu_moe` i `op_offload = false` se tam **nikdy** nenastaví.
Kryto testem `sdilena_pamet_nikdy_nesahne_po_hybridnim_moe`.

### Šablony promptů se skládají ručně

`apply_chat_template` z GGUF u Gemmy 4 vrací `ffi error -1` i pro samotnou
uživatelskou zprávu. Obecný ChatML fallback model zmate natolik, že začne
odpovídat ve svém interním formátu. `chat_template` proto skládá tahy sám —
a jde to otestovat bez načteného modelu.

Prompt pro Gemmu **musí** končit `<|channel>final<channel|>`, jinak model píše
do kanálu `thought` a do odpovědi se nedostane nic.

### Filtry výstupu počítají s rozsekanými značkami

Tokenizer značku běžně rozdělí mezi dva tokeny (`<thi` + `nk>`). `TagStripper`
proto drží konec dávky zpátky, dokud si není jistý. Kdyby to nedělal, kus
značky by prosákl do textu a zbytek by se zahodil.

### Prompt se skládá celý znovu, ale nepočítá se celý znovu

Aplikační vrstva předá celou viditelnou konverzaci. Engine ji tokenizuje,
porovná s tím, co už v KV cache leží, shodný začátek zachová a dopočítá jen
zbytek (`ai::kv_reuse`).

Bez toho by aplikace nebyla použitelná. Na hybridním MoE běhu jede zpracování
promptu jen **~27 tokenů za sekundu** — tedy zhruba stejně rychle jako samotné
generování, protože experti jsou v RAM a počítá je CPU. Naměřeno na
gemma-4-26B-A4B / RTX 4070 Laptop 8 GB:

| tokenů promptu | zpracování |
|---|---|
| 1 069 | 39 s |
| 2 109 | 78 s |
| 4 124 | 154 s |

Kdyby se prompt počítal při každém tahu celý, znamenala by konverzace
s obsahem jednoho zdrojáku minuty čekání před **každou** odpovědí.

**Pozor na pořadí operací:** evidence toho, co v cache leží, se vyprazdňuje
*před* dekódováním a plní až po úspěchu. Kdyby dekódování selhalo v půlce,
zůstal by v evidenci stav, který v cache reálně není, a příští tah by přeskočil
tokeny, které se nikdy nespočítaly — model by odpovídal na prompt, který nikdy
neviděl.

První tah nad novým obsahem tu cenu zaplatí celou; to je limit, se kterým
je potřeba u code review počítat.

### Mřížka rozvržení nesmí spoléhat na pořadí prvků

`grid-template-rows` přiřazuje řádky podle pořadí potomků. Když se pruh
s chybou nevykreslil, posunul se seznam zpráv o řádek výš a `1fr` dostalo
pole pro dotaz — to pak viselo uprostřed okna místo dole.

Kontejnery `.notices` a `.dock` jsou proto v DOM **vždycky**, i prázdné.
Podmíněné je až to, co je uvnitř nich.

### Pořadí konverzací je explicitní číslo

Ne odvozené z času poslední zprávy: uživatel si seznam rovná a připíná podle
sebe a kdyby o pořadí rozhodovala aktivita, každá odpověď by mu ho zamíchala.
Přerovnání se posílá jako celý seznam ID a čísla se přepočítají od nuly —
u desítek konverzací je to levné a odpadá tím vkládání mezi dvě sousední
hodnoty.

### `AppSettings` nelze složit pozičně

Struktura je `#[non_exhaustive]` a mimo crate se vyrobí jen přes `default()`
a `with_*`. Na jiném projektu se nastavení skládalo pozičním konstruktorem
a po přidání pole ho každé uložení tiše vynulovalo — uživateli „mizela"
nastavená složka a pořád naskakoval úvodní průvodce. S `..self` je tahle chyba
nevyslovitelná.

### Sandbox je lexikální a testovatelný

`RelativePath::parse` je čistě lexikální kontrola — nesahá na disk, takže je
celá pokrytá jednotkovými testy a nezávisí na tom, co zrovna existuje. Odmítá
absolutní cesty, únik přes `..`, řídicí znaky, vyhrazené názvy zařízení
(`CON`, `NUL`, `COM1`…) a segmenty končící tečkou nebo mezerou.

Infrastruktura na to navazuje druhou obranou: kanonizací cesty, která chytí
symlinky.

## Feature flagy

```
cargo test                              # všechno kromě enginu, kdekoli
cargo test -p anvil-infrastructure --features engine        # + llama.cpp (CPU)
pnpm tauri dev -- --features engine-vulkan                  # Windows / Linux
pnpm tauri dev -- --features engine-metal                   # macOS
```

Engine je záměrně volitelný. Kdyby na llama.cpp viselo všechno, nešlo by pustit
`cargo test` bez pětiminutového buildu a připraveného prostředí (CMake, Vulkan
SDK) — a testy, které se nepouštějí, nikoho nechrání.

CUDA v seznamu není: o rychlosti u modelů větších než VRAM nerozhoduje backend,
ale rozložení modelu, a Vulkan pokrývá NVIDII, AMD i Intel jedním SDK.

### Odchylka od Erata: bez `crt-static`

Anvil **nepoužívá** statickou CRT. Ušetří to celou třídu build problémů
(`LLAMA_STATIC_CRT`, `CMAKE_POLICY_DEFAULT_CMP0091`, LNK2038) a Windows 10/11
má UCRT v systému. Cena je závislost na VC++ redistributable, který je na
cílových strojích prakticky vždy.

## Testovací strategie

| Vrstva | Jak se testuje |
|---|---|
| doména | čisté jednotkové testy, bez prostředí |
| aplikace | proti `ScriptedEngine` — deterministický scénář odpovědí |
| infrastruktura (logika) | jednotkové testy bez llama.cpp |
| infrastruktura (engine) | `cargo check --features engine` + ruční běh |
| frontend | `tsc --noEmit`, vitest |

`ScriptedEngine` je klíč: model je pomalý, nedeterministický a k načtení
potřebuje 16 GB soubor. Dvojník místo toho vrací předem daný scénář a zapisuje
si, co dostal — takže jde přesně ověřit, co bylo v promptu, kolikrát se model
volal a jak se aplikace zachovala při chybě, prázdné odpovědi i zrušení.
Od fáze 2 na tom stojí testy agentní smyčky.

Testy sahající na síť nebo systémový keychain jsou označené `#[ignore]`:

```
cargo test -p anvil-infrastructure -- --ignored
```

## Ikona

`tools/iconforge` je samostatný rustový generátor (tiny-skia). Z jednoho
popisu tvaru vypadne PNG sada, `icon.ico` i `icon.icns`. Běží stejně na Windows
i na macOS, takže k regeneraci není potřeba nic než toolchain:

```
pnpm icons
```

Barvy musí zůstat v souladu s `src/styles/theme.css`.
