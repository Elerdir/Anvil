//! Nastavení v souboru `settings.json`.
//!
//! Zápis je atomický: nejdřív do `.tmp`, pak přejmenování. Přerušení uprostřed
//! (pád, výpadek proudu) tak nemůže nechat na disku půlku JSONu, po které by
//! aplikace příště nenaběhla.

use std::path::{Path, PathBuf};

use anvil_domain::{
    error::{DomainError, DomainResult},
    ports::SettingsStore,
    settings::AppSettings,
};
use async_trait::async_trait;

pub struct JsonSettingsStore {
    path: PathBuf,
}

impl JsonSettingsStore {
    /// Nastavení ve výchozím umístění podle platformy.
    pub fn new() -> Self {
        Self::at(crate::paths::config_dir().join("settings.json"))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Default for JsonSettingsStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SettingsStore for JsonSettingsStore {
    async fn load(&self) -> DomainResult<AppSettings> {
        let raw = match tokio::fs::read_to_string(&self.path).await {
            Ok(raw) => raw,
            // Při prvním spuštění soubor prostě není — to není chyba.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(AppSettings::default()),
            Err(e) => {
                return Err(DomainError::storage(format!(
                    "nelze přečíst {}: {e}",
                    self.path.display()
                )))
            }
        };

        serde_json::from_str(&raw).or_else(|e| {
            // Poškozený soubor nesmí zabránit startu. Odloží se stranou, ať
            // se dá podívat, co v něm bylo, a jede se s výchozím nastavením.
            tracing::error!(
                path = %self.path.display(),
                error = %e,
                "settings.json je poškozený — odkládám ho a startuji s výchozím nastavením"
            );
            let zaloha = self.path.with_extension("json.poskozeny");
            if let Err(e) = std::fs::rename(&self.path, &zaloha) {
                tracing::warn!(error = %e, "zálohu poškozeného nastavení se nepodařilo vytvořit");
            }
            Ok(AppSettings::default())
        })
    }

    async fn save(&self, settings: &AppSettings) -> DomainResult<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                DomainError::storage(format!("nelze vytvořit {}: {e}", parent.display()))
            })?;
        }

        let json = serde_json::to_string_pretty(settings)
            .map_err(|e| DomainError::storage(format!("nastavení nelze serializovat: {e}")))?;

        let docasny = self.path.with_extension("json.tmp");
        tokio::fs::write(&docasny, json.as_bytes())
            .await
            .map_err(|e| {
                DomainError::storage(format!("nelze zapsat {}: {e}", docasny.display()))
            })?;

        // `rename` přes existující soubor je na Windows chyba, na Unixu ne.
        // `tokio::fs::rename` mapuje na `std::fs::rename`, který si s tím na
        // Windows poradí (MoveFileEx s REPLACE_EXISTING), takže stačí jedno volání.
        tokio::fs::rename(&docasny, &self.path).await.map_err(|e| {
            DomainError::storage(format!(
                "nelze přesunout {} na {}: {e}",
                docasny.display(),
                self.path.display()
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use anvil_domain::model::{InferenceSettings, ModelId, ModelRole};

    use super::*;

    fn store() -> (tempfile::TempDir, JsonSettingsStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonSettingsStore::at(dir.path().join("settings.json"));
        (dir, store)
    }

    #[tokio::test]
    async fn chybejici_soubor_da_vychozi_nastaveni() {
        let (_d, s) = store();
        assert_eq!(s.load().await.unwrap(), AppSettings::default());
    }

    #[tokio::test]
    async fn ulozene_nastaveni_se_nacte_zpatky() {
        let (_d, s) = store();
        let nastaveni = AppSettings::default()
            .with_model(
                ModelRole::Coding,
                Some(ModelId::parse("qwen3-coder").unwrap()),
            )
            .with_inference(InferenceSettings::default().with_context(32_768))
            .with_setup_completed(true);

        s.save(&nastaveni).await.unwrap();
        assert_eq!(s.load().await.unwrap(), nastaveni);
    }

    #[tokio::test]
    async fn ulozeni_vytvori_chybejici_slozku() {
        let dir = tempfile::tempdir().unwrap();
        let s = JsonSettingsStore::at(dir.path().join("hloubka").join("a").join("settings.json"));
        s.save(&AppSettings::default()).await.unwrap();
        assert!(s.path().is_file());
    }

    #[tokio::test]
    async fn opakovane_ulozeni_prepise_puvodni() {
        // Přejmenování přes existující soubor se na Windows chová jinak než
        // na Unixu — tenhle test to hlídá na obou.
        let (_d, s) = store();
        s.save(&AppSettings::default()).await.unwrap();
        let druhe = AppSettings::default().with_setup_completed(true);
        s.save(&druhe).await.unwrap();
        assert_eq!(s.load().await.unwrap(), druhe);
    }

    #[tokio::test]
    async fn po_ulozeni_nezustane_docasny_soubor() {
        let (d, s) = store();
        s.save(&AppSettings::default()).await.unwrap();
        let zbytky: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(zbytky.is_empty(), "zůstaly dočasné soubory: {zbytky:?}");
    }

    #[tokio::test]
    async fn poskozeny_soubor_nezabrani_startu() {
        let (_d, s) = store();
        std::fs::write(s.path(), "{tohle není JSON").unwrap();

        // Musí naběhnout s výchozím nastavením...
        assert_eq!(s.load().await.unwrap(), AppSettings::default());
        // ...a původní soubor odložit, ať se dá zjistit, co v něm bylo.
        assert!(s.path().with_extension("json.poskozeny").is_file());
    }

    #[tokio::test]
    async fn nastaveni_z_novejsi_verze_se_nacte() {
        // Neznámé pole nesmí shodit načtení — jinak by downgrade appky
        // znamenal ztrátu celého nastavení.
        let (_d, s) = store();
        std::fs::write(
            s.path(),
            r#"{"setup_completed":true,"neznama_novinka":{"a":1}}"#,
        )
        .unwrap();
        assert!(s.load().await.unwrap().setup_completed);
    }
}
