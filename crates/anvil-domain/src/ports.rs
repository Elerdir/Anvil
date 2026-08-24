//! Porty — co aplikace potřebuje od okolního světa.
//!
//! Traity popisují *co*, ne *jak*. Implementace žijí v `anvil-infrastructure`,
//! testy aplikační vrstvy si dosazují vlastní dvojníky. Díky tomu jde celá
//! agentní smyčka testovat deterministicky, bez načteného modelu a bez disku.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    conversation::{Conversation, Message},
    error::DomainResult,
    history::ConversationSummary,
    id::ConversationId,
    model::{InferenceSettings, InstalledModel, ModelId, ModelRole, ModelSpec, Sampling},
};

// --- Generování textu ------------------------------------------------------

/// Jeden tah modelu.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    /// Systémová instrukce. Skládá se při každém tahu znovu, do historie
    /// se neukládá.
    pub system: Option<String>,
    /// Souhrn starších zpráv, pokud už proběhlo sloučení kontextu.
    /// Engine ho vloží před viditelné zprávy.
    pub summary: Option<String>,
    /// Viditelné zprávy konverzace v pořadí od nejstarší.
    pub messages: Vec<Message>,
    /// Horní mez délky odpovědi.
    pub max_tokens: u32,
    pub sampling: Sampling,
}

impl CompletionRequest {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            system: None,
            summary: None,
            messages,
            max_tokens: 2_048,
            sampling: Sampling::default(),
        }
    }

    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    pub fn with_summary(mut self, summary: Option<String>) -> Self {
        self.summary = summary;
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_sampling(mut self, sampling: Sampling) -> Self {
        self.sampling = sampling;
        self
    }
}

/// Výsledek tahu i s tím, co stál.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionOutcome {
    pub text: String,
    /// Kolik tokenů měl prompt (zpracování před prvním tokenem odpovědi).
    pub prompt_tokens: u32,
    pub generated_tokens: u32,
    /// Doba do prvního tokenu v milisekundách.
    pub time_to_first_token_ms: u64,
    /// Celková doba generování v milisekundách.
    pub total_ms: u64,
    /// `true`, když generování skončilo zrušením a text je jen část.
    #[serde(default)]
    pub cancelled: bool,
}

impl CompletionOutcome {
    /// Rychlost dekódování v tokenech za sekundu — bez času stráveného
    /// nad promptem, aby se dala srovnávat napříč délkami konverzace.
    pub fn decode_tokens_per_second(&self) -> f64 {
        let decode_ms = self.total_ms.saturating_sub(self.time_to_first_token_ms);
        if decode_ms == 0 || self.generated_tokens == 0 {
            return 0.0;
        }
        self.generated_tokens as f64 / (decode_ms as f64 / 1000.0)
    }
}

/// Průběžná aktualizace během streamovaného generování.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationProgress {
    /// Text, který právě přibyl.
    pub delta: String,
    /// Všechno vygenerované dosud.
    pub accumulated: String,
    pub token_count: u32,
}

pub type ProgressCallback = Arc<dyn Fn(GenerationProgress) + Send + Sync + 'static>;

/// Načtený model schopný odpovídat.
#[async_trait]
pub trait ChatEngine: Send + Sync {
    /// Vygeneruje odpověď. Když je `on_progress` vyplněné, volá se po každém
    /// tokenu. `cancel` se kontroluje průběžně — po zrušení vrátí to, co
    /// stihl, s `cancelled = true` (ne chybu, ta část textu je k něčemu).
    async fn complete(
        &self,
        request: &CompletionRequest,
        cancel: CancellationToken,
        on_progress: Option<ProgressCallback>,
    ) -> DomainResult<CompletionOutcome>;

    /// Počet tokenů textu podle tokenizeru **tohoto** modelu. Odhad ze znaků
    /// tu nestačí — na rozhodnutí o sloučení kontextu potřebujeme skutečné číslo.
    fn count_tokens(&self, text: &str) -> DomainResult<u32>;

