//! Trvalé nastavení aplikace.
//!
//! Struktura je `#[non_exhaustive]` a mimo tenhle crate se dá vyrobit jen
//! přes [`AppSettings::default`] a metody `with_*`. Není to obřadnost:
//! na jiném projektu se nastavení skládalo pozičním konstruktorem a po
//! přidání pole ho každé uložení tiše vynulovalo — uživateli „mizela"
//! nastavená složka pro modely a pořád naskakoval úvodní průvodce.
//! S `..self` a `non_exhaustive` je tahle chyba nevyslovitelná.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::{InferenceSettings, ModelId, ModelRole};

/// Který model je aktivní v které roli.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleModels {
    #[serde(default)]
    pub coding: Option<ModelId>,
    #[serde(default)]
    pub conversational: Option<ModelId>,
}

impl RoleModels {
    pub fn get(&self, role: ModelRole) -> Option<&ModelId> {
        match role {
            ModelRole::Coding => self.coding.as_ref(),
            ModelRole::Conversational => self.conversational.as_ref(),
        }
    }

    pub fn set(mut self, role: ModelRole, id: Option<ModelId>) -> Self {
        match role {
            ModelRole::Coding => self.coding = id,
            ModelRole::Conversational => self.conversational = id,
        }
        self
    }

    /// Všechny nastavené modely — pro kontrolu, jestli je vůbec co načíst.
    pub fn any(&self) -> bool {
        self.coding.is_some() || self.conversational.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AppSettings {
    /// Kam se stahují modely. `None` = výchozí složka podle platformy,
    /// kterou dosadí infrastruktura.
    #[serde(default)]
    pub models_directory: Option<PathBuf>,

    #[serde(default)]
    pub models: RoleModels,

    /// Role, ve které aplikace právě běží.
    #[serde(default = "AppSettings::default_role")]
    pub active_role: ModelRole,

    #[serde(default)]
    pub inference: InferenceSettings,

    /// Naposledy otevřená složka projektu — nabídne se při startu.
    #[serde(default)]
    pub last_workspace: Option<PathBuf>,

    /// Úvodní průvodce proběhl. Bez toho by naskakoval po každém startu.
    #[serde(default)]
    pub setup_completed: bool,
}

impl AppSettings {
    fn default_role() -> ModelRole {
        ModelRole::Coding
    }

    pub fn with_models_directory(self, dir: Option<PathBuf>) -> Self {
        Self {
            models_directory: dir.filter(|d| !d.as_os_str().is_empty()),
            ..self
        }
    }

    pub fn with_model(self, role: ModelRole, id: Option<ModelId>) -> Self {
        Self {
            models: self.models.clone().set(role, id),
            ..self
        }
    }

    pub fn with_active_role(self, role: ModelRole) -> Self {
        Self {
            active_role: role,
            ..self
        }
    }

    pub fn with_inference(self, inference: InferenceSettings) -> Self {
        Self { inference, ..self }
    }

    pub fn with_last_workspace(self, path: Option<PathBuf>) -> Self {
        Self {
            last_workspace: path,
            ..self
        }
    }

    pub fn with_setup_completed(self, done: bool) -> Self {
        Self {
            setup_completed: done,
            ..self
        }
    }

    /// Model aktivní v právě zvolené roli.
    pub fn active_model(&self) -> Option<&ModelId> {
        self.models.get(self.active_role)
    }
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            models_directory: None,
            models: RoleModels::default(),
            active_role: Self::default_role(),
            inference: InferenceSettings::default(),
            last_workspace: None,
            setup_completed: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> ModelId {
        ModelId::parse(s).unwrap()
    }

    #[test]
    fn zmena_jednoho_pole_nesmaze_ostatni() {
        // Přesně ta chyba, kvůli které je struktura non_exhaustive.
        let puvodni = AppSettings::default()
            .with_models_directory(Some(PathBuf::from("/data/models")))
            .with_model(ModelRole::Coding, Some(id("qwen3-coder")))
            .with_setup_completed(true);

        let po = puvodni.clone().with_active_role(ModelRole::Conversational);

        assert_eq!(po.models_directory, puvodni.models_directory);
        assert_eq!(po.models.coding, puvodni.models.coding);
        assert!(po.setup_completed);
        assert_eq!(po.active_role, ModelRole::Conversational);
    }

    #[test]
    fn role_maji_oddelene_sloty() {
        let s = AppSettings::default()
            .with_model(ModelRole::Coding, Some(id("qwen3-coder")))
            .with_model(ModelRole::Conversational, Some(id("gemma-4-26b")));

        assert_eq!(
            s.models.get(ModelRole::Coding).unwrap().as_str(),
            "qwen3-coder"
        );
        assert_eq!(
            s.models.get(ModelRole::Conversational).unwrap().as_str(),
            "gemma-4-26b"
        );
    }

    #[test]
    fn aktivni_model_jde_podle_aktivni_role() {
        let s = AppSettings::default()
            .with_model(ModelRole::Coding, Some(id("kod")))
            .with_model(ModelRole::Conversational, Some(id("cestina")))
            .with_active_role(ModelRole::Conversational);
        assert_eq!(s.active_model().unwrap().as_str(), "cestina");
    }

    #[test]
    fn prazdna_slozka_se_bere_jako_neurcena() {
        let s = AppSettings::default().with_models_directory(Some(PathBuf::from("")));
        assert_eq!(s.models_directory, None);
    }

    #[test]
    fn stary_soubor_bez_novych_poli_se_nacte() {
        // Nastavení uložené starší verzí nesmí po přidání pole shodit start.
        let s: AppSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, AppSettings::default());

        let s: AppSettings =
            serde_json::from_str(r#"{"models_directory":"/data/models"}"#).unwrap();
        assert_eq!(s.models_directory, Some(PathBuf::from("/data/models")));
        assert_eq!(s.active_role, ModelRole::Coding);
        assert!(!s.setup_completed);
    }

    #[test]
    fn nastaveni_prezije_kolecko_pres_json() {
        let s = AppSettings::default()
            .with_model(ModelRole::Coding, Some(id("qwen3-coder")))
            .with_inference(InferenceSettings::default().with_context(32_768))
            .with_last_workspace(Some(PathBuf::from("/projects/anvil")))
            .with_setup_completed(true);

        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<AppSettings>(&json).unwrap(), s);
    }

    #[test]
    fn odebrani_modelu_z_role_projde() {
        let s = AppSettings::default()
            .with_model(ModelRole::Coding, Some(id("kod")))
            .with_model(ModelRole::Coding, None);
        assert!(!s.models.any());
    }
}
