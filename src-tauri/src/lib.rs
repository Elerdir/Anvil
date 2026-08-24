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
            let state = AppState::new();
            app.manage(state);

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
            commands::send_message,
            commands::cancel_generation,
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
