# Jak se na Anvilu pracuje

## Postup

`main` je chráněná — nepřijímá přímé pushnutí. Každá změna jde přes větev
a pull request:

1. **Větev** z aktuálního `main`:

   ```bash
   git switch main && git pull
   git switch -c feat/nazev-zmeny
   ```

   Předpony: `feat/` nová funkčnost, `fix/` oprava, `chore/` údržba,
   `docs/` dokumentace.

2. **Práce a commity.** Zpráva commitu česky, imperativ, první řádek do
   72 znaků. Když commit řeší něco neintuitivního, patří důvod do těla —
   ne do kódu jako komentář „proč tohle je takhle" na pěti místech.

3. **Pull request** s auto-merge:

   ```bash
   gh pr create --fill
   gh pr merge --squash --auto --delete-branch
   ```

   Auto-merge počká, až budou všechny kontroly zelené, a pak PR sloučí sám.
   Když něco spadne, zůstane otevřený a čeká na opravu.

4. **Další část** se dělá zase z čerstvého `main`.

## Co musí projít

CI (`.github/workflows/ci.yml`) má čtyři úlohy a všechny jsou povinné:

| Úloha | Co ověřuje |
|---|---|
| `Rust — testy, formát, clippy` | `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` |
| `Engine (llama.cpp) se přeloží` | `cargo check --features engine` — chytí rozejití s API llama-cpp-2 |
| `Frontend — typy a build` | `tsc --noEmit` a `vite build` |
| `Tauri shell se přeloží` | `cargo check -p anvil` i s frontendem |

Lokálně to samé před odesláním:

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test && pnpm build
```

## Co CI neověří

**Nic, co potřebuje skutečný model.** Runner nemá 16 GB GGUF ani GPU, takže
engine se jen překládá. Chyby, které se projeví až na reálném modelu — špatně
poskládaný prompt, filtr, který sežere odpověď, offload horší než CPU — musí
odchytit ruční běh:

```bash
scripts\smoke.bat D:\models\model.gguf        # dvě kola konverzace + kontroly
scripts\prefill.bat D:\models\model.gguf      # rychlost zpracování promptu
```

Když se sahá na `ai/llama_engine.rs`, `ai/chat_template.rs`, `ai/offload_plan.rs`
nebo `ai/kv_reuse.rs`, pusť `smoke` **před** otevřením PR a výsledek napiš do
popisu PR. Tyhle soubory prošly jednotkovými testy i ve chvíli, kdy byly
prokazatelně špatně.

**macOS.** Zatím ho nikdo nespustil. Než se to změní, ber každou změnu
v `#[cfg]` větvích jako neověřenou.

## Zásady v kódu

- **Doména nesmí na I/O.** Žádné `tokio::fs`, `reqwest`, `sqlx` ani práce
  se souborovým systémem v `anvil-domain`.
- **Testy proti dvojníkům, ne proti modelu.** `ScriptedEngine`
  (`anvil-application::testing`) vrací skriptovaný scénář a zapisuje si, co
  dostal — díky tomu jde ověřit i chování při chybě, prázdné odpovědi a zrušení.
- **Nepožírat chyby.** Každý tichý `catch` loguje.
- **Komentář vysvětluje proč, ne co.** Když je řešení neintuitivní, patří
  k němu důvod — ideálně s číslem, které to rozhodlo.
- **Naměřené hodnoty patří do kódu s poznámkou, kde se vzaly.** Konstanta
  bez původu je za rok k nepoužití.

Podrobnosti o vrstvách a rozhodnutích: [ARCHITECTURE.md](ARCHITECTURE.md).
