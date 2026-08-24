//! Modely, jejich role v aplikaci a nastavení inference.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{DomainError, DomainResult};

/// Identifikátor modelu v katalogu. Stabilní, malými písmeny, bez mezer —
/// používá se i jako klíč v nastavení a jako název souboru na disku.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(String);

impl ModelId {
    pub fn parse(value: impl Into<String>) -> DomainResult<Self> {
        let value = value.into();
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(DomainError::validation("ID modelu nesmí být prázdné"));
        }
        if !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(DomainError::validation(format!(
                "ID modelu smí obsahovat jen písmena, číslice, '-', '_' a '.': {trimmed}"
            )));
        }
        Ok(Self(trimmed.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Role, kterou model v aplikaci zastává.
///
/// Modely dobré v programování a modely dobré v češtině jsou dnes dvě různé
/// množiny. Anvil proto drží dva sloty a nechá uživatele přepínat — aktivní
/// je vždy jen jeden, protože se oba naráz do paměti nevejdou.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    /// Generování a analýza kódu. Odpovědi jsou typicky anglicky.
    Coding,
    /// Konverzace, vysvětlování, čeština.
    Conversational,
}

impl ModelRole {
    pub const ALL: [ModelRole; 2] = [ModelRole::Coding, ModelRole::Conversational];

    pub fn label_cs(self) -> &'static str {
        match self {
            ModelRole::Coding => "programování",
            ModelRole::Conversational => "konverzace a čeština",
        }
    }
}

/// Jak se pro model skládá prompt.
///
/// Záměrně **nepoužíváme** `apply_chat_template` z GGUF metadat: u Gemmy 4
/// vrací `ffi error -1` i pro samotnou uživatelskou zprávu, takže by se stejně
/// musela obcházet. Explicitní výčet je navíc testovatelný bez načteného modelu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatTemplateKind {
    /// `<start_of_turn>role\n…<end_of_turn>`, odpověď musí být uvozená
    /// kanálem `final`, jinak model píše do kanálu `thought`.
    Gemma4,
    /// Hermes/ChatML varianta používaná řadou Qwen3 modelů.
    Qwen3,
    /// Obecné `<|im_start|>role\n…<|im_end|>`.
    ChatMl,
}

/// Záznam v katalogu — model, který si uživatel může stáhnout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelSpec {
    pub id: ModelId,
    /// Název do UI.
    pub name: String,
    /// K čemu se hodí a proč je v katalogu.
    pub description: String,
    pub role: ModelRole,
    /// HuggingFace repozitář, např. `unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF`.
    pub repo: String,
    /// Soubor v repozitáři včetně přípony.
    pub file: String,
    /// Velikost v bajtech. Před stažením se ověří proti `Content-Length`.
    pub size_bytes: u64,
    pub template: ChatTemplateKind,
    /// Vyžaduje přijetí licence na HuggingFace, tj. platný token.
    pub gated: bool,
    /// Výchozí volba pro svou roli.
    pub recommended: bool,
    /// Aktivní parametry v miliardách. U MoE je to ta část, která se počítá
    /// pro každý token — a právě ona rozhoduje o rychlosti, ne celková velikost.
    pub active_params_b: f32,
    /// Celkové parametry v miliardách.
    pub total_params_b: f32,
    /// Nativní kontextové okno modelu v tokenech.
    pub native_context_tokens: u32,
}

impl ModelSpec {
    /// Přímý odkaz na soubor v repozitáři.
    pub fn download_url(&self) -> String {
        format!(
            "https://huggingface.co/{}/resolve/main/{}",
            self.repo, self.file
        )
    }

    /// Název, pod kterým soubor leží ve složce s modely.
    pub fn local_filename(&self) -> &str {
        &self.file
    }

    /// Model je řídký (Mixture of Experts), pokud se aktivuje jen zlomek vah.
    /// Takové modely jsou na běžném stroji jediná použitelná cesta k velkým
    /// kvalitám — dense model nad 30B jede jednotky tokenů za sekundu.
    pub fn is_sparse(&self) -> bool {
        self.active_params_b < self.total_params_b * 0.5
    }
}

/// Model nalezený na disku.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledModel {
    pub id: ModelId,
    /// Absolutní cesta ke GGUF souboru.
    pub path: std::path::PathBuf,
    pub size_bytes: u64,
}

/// Nastavení běhu inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceSettings {
    /// Velikost kontextového okna v tokenech — drží prompt i odpověď.
    #[serde(default = "InferenceSettings::default_context")]
    pub context_tokens: u32,
    /// `false` vynutí běh na CPU. Na buildu bez GPU backendu nemá efekt.
    #[serde(default = "InferenceSettings::default_use_gpu")]
    pub use_gpu: bool,
    /// Počet vláken. `None` = odvodit z počtu fyzických jader.
    ///
    /// Fyzická, ne logická: na hybridních CPU (P + E jádra) drží E-jádra
    /// bariéru zpátky a víc vláken výsledek zhorší. Decode je stejně omezený
    /// propustností paměti, ne výpočtem.
    #[serde(default)]
    pub threads: Option<u32>,
}

