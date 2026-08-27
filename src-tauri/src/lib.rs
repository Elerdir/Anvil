//! Prezentační vrstva — Tauri okno, příkazy a start aplikace.

mod commands;
mod logging;
mod state;

use anvil_domain::workspace::Workspace;
use state::AppState;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _log_guard = logging::init();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // Bez tohohle je log po startu prázdný a při hlášení problému
            // z něj nejde poznat ani to, jestli má build engine.
            tracing::info!(
                verze = env!("CARGO_PKG_VERSION"),
                platforma = std::env::consts::OS,
                engine = cfg!(feature = "engine"),
                "Anvil startuje"
            );
            if !cfg!(feature = "engine") {
                tracing::warn!(
                    "Build je bez llama.cpp — model se nenačte.                      Spusť aplikaci přes run.bat (Windows) nebo scripts/run-mac.sh (macOS)."
                );
            }

            // Otevření databáze je asynchronní; `setup` běží dřív, než se
            // rozjede smyčka událostí, takže se na něj tady čeká.
            app.manage(tauri::async_runtime::block_on(AppState::new()));

            // Naposledy otevřenou složku obnovíme na pozadí, ať se okno
            // neukáže až po sáhnutí na disk.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = restore_last_workspace(&handle).await {
                    tracing::warn!(error = %e, "Poslední workspace se nepodařilo obnovit");
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_settings,
            commands::save_settings,
            commands::save_hf_token,
            commands::clear_hf_token,
            commands::list_models,
            commands::ensure_model,
            commands::load_model,
            commands::unload_model,
            commands::set_workspace,
            commands::get_session,
            commands::new_conversation,
            commands::list_conversations,
            commands::open_conversation,
            commands::rename_conversation,
            commands::pin_conversation,
            commands::reorder_conversations,
            commands::delete_conversation,
            commands::send_message,
            commands::cancel_generation,
            commands::pending_edits,
            commands::apply_edits,
            commands::discard_edits,
            commands::branch_conversation,
            commands::branch_before_message,
            commands::run_review,
            commands::list_tools,
        ])
        .run(tauri::generate_context!())
        .expect("Tauri se nepodařilo spustit");
}

/// Otevře složku, se kterou se pracovalo naposledy — pokud pořád existuje.
/// Když ji uživatel mezitím přesunul, tiše se přeskočí; hlásit to při startu
/// by bylo otravnější než užitečné.
async fn restore_last_workspace(app: &tauri::AppHandle) -> anvil_domain::error::DomainResult<()> {
    let state = app.state::<AppState>();
    let settings = state.settings_store.load().await?;
    let Some(path) = settings.last_workspace else {
        return Ok(());
    };
    if !path.is_dir() {
        tracing::info!(path = %path.display(), "Poslední workspace už neexistuje");
        return Ok(());
    }
    state.session.lock().await.workspace = Some(Workspace::new(path)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Pojistka proti chybě, která tuhle appku připravila o schopnost načíst
    /// model: `engine-vulkan` zapínalo `engine` jen v infrastruktuře, ne tady.
    /// Build s llama.cpp pak kompiloval záslepku a tvrdil uživateli, že je
    /// bez enginu.
    ///
    /// Kontroluje se manifest, ne `cfg!` — chyba je v propojení feature, takže
    /// by se běžným testem projevila jen v buildu, který llama.cpp skutečně
    /// staví, a ten na CI neběží.
    #[test]
    fn gpu_feature_zapina_i_mistni_engine() {
        let manifest = include_str!("../Cargo.toml");

        for feature in ["engine-vulkan", "engine-metal"] {
            let radek = manifest
                .lines()
                .map(str::trim)
                .find(|l| l.starts_with(feature) && l.contains('='))
                .unwrap_or_else(|| panic!("v Cargo.toml chybí feature {feature}"));

            assert!(
                radek.contains("\"engine\""),
                "{feature} musí zapnout i místní `engine`, jinak se přeloží                  llama.cpp, ale použije se záslepka. Řádek: {radek}"
            );
        }
    }

    /// Pole pro dotaz musí zůstat ukotvené dole.
    ///
    /// Mřížka `.main` má čtyři řádky a `1fr` patří konverzaci. Když jeden
    /// z prvků zmizí (`display: none`), zbylé se posunou o řádek výš, `1fr`
    /// spadne na `.dock`, ten se roztáhne přes zbytek okna a pole zůstane
    /// viset nahoře v něm. Uživatel to nahlásil **dvakrát**; podruhé kvůli
    /// `.notices:empty { display: none }`.
    ///
    /// Testuje se CSS textem, protože jinak to nemá kdo chytit: v prohlížeči
    /// se chyba maskuje tím, že tam Tauri příkazy selžou a vykreslí se
    /// chybový banner — a s neprázdným `.notices` je rozvržení správně.
    #[test]
    fn prvky_hlavni_mrizky_maji_pevne_radky() {
        // Komentáře pryč: test má posuzovat pravidla, ne text kolem nich.
        // Bez toho ho shodí i vysvětlení, proč tam něco nesmí být.
        let syrove = include_str!("../../src/styles/app.css");
        let mut css = String::with_capacity(syrove.len());
        let mut zbytek = syrove;
        while let Some(zacatek) = zbytek.find("/*") {
            css.push_str(&zbytek[..zacatek]);
            match zbytek[zacatek..].find("*/") {
                Some(konec) => zbytek = &zbytek[zacatek + konec + 2..],
                None => zbytek = "",
            }
        }
        css.push_str(zbytek);

        for (selektor, radek) in [
            (".topbar", "grid-row: 1"),
            (".notices", "grid-row: 2"),
            (".messages", "grid-row: 3"),
            (".dock", "grid-row: 4"),
        ] {
            let blok = css
                .split(&format!("\n{selektor} {{"))
                .nth(1)
                .unwrap_or_else(|| panic!("v app.css chybí pravidlo pro {selektor}"));
            let blok = blok.split('}').next().unwrap_or_default();
            assert!(
                blok.contains(radek),
                "{selektor} nemá `{radek}`. Bez pevného čísla řádku stačí, aby \
                 jeden prvek mřížky zmizel, a pole pro dotaz se odlepí ode dna."
            );
        }

        assert!(
            !css.contains(".notices:empty"),
            "`.notices:empty {{ display: none }}` vyřadí pruh oznámení z mřížky \
             a rozhodí řádky. Prázdný pruh nemá výšku ani rám, takže není co skrývat."
        );
    }

    /// Když je engine přeložený v infrastruktuře, musí ho vidět i tenhle crate.
    /// Doplňuje kontrolu manifestu o skutečný stav překladu.
    #[test]
    fn engine_sedi_s_infrastrukturou() {
        assert_eq!(
            anvil_infrastructure::ai::ENGINE_COMPILED,
            cfg!(feature = "engine"),
            "feature `engine` se rozešla mezi src-tauri a anvil-infrastructure"
        );
    }
}