    /// Velikost kontextového okna, se kterým byl model načten.
    fn context_tokens(&self) -> u32;

    fn model_id(&self) -> &ModelId;
}

/// Správa načtených modelů.
#[async_trait]
pub trait ChatEngineFactory: Send + Sync {
    /// Vrátí engine pro daný model. Instance se cachují — načtení 18GB modelu
    /// trvá desítky sekund, takže se nesmí opakovat pro každý dotaz.
    /// Engine s jiným `settings` je jiná instance (jiná stopa ve VRAM).
    async fn get(
        &self,
        model: &ModelId,
        settings: InferenceSettings,
    ) -> DomainResult<Arc<dyn ChatEngine>>;

    /// Uvolní načtené modely a s nimi paměť GPU. Volá se při přepnutí role —
    /// dva modely se do 8 GB VRAM nevejdou.
    async fn release(&self);
}

// --- Modely ---------------------------------------------------------------

/// Katalog modelů, které si uživatel může stáhnout.
pub trait ModelCatalog: Send + Sync {
    fn all(&self) -> Vec<ModelSpec>;

    fn find(&self, id: &ModelId) -> Option<ModelSpec> {
        self.all().into_iter().find(|m| &m.id == id)
    }

    fn for_role(&self, role: ModelRole) -> Vec<ModelSpec> {
        self.all().into_iter().filter(|m| m.role == role).collect()
    }

    /// Doporučená volba pro roli — to, co appka nabídne při prvním spuštění.
    fn recommended(&self, role: ModelRole) -> Option<ModelSpec> {
        self.for_role(role).into_iter().find(|m| m.recommended)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DownloadProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub bytes_per_second: f64,
}

impl DownloadProgress {
    pub fn percent(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        (self.downloaded_bytes as f64 / self.total_bytes as f64) * 100.0
    }

    /// Odhad zbývajícího času v sekundách. `None`, dokud není známá rychlost.
    pub fn eta_seconds(&self) -> Option<u64> {
        if self.bytes_per_second <= 0.0 {
            return None;
        }
        let zbyva = self.total_bytes.saturating_sub(self.downloaded_bytes);
        Some((zbyva as f64 / self.bytes_per_second) as u64)
    }
}

pub type DownloadCallback = Arc<dyn Fn(DownloadProgress) + Send + Sync + 'static>;

/// Zajištění modelu na disku — najít, případně zkopírovat nebo stáhnout.
#[async_trait]
pub trait ModelProvisioner: Send + Sync {
    /// Modely nalezené na disku, ať už ve zvolené složce nebo v místech,
    /// kam je odkládají jiné aplikace.
    async fn installed(&self) -> DomainResult<Vec<InstalledModel>>;

    /// Postará se, aby model byl v cílové složce a byl použitelný.
    /// Když už tam je, nic nedělá; když leží jinde, zkopíruje ho;
    /// jinak stáhne. Zdrojový soubor **nikdy** nemaže — může jít
    /// o uživatelovu sbírku.
    async fn ensure(
        &self,
        spec: &ModelSpec,
        cancel: CancellationToken,
        on_progress: Option<DownloadCallback>,
    ) -> DomainResult<InstalledModel>;
}

// --- Tajemství a nastavení ------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretKey {
    /// Token pro HuggingFace — potřeba jen pro modely za souhlasem s licencí.
    HuggingFace,
}

impl SecretKey {
    /// Stabilní název položky v systémovém úložišti. Přejmenování téhle
    /// konstanty znamená, že uživatel o uložený token přijde — pak je nutná
    /// migrace, ne prosté přepsání.
    pub fn entry_name(self) -> &'static str {
        match self {
            SecretKey::HuggingFace => "anvil.huggingface.token.v1",
        }
    }
}

/// Systémové úložiště hesel — Credential Manager, Keychain.
pub trait SecretStore: Send + Sync {
    fn get(&self, key: SecretKey) -> DomainResult<Option<String>>;
    fn set(&self, key: SecretKey, value: &str) -> DomainResult<()>;
    fn delete(&self, key: SecretKey) -> DomainResult<()>;
}