impl InferenceSettings {
    /// Kód se čte po celých souborech, takže 4K jako u prózy nestačí.
    pub const DEFAULT_CONTEXT_TOKENS: u32 = 16_384;
    /// Pod 2K se nevejde ani systémový prompt s popisem nástrojů.
    pub const MIN_CONTEXT_TOKENS: u32 = 2_048;
    /// Strop pro UI. Vyšší okno znamená větší KV cache — na 8 GB VRAM
    /// se to projeví dřív, než dojde užitečnost.
    pub const MAX_CONTEXT_TOKENS: u32 = 131_072;

    fn default_context() -> u32 {
        Self::DEFAULT_CONTEXT_TOKENS
    }

    fn default_use_gpu() -> bool {
        true
    }

    /// Ořízne kontext do platného rozsahu.
    pub fn clamp_context(tokens: u32) -> u32 {
        tokens.clamp(Self::MIN_CONTEXT_TOKENS, Self::MAX_CONTEXT_TOKENS)
    }

    /// Vrátí kopii s oříznutým kontextem — jediná cesta, jak kontext nastavit
    /// zvenčí, aby se do enginu nedostala nesmyslná hodnota.
    pub fn with_context(self, tokens: u32) -> Self {
        Self {
            context_tokens: Self::clamp_context(tokens),
            ..self
        }
    }

    pub fn with_gpu(self, use_gpu: bool) -> Self {
        Self { use_gpu, ..self }
    }

    pub fn with_threads(self, threads: Option<u32>) -> Self {
        Self {
            threads: threads.filter(|t| *t > 0),
            ..self
        }
    }
}

impl Default for InferenceSettings {
    fn default() -> Self {
        Self {
            context_tokens: Self::DEFAULT_CONTEXT_TOKENS,
            use_gpu: true,
            threads: None,
        }
    }
}

/// Parametry samplování. Pro code review chceme nízkou teplotu — nálezy mají
/// být reprodukovatelné, ne kreativní.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sampling {
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: u32,
    pub repeat_penalty: f32,
}

impl Sampling {
    /// Pro analýzu kódu a volání nástrojů — co nejblíž deterministickému běhu.
    pub const PRECISE: Sampling = Sampling {
        temperature: 0.15,
        top_p: 0.9,
        top_k: 40,
        repeat_penalty: 1.05,
    };

    /// Pro vysvětlování a konverzaci.
    pub const BALANCED: Sampling = Sampling {
        temperature: 0.7,
        top_p: 0.95,
        top_k: 50,
        repeat_penalty: 1.1,
    };
}

impl Default for Sampling {
    fn default() -> Self {
        Self::PRECISE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(active: f32, total: f32) -> ModelSpec {
        ModelSpec {
            id: ModelId::parse("test").unwrap(),
            name: "Test".into(),
            description: String::new(),
            role: ModelRole::Coding,
            repo: "vendor/repo-GGUF".into(),
            file: "model-Q4_K_M.gguf".into(),
            size_bytes: 1,
            template: ChatTemplateKind::Qwen3,
            gated: false,
            recommended: false,
            active_params_b: active,
            total_params_b: total,
            native_context_tokens: 32_768,
        }
    }

    #[test]
    fn id_modelu_se_normalizuje() {
        assert_eq!(
            ModelId::parse("  Qwen3-Coder  ").unwrap().as_str(),
            "qwen3-coder"
        );
    }

    #[test]
    fn id_modelu_odmita_nesmysly() {
        assert!(ModelId::parse("").is_err());
        assert!(ModelId::parse("   ").is_err());
        assert!(ModelId::parse("model s mezerou").is_err());
        assert!(ModelId::parse("../../etc/passwd").is_err());
    }

    #[test]
    fn odkaz_ke_stazeni_miri_na_resolve_main() {
        assert_eq!(
            spec(3.0, 30.0).download_url(),
            "https://huggingface.co/vendor/repo-GGUF/resolve/main/model-Q4_K_M.gguf"
        );
    }

    #[test]
    fn ridkost_se_pozna_z_pomeru_parametru() {
        assert!(spec(3.3, 30.5).is_sparse(), "MoE má být řídký");
        assert!(!spec(32.0, 32.0).is_sparse(), "dense model řídký není");
    }

    #[test]
    fn kontext_se_orizne_do_rozsahu() {
        assert_eq!(
            InferenceSettings::clamp_context(10),
            InferenceSettings::MIN_CONTEXT_TOKENS
        );
        assert_eq!(
            InferenceSettings::clamp_context(u32::MAX),
            InferenceSettings::MAX_CONTEXT_TOKENS
        );
        assert_eq!(InferenceSettings::clamp_context(8_192), 8_192);
    }

    #[test]
    fn with_context_neobejde_orez() {
        let s = InferenceSettings::default().with_context(1);
        assert_eq!(s.context_tokens, InferenceSettings::MIN_CONTEXT_TOKENS);
    }

    #[test]
    fn nulova_vlakna_se_berou_jako_neurceno() {
        assert_eq!(
            InferenceSettings::default().with_threads(Some(0)).threads,
            None
        );
        assert_eq!(
            InferenceSettings::default().with_threads(Some(14)).threads,
            Some(14)
        );
    }

    #[test]
    fn nastaveni_inference_snese_chybejici_pole() {
        // Starší settings.json nesmí po přidání pole rozbít načtení.
        let s: InferenceSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, InferenceSettings::default());
    }
}
