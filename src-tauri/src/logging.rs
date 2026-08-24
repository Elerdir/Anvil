//! Logování do konzole i do souboru.
//!
//! Zásada, která tu platí: **nepožírat chyby.** Každý tichý `catch` má
//! zalogovat. Bez toho se problém projeví až tím, že něco „nefunguje",
//! a hledá se od nuly.

use std::fs::OpenOptions;

use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

/// Kolik dní se logy drží.
const RETENTION_DAYS: i64 = 30;

/// Nastaví odběratele logu. Vrácenou hodnotu je nutné držet po celý běh —
/// s jejím zánikem se zavře soubor.
pub fn init() -> Option<std::fs::File> {
    // llama.cpp hlásí běžnou diagnostiku (velikosti bufferů, výpis tenzorů)
    // jako Warning. Kdyby se propouštěla, desítky řádků by schovaly skutečná
    // varování — proto se knihovní logy drží na `info` a výš jen z Anvilu.
    let filter = EnvFilter::try_from_env("ANVIL_LOG").unwrap_or_else(|_| {
        EnvFilter::new("info,anvil=debug,anvil_lib=debug,anvil_domain=debug,anvil_application=debug,anvil_infrastructure=debug,llama_cpp_2=warn")
    });

    let soubor = open_log_file();

    let konzole = fmt::layer()
        .with_target(true)
        .with_ansi(true)
        .with_filter(EnvFilter::new("info"));

    match soubor {
        Some(f) => {
            let kopie = f.try_clone().ok();
            let do_souboru = fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(f);
            tracing_subscriber::registry()
                .with(filter)
                .with(konzole)
                .with(do_souboru)
                .init();
            kopie
        }
        None => {
            tracing_subscriber::registry()
                .with(filter)
                .with(konzole)
                .init();
            None
        }
    }
}

fn open_log_file() -> Option<std::fs::File> {
    let dir = anvil_infrastructure::paths::log_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("Složku pro logy nelze vytvořit ({}): {e}", dir.display());
        return None;
    }

    prune_old_logs(&dir);

    let dnes = time::OffsetDateTime::now_utc().date();
    let cesta = dir.join(format!("anvil-{dnes}.log"));
    match OpenOptions::new().create(true).append(true).open(&cesta) {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("Log soubor nelze otevřít ({}): {e}", cesta.display());
            None
        }
    }
}

/// Smaže logy starší než [`RETENTION_DAYS`]. Selhání se jen vypíše —
/// neuklizený log není důvod nespustit aplikaci.
fn prune_old_logs(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let hranice = time::OffsetDateTime::now_utc() - time::Duration::days(RETENTION_DAYS);

    for entry in entries.flatten() {
        let path = entry.path();
        let je_nas_log = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("anvil-") && n.ends_with(".log"));
        if !je_nas_log {
            continue;
        }
        let stary = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|m| time::OffsetDateTime::from(m) < hranice)
            .unwrap_or(false);
        if stary {
            if let Err(e) = std::fs::remove_file(&path) {
                eprintln!("Starý log {} nelze smazat: {e}", path.display());
            }
        }
    }
}
