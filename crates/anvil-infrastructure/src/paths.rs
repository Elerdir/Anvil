//! Kde má aplikace co na disku.
//!
//! Windows a macOS mají pro nastavení, data a logy různé konvence a Anvil
//! je respektuje — nikde se nestaví cesta ručně z `%APPDATA%` ani z `~`.

use std::path::{Path, PathBuf};

use directories::{ProjectDirs, UserDirs};

/// Název aplikace, pod kterým vznikají adresáře i položky v keychainu.
pub const APP_NAME: &str = "Anvil";

fn project_dirs() -> Option<ProjectDirs> {
    // Prázdné qualifier a organization dávají na Windows `%APPDATA%\Anvil`
    // a na macOS `~/Library/Application Support/Anvil` — tedy přesně to,
    // co uživatel na dané platformě čeká.
    ProjectDirs::from("", "", APP_NAME)
}

/// Nastavení (`settings.json`).
pub fn config_dir() -> PathBuf {
    project_dirs()
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| fallback_dir().join("config"))
}

/// Data aplikace — historie konverzací, výchozí složka pro modely.
pub fn data_dir() -> PathBuf {
    project_dirs()
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| fallback_dir().join("data"))
}

/// Logy.
pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}

/// Výchozí složka pro modely, když si uživatel nezvolil vlastní.
pub fn default_models_dir() -> PathBuf {
    data_dir().join("models")
}

/// Když se ani domovský adresář nepodaří zjistit, je něco hodně špatně —
/// ale spadnout kvůli tomu při startu je horší než pracovat v dočasné složce.
fn fallback_dir() -> PathBuf {
    std::env::temp_dir().join(APP_NAME)
}

/// Kde všude hledat už stažené modely, v pořadí priority.
///
/// Smysl je jediný a velmi praktický: GGUF soubory mají 15–20 GB a uživatel
/// jich obvykle pár má z jiných aplikací. Stáhnout znovu to, co už na disku
/// leží, je hodina čekání za nic.
///
/// Seznam po `configured` a výchozí složce je **heuristika** — konvenční
/// místa, kam modely odkládají jiné nástroje. Neexistující se vyfiltrují,
/// takže na stroji, kde nejsou, nic nestojí.
pub fn model_search_paths(configured: Option<&Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();

    if let Some(dir) = configured {
        out.push(dir.to_path_buf());
    }
    out.push(default_models_dir());

    // Standardní cache HuggingFace Hubu — stejná na obou platformách.
    if let Some(home) = UserDirs::new().map(|d| d.home_dir().to_path_buf()) {
        out.push(home.join(".cache").join("huggingface").join("hub"));
        out.push(home.join("models"));
    }

    out.extend(conventional_collections());

    // Zachovat pořadí, ale zahodit duplicity a to, co neexistuje.
    let mut videne = std::collections::HashSet::new();
    out.retain(|p| videne.insert(p.clone()) && p.is_dir());
    out
}

#[cfg(windows)]
fn conventional_collections() -> Vec<PathBuf> {
    // Modely se kvůli velikosti běžně odkládají na jiný disk než systémový.
    // Prohledat pár obvyklých míst je levné — neexistující se stejně
    // vyfiltrují — a ušetří to opakované stahování desítek gigabajtů.
    let mut out = Vec::new();
    for disk in ['C', 'D', 'E', 'F'] {
        out.push(PathBuf::from(format!("{disk}:\\models")));
        out.push(PathBuf::from(format!("{disk}:\\Models")));
    }
    out
}

#[cfg(not(windows))]
fn conventional_collections() -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from("/opt/models")];
    if let Some(home) = UserDirs::new().map(|d| d.home_dir().to_path_buf()) {
        // Obvyklá místa na macOS.
        out.push(
            home.join("Library")
                .join("Application Support")
                .join("models"),
        );
        out.push(home.join("Documents").join("models"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adresare_jsou_absolutni() {
        for dir in [config_dir(), data_dir(), log_dir(), default_models_dir()] {
            assert!(dir.is_absolute(), "{} není absolutní", dir.display());
        }
    }

    #[test]
    fn logy_lezi_pod_daty() {
        assert!(log_dir().starts_with(data_dir()));
    }

    #[test]
    fn nastavena_slozka_ma_prednost() {
        let tmp = tempfile::tempdir().unwrap();
        let cesty = model_search_paths(Some(tmp.path()));
        assert_eq!(
            cesty.first().map(PathBuf::as_path),
            Some(tmp.path()),
            "zvolená složka musí být první v pořadí"
        );
    }

    #[test]
    fn hledani_vraci_jen_existujici_slozky() {
        for p in model_search_paths(None) {
            assert!(p.is_dir(), "{} neexistuje a nemá se vracet", p.display());
        }
    }

    #[test]
    fn hledani_nevraci_duplicity() {
        let tmp = tempfile::tempdir().unwrap();
        // Tatáž složka jako zvolená i jako „konvenční" nesmí vyjít dvakrát.
        let cesty = model_search_paths(Some(tmp.path()));
        let mut unikatni = cesty.clone();
        unikatni.sort();
        unikatni.dedup();
        assert_eq!(unikatni.len(), cesty.len());
    }

    #[test]
    fn nazev_aplikace_je_v_ceste() {
        // Kdyby se změnil, uživatel přijde o nastavení — má to být vědomý krok.
        assert!(config_dir().to_string_lossy().contains(APP_NAME));
    }
}
