//! Stav aplikace sdílený mezi Tauri příkazy.

use std::sync::Arc;

use anvil_application::{agent::tools::SharedPlan, ChatService};
use anvil_domain::{
    conversation::Conversation,
    error::{DomainError, DomainResult},
    model::{InstalledModel, ModelId, ModelSpec},
    ports::{
        ChatEngine, ConversationStore, ModelCatalog, ModelProvisioner, SecretKey, SecretStore,
        SettingsStore, TokenValidator,
    },
    settings::AppSettings,
    workspace::Workspace,
};
use anvil_infrastructure::{
    ai::model_catalog::StaticModelCatalog, conversation_store::SqliteConversationStore,
    huggingface::HuggingFaceClient, model_provisioner::FileSystemModelProvisioner, paths,
    secrets::KeyringSecretStore, settings_store::JsonSettingsStore,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// Co se mění za běhu. Za zámkem, protože k tomu sahá víc příkazů naráz.
#[derive(Default)]
pub struct Session {
    pub engine: Option<Arc<dyn ChatEngine>>,
    pub loaded_model: Option<ModelId>,
    /// Popis rozložení modelu mezi GPU a RAM — do stavového řádku.
    pub plan_description: Option<String>,
    pub conversation: Option<Conversation>,
    pub workspace: Option<Workspace>,
    /// Token právě běžícího generování nebo stahování.
    pub cancel: Option<CancellationToken>,
    /// Návrhy úprav, které čekají na schválení uživatelem.
    ///
    /// Drží se v session, ne v plánu na jeden tah: mezi návrhem a schválením
    /// může uživatel klidně napsat další zprávu a nesmí tím o návrhy přijít.
    pub edits: SharedPlan,
    /// Právě běží generování.
    ///
    /// Bez téhle pojistky by dvě souběžná odeslání ztratila historii:
    /// obě si konverzaci vyjmou ze session (druhé už dostane `None`, tedy
    /// prázdnou) a na konci ji obě zapíšou zpátky — vyhraje to, které skončí
    /// později. UI sice tlačítko během generování blokuje, ale spoléhat na to
    /// v příkazové vrstvě je špatně.
    pub generating: bool,
}

pub struct AppState {
    pub settings_store: Arc<dyn SettingsStore>,
    pub conversations: Arc<dyn ConversationStore>,
    pub secrets: Arc<dyn SecretStore>,
    pub catalog: Arc<dyn ModelCatalog>,
    pub validator: Arc<dyn TokenValidator>,
    pub chat: ChatService,
    pub session: Mutex<Session>,
}

impl AppState {
    /// Otevře úložiště historie a poskládá stav.
    ///
    /// Když se databáze otevřít nepodaří (plný disk, poškozený soubor),
    /// aplikace **nastartuje** s historií jen v paměti a hlasitě to zaloguje.
    /// Odmítnout start kvůli historii by uživatele připravilo i o to, co
    /// funguje.
    pub async fn new() -> Self {
        let cesta = paths::data_dir().join("history.db");
        let conversations: Arc<dyn ConversationStore> =
            match SqliteConversationStore::open(&cesta).await {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    tracing::error!(
                        path = %cesta.display(),
                        error = %e,
                        "Historii se nepodařilo otevřít — konverzace se tenhle běh neuloží"
                    );
                    Arc::new(
                        SqliteConversationStore::in_memory()
                            .await
                            .expect("paměťová databáze"),
                    )
                }
            };

        Self {
            settings_store: Arc::new(JsonSettingsStore::new()),
            conversations,
            secrets: Arc::new(KeyringSecretStore::new()),
            catalog: Arc::new(StaticModelCatalog),
            validator: Arc::new(HuggingFaceClient::new()),
            chat: ChatService::new(),
            session: Mutex::new(Session::default()),
        }
    }

    pub async fn settings(&self) -> DomainResult<AppSettings> {
        self.settings_store.load().await
    }

    /// Uloží nastavení. Čtení + úprava + zápis musí projít jedním místem,
    /// aby se nestalo, že jeden příkaz přepíše, co právě uložil jiný.
    pub async fn update_settings(
        &self,
        edit: impl FnOnce(AppSettings) -> AppSettings,
    ) -> DomainResult<AppSettings> {
        let aktualni = self.settings_store.load().await?;
        let nove = edit(aktualni);
        self.settings_store.save(&nove).await?;
        Ok(nove)
    }

    /// Provisioner poskládaný podle aktuálního nastavení a uloženého tokenu.
    pub async fn provisioner(&self) -> DomainResult<FileSystemModelProvisioner> {
        let settings = self.settings().await?;
        let cil = settings
            .models_directory
            .clone()
            .unwrap_or_else(paths::default_models_dir);
        let hledat = paths::model_search_paths(settings.models_directory.as_deref());

        let mut p = FileSystemModelProvisioner::new(cil, hledat, self.catalog.clone());
        // Token je volitelný — chybějící znamená jen to, že modely za
        // souhlasem s licencí nepůjdou stáhnout.
        if let Ok(Some(token)) = self.secrets.get(SecretKey::HuggingFace) {
            p.set_hf_token(Some(token));
        }
        Ok(p)
    }

    pub async fn installed_models(&self) -> DomainResult<Vec<InstalledModel>> {
        self.provisioner().await?.installed().await
    }

    pub fn find_spec(&self, id: &ModelId) -> DomainResult<ModelSpec> {
        self.catalog
            .find(id)
            .ok_or_else(|| DomainError::not_found(format!("model {id} není v katalogu")))
    }

    /// Zruší, co právě běží, a vrátí `true`, když bylo co rušit.
    pub async fn cancel_running(&self) -> bool {
        let mut session = self.session.lock().await;
        match session.cancel.take() {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// Nový token pro operaci, kterou půjde zrušit. Předchozí se ruší —
    /// dvě generování naráz by si přepisovala konverzaci.
    pub async fn begin_cancellable(&self) -> CancellationToken {
        let mut session = self.session.lock().await;
        if let Some(stary) = session.cancel.take() {
            stary.cancel();
        }
        let token = CancellationToken::new();
        session.cancel = Some(token.clone());
        token
    }

    pub async fn engine(&self) -> DomainResult<Arc<dyn ChatEngine>> {
        self.session
            .lock()
            .await
            .engine
            .clone()
            .ok_or_else(|| DomainError::not_found("není načtený žádný model"))
    }
}