/// Ověření tokenu proti HuggingFace. Token se ukládá až po úspěchu, aby
/// v úložišti neležel překlep, o kterém se uživatel dozví až za hodinu
/// stahování.
#[async_trait]
pub trait TokenValidator: Send + Sync {
    /// Vrátí uživatelské jméno, ke kterému token patří.
    async fn validate_huggingface(&self, token: &str) -> DomainResult<String>;
}

/// Historie konverzací.
///
/// Seznam se načítá bez zpráv ([`ConversationSummary`]) — při startu by
/// nemělo smysl tahat do paměti celou historii kvůli tomu, aby se vlevo
/// vypsalo pár názvů.
#[async_trait]
pub trait ConversationStore: Send + Sync {
    /// Přehled konverzací seřazený k zobrazení (připnuté nahoře).
    async fn list(&self) -> DomainResult<Vec<ConversationSummary>>;

    async fn load(&self, id: ConversationId) -> DomainResult<Conversation>;

    /// Vloží nebo přepíše konverzaci i s jejími zprávami.
    async fn save(&self, conversation: &Conversation) -> DomainResult<()>;

    async fn rename(&self, id: ConversationId, title: &str) -> DomainResult<()>;

    async fn set_pinned(&self, id: ConversationId, pinned: bool) -> DomainResult<()>;

    /// Přepíše pořadí podle zadaného seznamu ID. Konverzace, které v seznamu
    /// nejsou, si své pořadí ponechají.
    async fn reorder(&self, ids: &[ConversationId]) -> DomainResult<()>;

    /// Smaže konverzaci. Smazání neexistující není chyba.
    async fn delete(&self, id: ConversationId) -> DomainResult<()>;
}

/// Trvalé nastavení aplikace.
#[async_trait]
pub trait SettingsStore: Send + Sync {
    async fn load(&self) -> DomainResult<crate::settings::AppSettings>;
    async fn save(&self, settings: &crate::settings::AppSettings) -> DomainResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn procenta_stahovani() {
        let p = DownloadProgress {
            downloaded_bytes: 250,
            total_bytes: 1000,
            bytes_per_second: 50.0,
        };
        assert_eq!(p.percent(), 25.0);
        assert_eq!(p.eta_seconds(), Some(15));
    }

    #[test]
    fn stahovani_bez_znameho_celku_nedeli_nulou() {
        let p = DownloadProgress {
            downloaded_bytes: 10,
            total_bytes: 0,
            bytes_per_second: 0.0,
        };
        assert_eq!(p.percent(), 0.0);
        assert_eq!(p.eta_seconds(), None);
    }

    #[test]
    fn rychlost_dekodovani_ignoruje_cas_promptu() {
        let o = CompletionOutcome {
            text: String::new(),
            prompt_tokens: 4000,
            generated_tokens: 100,
            time_to_first_token_ms: 2_000,
            total_ms: 7_000,
            cancelled: false,
        };
        // 100 tokenů za 5 s dekódování = 20 t/s; 2 s nad promptem se nepočítají.
        assert!((o.decode_tokens_per_second() - 20.0).abs() < 1e-9);
    }

    #[test]
    fn rychlost_bez_dekodovani_je_nula_a_ne_deleni_nulou() {
        let o = CompletionOutcome {
            text: String::new(),
            prompt_tokens: 10,
            generated_tokens: 0,
            time_to_first_token_ms: 500,
            total_ms: 500,
            cancelled: true,
        };
        assert_eq!(o.decode_tokens_per_second(), 0.0);
    }

    #[test]
    fn nazev_polozky_v_uloziste_je_verzovany() {
        // Pojistka proti tichému přejmenování — viz komentář u entry_name.
        assert_eq!(
            SecretKey::HuggingFace.entry_name(),
            "anvil.huggingface.token.v1"
        );
    }
}
