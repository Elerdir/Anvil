//! Tauri příkazy — jediné místo, kde se frontend potkává s aplikační vrstvou.
//!
//! Příkazy samy nic nepočítají: převedou vstup, zavolají službu a výsledek
//! přeloží do tvaru, kterému rozumí UI. Logika patří do `anvil-application`,
//! kde jde otestovat bez okna.

use std::{path::PathBuf, str::FromStr, sync::Arc};

use anvil_application::{
    agent::runner::{AgentEvent, AgentHooks, AgentLoop},
    review::{empty_project_system, workspace_chat_system, ReviewService},
    TurnContext,
};
use anvil_domain::{
    conversation::Conversation,
    edit::DiffLine,
    error::{DomainError, DomainResult},
    history,
    id::{ConversationId, MessageId},
    model::{InferenceSettings, ModelId, ModelRole},
    ports::{ChatEngine, DownloadProgress, GenerationProgress, ModelProvisioner, SecretKey},
    review::Severity,
    tool::ToolSpec,
    workspace::{RelativePath, Workspace},
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use anvil_infrastructure::workspace_fs::LocalWorkspaceFs;

use crate::state::AppState;

// --- Chyby ----------------------------------------------------------------

/// Chyba ve tvaru, kterému rozumí frontend.
#[derive(Debug, Serialize)]
pub struct CommandError {
    pub message: String,
    /// Zrušení uživatelem — UI ho nemá hlásit jako chybu.
    pub cancelled: bool,
}

impl From<DomainError> for CommandError {
    fn from(e: DomainError) -> Self {
        Self {
            cancelled: e.is_cancelled(),
            message: e.to_string(),
        }
    }
}

type CommandResult<T> = Result<T, CommandError>;

// --- Pohledy pro UI -------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ModelView {
    pub id: String,
    pub name: String,
    pub description: String,
    pub role: String,
    pub size_bytes: u64,
    pub recommended: bool,
    pub installed: bool,
    pub gated: bool,
    pub active_params_b: f32,
    pub total_params_b: f32,
    pub native_context_tokens: u32,
}

#[derive(Debug, Serialize)]
pub struct MessageView {
    pub id: String,
    pub role: String,
    pub content: String,
    pub token_count: Option<u32>,
}

impl From<&anvil_domain::conversation::Message> for MessageView {
    fn from(m: &anvil_domain::conversation::Message) -> Self {
        Self {
            id: m.id.to_string(),
            role: match m.role {
                anvil_domain::conversation::Role::User => "user",
                anvil_domain::conversation::Role::Assistant => "assistant",
                anvil_domain::conversation::Role::Tool => "tool",
                anvil_domain::conversation::Role::System => "system",
            }
            .into(),
            content: m.content.clone(),
            token_count: m.token_count,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ConversationSummaryView {
    pub id: String,
    pub title: String,
    pub pinned: bool,
    pub message_count: u32,
    pub updated_at: String,
    /// Konverzace, ze které tahle vznikla — u větve. Název si UI dohledá
    /// v tomhle seznamu, takže ho není potřeba posílat zvlášť.
    pub parent_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SessionView {
    /// Otevrena konverzace. `None`, dokud zadna neni.
    pub conversation_id: Option<String>,
    /// Prave bezi generovani - postranni panel to ma u konverzace ukazat.
    pub generating: bool,
    pub loaded_model: Option<String>,
    pub plan_description: Option<String>,
    pub workspace_path: Option<String>,
    pub workspace_name: Option<String>,
    pub conversation_title: String,
    /// Rodič otevřené konverzace, když je to větev — kvůli odkazu zpátky
    /// do původního vlákna.
    pub parent_id: Option<String>,
    pub messages: Vec<MessageView>,
    /// Kolik z okna je zabráno viditelnými zprávami — pro ukazatel v UI.
    pub used_tokens: u32,
    pub context_tokens: u32,
    pub has_summary: bool,
    /// Build umí načíst model. Bez toho appka jede, ale jen jako prohlížeč.
    pub engine_available: bool,
}

#[derive(Debug, Serialize)]
pub struct SettingsView {
    pub models_directory: Option<String>,
    pub default_models_directory: String,
    pub coding_model: Option<String>,
    pub conversational_model: Option<String>,
    pub active_role: String,
    pub context_tokens: u32,
    pub use_gpu: bool,
    pub setup_completed: bool,
    pub has_hf_token: bool,
    pub last_workspace: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GenerationStats {
    pub prompt_tokens: u32,
    pub generated_tokens: u32,
    pub time_to_first_token_ms: u64,
    pub total_ms: u64,
    pub tokens_per_second: f64,
    pub cancelled: bool,
    /// Vyplněné, když se před odesláním slučoval kontext.
    pub compacted_messages: Option<usize>,
}

// --- Agent a review -------------------------------------------------------

/// Co se právě děje ve smyčce. UI z toho staví řádek „čte src/main.rs…“,
/// aby uživatel nekoukal minuty na prázdné okno.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEventView {
    Round {
        round: u32,
    },
    ToolCalled {
        name: String,
        summary: String,
    },
    ToolFinished {
        name: String,
        ok: bool,
    },
    Prose {
        text: String,
    },
    Step {
        done: u32,
        total: u32,
        label: String,
    },
}

impl From<AgentEvent> for AgentEventView {
    fn from(e: AgentEvent) -> Self {
        match e {
            AgentEvent::RoundStarted { round } => AgentEventView::Round { round },
            AgentEvent::ToolCalled { name, summary } => {
                AgentEventView::ToolCalled { name, summary }
            }
            AgentEvent::ToolFinished { name, ok } => AgentEventView::ToolFinished { name, ok },
            AgentEvent::Prose { text } => AgentEventView::Prose { text },
            AgentEvent::Step { done, total, label } => AgentEventView::Step { done, total, label },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct FindingView {
    pub file: String,
    pub line: Option<u32>,
    pub severity: String,
    pub summary: String,
    pub detail: String,
    pub location: String,
}

#[derive(Debug, Serialize)]
pub struct ReviewReportView {
    pub headline: String,
    pub findings: Vec<FindingView>,
    pub files_read: Vec<String>,
    pub rounds: u32,
    /// Kolik souborů projekt má. Bez toho vypadá „bez nálezu" po dvou
    /// prošlých souborech stejně jako po všech čtrnácti.
    pub files_total: u32,
    /// Skončilo se na limitu kol, ne proto, že model dokončil práci.
    pub hit_round_limit: bool,
    pub summary: String,
    pub total_ms: u64,
}

fn severity_key(s: Severity) -> &'static str {
    match s {
        Severity::Critical => "critical",
        Severity::Warning => "warning",
        Severity::Note => "note",
    }
}

// --- Nastavení ------------------------------------------------------------

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> CommandResult<SettingsView> {
    let s = state.settings().await?;
    Ok(SettingsView {
        models_directory: s.models_directory.as_ref().map(|p| p.display().to_string()),
        default_models_directory: anvil_infrastructure::paths::default_models_dir()
            .display()
            .to_string(),
        coding_model: s.models.coding.as_ref().map(ModelId::to_string),
        conversational_model: s.models.conversational.as_ref().map(ModelId::to_string),
        active_role: role_key(s.active_role).into(),
        context_tokens: s.inference.context_tokens,
        use_gpu: s.inference.use_gpu,
        setup_completed: s.setup_completed,
        has_hf_token: state
            .secrets
            .get(SecretKey::HuggingFace)
            .ok()
            .flatten()
            .is_some(),
        last_workspace: s.last_workspace.as_ref().map(|p| p.display().to_string()),
    })
}

#[derive(Debug, Deserialize)]
pub struct SettingsPatch {
    pub models_directory: Option<String>,
    pub coding_model: Option<String>,
    pub conversational_model: Option<String>,
    pub active_role: Option<String>,
    pub context_tokens: Option<u32>,
    pub use_gpu: Option<bool>,
    pub setup_completed: Option<bool>,
}

#[tauri::command]
pub async fn save_settings(
    state: State<'_, AppState>,
    patch: SettingsPatch,
) -> CommandResult<SettingsView> {
    // Modely se ověří proti katalogu dřív, než se cokoli uloží — neplatné ID
    // v nastavení by se projevilo až při příštím startu.
    let coding = parse_model(&state, patch.coding_model.as_deref())?;
    let conversational = parse_model(&state, patch.conversational_model.as_deref())?;

    state
        .update_settings(|mut s| {
            if let Some(dir) = &patch.models_directory {
                s = s.with_models_directory(Some(PathBuf::from(dir)));
            }
            if patch.coding_model.is_some() {
                s = s.with_model(ModelRole::Coding, coding.clone());
            }
            if patch.conversational_model.is_some() {
                s = s.with_model(ModelRole::Conversational, conversational.clone());
            }
            if let Some(role) = patch.active_role.as_deref().and_then(parse_role) {
                s = s.with_active_role(role);
            }
            let mut inference = s.inference;
            if let Some(ctx) = patch.context_tokens {
                inference = inference.with_context(ctx);
            }
            if let Some(gpu) = patch.use_gpu {
                inference = inference.with_gpu(gpu);
            }
            s = s.with_inference(inference);
            if let Some(done) = patch.setup_completed {
                s = s.with_setup_completed(done);
            }
            s
        })
        .await?;

    get_settings(state).await
}

fn parse_model(state: &State<'_, AppState>, raw: Option<&str>) -> DomainResult<Option<ModelId>> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let id = ModelId::parse(raw)?;
    state.find_spec(&id)?;
    Ok(Some(id))
}

fn parse_role(raw: &str) -> Option<ModelRole> {
    match raw {
        "coding" => Some(ModelRole::Coding),
        "conversational" => Some(ModelRole::Conversational),
        _ => None,
    }
}

fn role_key(role: ModelRole) -> &'static str {
    match role {
        ModelRole::Coding => "coding",
        ModelRole::Conversational => "conversational",
    }
}

// --- Token HuggingFace ----------------------------------------------------

/// Ověří token a teprve po úspěchu ho uloží. Uložit ho hned by znamenalo,
/// že se uživatel o překlepu dozví až v okamžiku, kdy mu selže stahování.
#[tauri::command]
pub async fn save_hf_token(state: State<'_, AppState>, token: String) -> CommandResult<String> {
    let jmeno = state.validator.validate_huggingface(&token).await?;
    state.secrets.set(SecretKey::HuggingFace, token.trim())?;
    tracing::info!(user = %jmeno, "Token HuggingFace ověřen a uložen");
    Ok(jmeno)
}

#[tauri::command]
pub async fn clear_hf_token(state: State<'_, AppState>) -> CommandResult<()> {
    state.secrets.delete(SecretKey::HuggingFace)?;
    Ok(())
}

// --- Modely ---------------------------------------------------------------

#[tauri::command]
pub async fn list_models(state: State<'_, AppState>) -> CommandResult<Vec<ModelView>> {
    let nainstalovane = state.installed_models().await?;
    Ok(state
        .catalog
        .all()
        .into_iter()
        .map(|m| ModelView {
            installed: nainstalovane.iter().any(|i| i.id == m.id),
            id: m.id.to_string(),
            name: m.name,
            description: m.description,
            role: role_key(m.role).into(),
            size_bytes: m.size_bytes,
            recommended: m.recommended,
            gated: m.gated,
            active_params_b: m.active_params_b,
            total_params_b: m.total_params_b,
            native_context_tokens: m.native_context_tokens,
        })
        .collect())
}

/// Postará se, aby model byl na disku — najde, zkopíruje nebo stáhne.
/// Průběh chodí událostí `download:progress`.
#[tauri::command]
pub async fn ensure_model(
    app: AppHandle,
    state: State<'_, AppState>,
    model_id: String,
) -> CommandResult<String> {
    let spec = state.find_spec(&ModelId::parse(model_id)?)?;
    let provisioner = state.provisioner().await?;
    let cancel = state.begin_cancellable().await;

    let hlasic = app.clone();
    let progress: anvil_domain::ports::DownloadCallback = Arc::new(move |p: DownloadProgress| {
        let _ = hlasic.emit("download:progress", &p);
    });

    let model = provisioner
        .ensure(&spec, cancel, Some(progress))
        .await
        .map_err(CommandError::from)?;

    Ok(model.path.display().to_string())
}

/// Načte model do paměti. Trvá desítky sekund, proto běží mimo hlavní vlákno.
#[tauri::command]
pub async fn load_model(
    state: State<'_, AppState>,
    model_id: String,
) -> CommandResult<SessionView> {
    let spec = state.find_spec(&ModelId::parse(model_id)?)?;
    let settings = state.settings().await?;

    let nainstalovane = state.installed_models().await?;
    let na_disku = nainstalovane
        .into_iter()
        .find(|m| m.id == spec.id)
        .ok_or_else(|| {
            DomainError::not_found(format!(
                "model {} není na disku — nejdřív ho stáhni",
                spec.name
            ))
        })?;

    // Starý model se pustí dřív, než se načte nový — dva se do VRAM nevejdou.
    {
        let mut session = state.session.lock().await;
        session.engine = None;
        session.loaded_model = None;
        session.plan_description = None;
    }

    let (engine, plan) = build_engine(spec.clone(), na_disku.path, settings.inference).await?;

    {
        let mut session = state.session.lock().await;
        session.engine = Some(engine);
        session.loaded_model = Some(spec.id.clone());
        session.plan_description = plan;
        if session.conversation.is_none() {
            session.conversation = Some(Conversation::new(""));
        }
    }

    session_view(&state).await
}

#[cfg(feature = "engine")]
async fn build_engine(
    spec: anvil_domain::model::ModelSpec,
    path: PathBuf,
    inference: InferenceSettings,
) -> DomainResult<(Arc<dyn ChatEngine>, Option<String>)> {
    use anvil_infrastructure::ai::llama_engine::LlamaChatEngine;

    tokio::task::spawn_blocking(move || {
        let engine = LlamaChatEngine::load(spec.id, &path, spec.template, inference)?;
        let plan = engine.plan_description().to_string();
        Ok::<_, DomainError>((Arc::new(engine) as Arc<dyn ChatEngine>, Some(plan)))
    })
    .await
    .map_err(|e| DomainError::model(format!("načítání modelu selhalo: {e}")))?
}

#[cfg(not(feature = "engine"))]
async fn build_engine(
    _spec: anvil_domain::model::ModelSpec,
    _path: PathBuf,
    _inference: InferenceSettings,
) -> DomainResult<(Arc<dyn ChatEngine>, Option<String>)> {
    Err(DomainError::model(
        "Tenhle build je bez enginu llama.cpp. Spusť aplikaci přes \
         scripts\\dev-vulkan.bat (Windows) nebo scripts/dev-metal.sh (macOS).",
    ))
}

#[tauri::command]
pub async fn unload_model(state: State<'_, AppState>) -> CommandResult<SessionView> {
    {
        let mut session = state.session.lock().await;
        session.engine = None;
        session.loaded_model = None;
        session.plan_description = None;
    }
    session_view(&state).await
}

// --- Workspace ------------------------------------------------------------

/// Otevře složku projektu. `None` ji zavře.
///
/// `create` založí chybějící složku. Zapíná se jen u „nového projektu“, kde
/// je vznik složky přesně to, o co uživatel požádal — u běžného otevření je
/// neexistující cesta překlep a aplikace se má ozvat, ne mlčky vyrobit
/// prázdnou složku někde vedle.
#[tauri::command]
pub async fn set_workspace(
    state: State<'_, AppState>,
    path: Option<String>,
    create: Option<bool>,
) -> CommandResult<SessionView> {
    let workspace = match path.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => {
            let cesta = PathBuf::from(p);
            if !cesta.exists() && create.unwrap_or(false) {
                std::fs::create_dir_all(&cesta).map_err(|e| {
                    DomainError::storage(format!("{} nejde založit: {e}", cesta.display()))
                })?;
            }
            if !cesta.is_dir() {
                return Err(DomainError::validation(format!(
                    "{} není složka nebo neexistuje",
                    cesta.display()
                ))
                .into());
            }
            // Kanonizace srovná `..`, krátké názvy i velikost písmen —
            // bez ní by se hlídání hranic workspace dalo obejít.
            let cesta = std::fs::canonicalize(&cesta).unwrap_or(cesta);
            Some(Workspace::new(cesta)?)
        }
        None => None,
    };

    let ulozit = workspace.as_ref().map(|w| w.root().to_path_buf());
    state
        .update_settings(|s| s.with_last_workspace(ulozit.clone()))
        .await?;

    state.session.lock().await.workspace = workspace;
    session_view(&state).await
}

// --- Konverzace -----------------------------------------------------------

#[tauri::command]
pub async fn get_session(state: State<'_, AppState>) -> CommandResult<SessionView> {
    session_view(&state).await
}

#[tauri::command]
pub async fn new_conversation(state: State<'_, AppState>) -> CommandResult<SessionView> {
    let existujici = state.conversations.list().await?;

    let mut nova = Conversation::new("Nova konverzace");
    nova.sort_order = history::order_for_new(&existujici);
    state.conversations.save(&nova).await?;

    state.session.lock().await.conversation = Some(nova);
    session_view(&state).await
}

// --- Historie -------------------------------------------------------------

#[tauri::command]
pub async fn list_conversations(
    state: State<'_, AppState>,
) -> CommandResult<Vec<ConversationSummaryView>> {
    Ok(state
        .conversations
        .list()
        .await?
        .into_iter()
        .map(|c| ConversationSummaryView {
            id: c.id.to_string(),
            title: c.title,
            pinned: c.pinned,
            message_count: c.message_count,
            updated_at: c
                .updated_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
            parent_id: c.parent_id.map(|p| p.to_string()),
        })
        .collect())
}

#[tauri::command]
pub async fn open_conversation(
    state: State<'_, AppState>,
    id: String,
) -> CommandResult<SessionView> {
    let id = parse_id(&id)?;

    // Rozepsanou konverzaci ulozit, nez se prepne - jinak by se ztratilo,
    // co uzivatel napsal tesne predtim.
    let otevrena = state.session.lock().await.conversation.clone();
    if let Some(otevrena) = otevrena {
        if let Err(e) = state.conversations.save(&otevrena).await {
            tracing::warn!(error = %e, "Predchozi konverzaci se nepodarilo ulozit");
        }
    }

    let nactena = state.conversations.load(id).await?;
    state.session.lock().await.conversation = Some(nactena);
    session_view(&state).await
}

#[tauri::command]
pub async fn rename_conversation(
    state: State<'_, AppState>,
    id: String,
    title: String,
) -> CommandResult<()> {
    let id = parse_id(&id)?;
    state.conversations.rename(id, &title).await?;

    // Kdyz je prejmenovana prave otevrena, musi se nazev srovnat i v pameti -
    // jinak by ho pristi ulozeni prepsalo zpatky.
    let mut session = state.session.lock().await;
    if let Some(c) = session.conversation.as_mut() {
        if c.id == id {
            c.title = title.trim().to_string();
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn pin_conversation(
    state: State<'_, AppState>,
    id: String,
    pinned: bool,
) -> CommandResult<()> {
    let id = parse_id(&id)?;
    state.conversations.set_pinned(id, pinned).await?;

    let mut session = state.session.lock().await;
    if let Some(c) = session.conversation.as_mut() {
        if c.id == id {
            c.pinned = pinned;
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn reorder_conversations(
    state: State<'_, AppState>,
    ids: Vec<String>,
) -> CommandResult<()> {
    let ids: Result<Vec<_>, _> = ids.iter().map(|i| parse_id(i)).collect();
    state.conversations.reorder(&ids?).await?;

    // Poradi je v pameti taky, aby ho pristi ulozeni otevrene konverzace
    // nevratilo na starou hodnotu.
    let seznam = state.conversations.list().await?;
    let mut session = state.session.lock().await;
    if let Some(c) = session.conversation.as_mut() {
        if let Some(aktualni) = seznam.iter().find(|s| s.id == c.id) {
            c.sort_order = aktualni.sort_order;
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn delete_conversation(
    state: State<'_, AppState>,
    id: String,
) -> CommandResult<SessionView> {
    let id = parse_id(&id)?;
    state.conversations.delete(id).await?;

    // Smazani otevrene konverzace nesmi nechat v okne zpravy, ktere uz
    // nikde nejsou.
    {
        let mut session = state.session.lock().await;
        if session.conversation.as_ref().is_some_and(|c| c.id == id) {
            session.conversation = None;
        }
    }

    session_view(&state).await
}

/// Odvětví novou konverzaci od zadané zprávy — **včetně** jí.
///
/// „Odsud jinudy": původní vlákno zůstane, jak bylo, a pokračuje se v kopii.
#[tauri::command]
pub async fn branch_conversation(
    state: State<'_, AppState>,
    message_id: String,
) -> CommandResult<SessionView> {
    vetvit(&state, &message_id, true).await
}

/// Odvětví novou konverzaci **před** zadanou zprávou.
///
/// „Zeptat se znovu jinak": zpráva se do větve nezkopíruje, takže vlákno
/// čeká na nové zadání. Text původní zprávy si do pole doplní UI — má ho
/// vykreslený, takže ho není potřeba posílat zpátky.
#[tauri::command]
pub async fn branch_before_message(
    state: State<'_, AppState>,
    message_id: String,
) -> CommandResult<SessionView> {
    vetvit(&state, &message_id, false).await
}

async fn vetvit(
    state: &State<'_, AppState>,
    message_id: &str,
    vcetne: bool,
) -> CommandResult<SessionView> {
    let message_id = MessageId::from_str(message_id)
        .map_err(|_| DomainError::validation(format!("neplatné ID zprávy: {message_id}")))?;

    // Rodič se bere z paměti, ne z databáze: poslední odpověď se ukládá až
    // po dogenerování a větev by o ni jinak přišla.
    let rodic = {
        let session = state.session.lock().await;
        if session.generating {
            return Err(
                DomainError::validation("Model právě odpovídá. Větvit jde až potom.").into(),
            );
        }
        session
            .conversation
            .clone()
            .ok_or_else(|| DomainError::validation("Není otevřená žádná konverzace."))?
    };

    let mut vetev = if vcetne {
        rodic.branch_through(message_id)?
    } else {
        rodic.branch_before(message_id)?
    };

    let existujici = state.conversations.list().await?;
    let nazvy: Vec<String> = existujici.iter().map(|c| c.title.clone()).collect();
    vetev.title = history::branch_title(&rodic.title, &nazvy);
    vetev.sort_order = history::order_for_new(&existujici);

    // Rodič se ukládá taky — než se přepne, musí být na disku i to, co se
    // do něj přidalo od posledního uložení.
    if let Err(e) = state.conversations.save(&rodic).await {
        tracing::warn!(error = %e, "Rodice se pred vetvenim nepodarilo ulozit");
    }
    state.conversations.save(&vetev).await?;

    state.session.lock().await.conversation = Some(vetev);
    session_view(state).await
}

/// Projde otevřenou složku a vrátí nálezy.
#[tauri::command]
pub async fn run_review(
    app: AppHandle,
    state: State<'_, AppState>,
    focus: Option<String>,
) -> CommandResult<ReviewReportView> {
    let engine = state.engine().await?;
    let cancel = state.begin_cancellable().await;

    let (mut conversation, workspace) = {
        let mut session = state.session.lock().await;
        if session.generating {
            return Err(DomainError::validation(
                "Model právě pracuje. Počkej na dokončení, nebo ho zastav.",
            )
            .into());
        }
        let Some(ws) = session.workspace.clone() else {
            return Err(DomainError::validation(
                "Nejdřív vyber složku projektu — bez ní není co kontrolovat.",
            )
            .into());
        };
        session.generating = true;
        (
            session
                .conversation
                .take()
                .unwrap_or_else(|| Conversation::new("")),
            ws,
        )
    };

    let fs = match LocalWorkspaceFs::new(workspace) {
        Ok(fs) => Arc::new(fs),
        Err(e) => {
            let mut session = state.session.lock().await;
            session.conversation = Some(conversation);
            session.generating = false;
            return Err(e.into());
        }
    };

    let hlasic = app.clone();
    let progress: anvil_domain::ports::ProgressCallback = Arc::new(move |p: GenerationProgress| {
        let _ = hlasic.emit("generation:delta", &p);
    });

    let vysledek = ReviewService::new()
        .run(
            &mut conversation,
            &engine,
            fs,
            focus.as_deref(),
            cancel,
            AgentHooks::events(agent_events(&app)).with_progress(Some(progress)),
        )
        .await;

    if let Err(e) = state.conversations.save(&conversation).await {
        tracing::error!(error = %e, "Konverzaci se nepodarilo ulozit");
    }
    {
        let mut session = state.session.lock().await;
        session.conversation = Some(conversation);
        session.generating = false;
    }

    let out = vysledek?;
    Ok(ReviewReportView {
        headline: out.report.headline(),
        findings: out
            .report
            .sorted()
            .into_iter()
            .map(|f| FindingView {
                file: f.file.to_string(),
                line: f.line,
                severity: severity_key(f.severity).into(),
                summary: f.summary.clone(),
                detail: f.detail.clone(),
                location: f.location(),
            })
            .collect(),
        files_read: out
            .report
            .files_read
            .iter()
            .map(|p| p.to_string())
            .collect(),
        rounds: out.report.rounds,
        files_total: out.report.files_total,
        hit_round_limit: out.report.hit_round_limit,
        summary: out.summary,
        total_ms: out.total_ms,
    })
}

// --- Úpravy souborů -------------------------------------------------------

/// Čeká na schválení. Ukazuje se jako diff.
#[derive(Debug, Serialize)]
pub struct PendingEditView {
    pub path: String,
    pub headline: String,
    pub lines: Vec<DiffLine>,
    pub added: u32,
    pub removed: u32,
    pub creates_file: bool,
    /// Náhled je zkrácený — u velké změny se ukáže jen začátek.
    pub truncated: bool,
    /// Kolik úprav se na tomhle souboru sešlo.
    pub edits: u32,
}

/// Návrhy, které čekají na rozhodnutí uživatele.
#[tauri::command]
pub async fn pending_edits(state: State<'_, AppState>) -> CommandResult<Vec<PendingEditView>> {
    let plan = state.session.lock().await.edits.clone();
    let plan = plan.lock().await;

    Ok(plan
        .changes()
        .iter()
        .map(|zmena| {
            let nahled = zmena.preview();
            PendingEditView {
                path: nahled.path.to_string(),
                headline: nahled.headline(),
                added: nahled.added,
                removed: nahled.removed,
                creates_file: nahled.creates_file,
                truncated: nahled.truncated,
                edits: zmena.edits,
                lines: nahled.lines,
            }
        })
        .collect())
}

/// Zapíše schválené soubory na disk.
///
/// **Jediná cesta, kudy se model dostane k zápisu**, a vede přes tlačítko,
/// které zmáčkne uživatel poté, co viděl diff.
#[tauri::command]
pub async fn apply_edits(
    state: State<'_, AppState>,
    paths: Vec<String>,
) -> CommandResult<Vec<String>> {
    let (plan, workspace) = {
        let session = state.session.lock().await;
        (session.edits.clone(), session.workspace.clone())
    };
    let Some(ws) = workspace else {
        return Err(DomainError::validation("Není otevřená složka projektu.").into());
    };

    let cesty: Result<Vec<RelativePath>, _> =
        paths.iter().map(|p| RelativePath::parse(p)).collect();
    let fs: Arc<dyn anvil_domain::ports::WorkspaceFs> = Arc::new(LocalWorkspaceFs::new(ws)?);

    let zapsane = plan.lock().await.apply(&fs, &cesty?).await?;
    Ok(zapsane.iter().map(|p| p.to_string()).collect())
}

/// Zahodí návrhy. Bez `paths` zahodí všechny.
#[tauri::command]
pub async fn discard_edits(
    state: State<'_, AppState>,
    paths: Option<Vec<String>>,
) -> CommandResult<()> {
    let plan = state.session.lock().await.edits.clone();
    let mut plan = plan.lock().await;

    match paths {
        Some(paths) => {
            let cesty: Result<Vec<RelativePath>, _> =
                paths.iter().map(|p| RelativePath::parse(p)).collect();
            plan.discard(&cesty?);
        }
        None => plan.clear(),
    }
    Ok(())
}

/// Přeposílá kroky smyčky do UI.
fn agent_events(app: &AppHandle) -> anvil_application::agent::runner::AgentEventCallback {
    let app = app.clone();
    Arc::new(move |e: AgentEvent| {
        let _ = app.emit("agent:event", AgentEventView::from(e));
    })
}

/// Nástroje, které má model při otevřené složce k dispozici — do nápovědy v UI.
#[tauri::command]
pub async fn list_tools(state: State<'_, AppState>) -> CommandResult<Vec<String>> {
    let workspace = state.session.lock().await.workspace.clone();
    let Some(ws) = workspace else {
        return Ok(Vec::new());
    };
    let fs = Arc::new(LocalWorkspaceFs::new(ws)?);
    Ok(anvil_application::agent::tools::Toolbox::for_review(fs)
        .specs()
        .iter()
        .map(ToolSpec::prompt_line)
        .collect())
}

fn parse_id(raw: &str) -> DomainResult<ConversationId> {
    raw.parse()
        .map_err(|_| DomainError::validation(format!("'{raw}' neni platne ID konverzace")))
}

/// Odešle dotaz. Tokeny chodí událostí `generation:delta`, výsledné
/// statistiky přes `generation:finished`.
#[tauri::command]
pub async fn send_message(
    app: AppHandle,
    state: State<'_, AppState>,
    text: String,
) -> CommandResult<SessionView> {
    let engine = state.engine().await?;
    let settings = state.settings().await?;
    let cancel = state.begin_cancellable().await;

    // Konverzace se na dobu generování vyjme ze session, aby se nemusel držet
    // zámek přes celý (dlouhý) běh modelu. Druhé odeslání se proto musí
    // odmítnout — jinak by si obě konverzaci vyjmula a na konci přepsala.
    let (mut conversation, workspace, edit_plan) = {
        let mut session = state.session.lock().await;
        if session.generating {
            return Err(DomainError::validation(
                "Model právě odpovídá. Počkej na dokončení, nebo generování zastav.",
            )
            .into());
        }
        session.generating = true;
        (
            session
                .conversation
                .take()
                .unwrap_or_else(|| Conversation::new("")),
            session.workspace.clone(),
            session.edits.clone(),
        )
    };

    let hlasic = app.clone();
    let progress: anvil_domain::ports::ProgressCallback = Arc::new(move |p: GenerationProgress| {
        let _ = hlasic.emit("generation:delta", &p);
    });

    // S otevřenou složkou dostane model nástroje a smyčku; bez ní je to
    // obyčejný chat. Nacpat mu obsah projektu do promptu předem nejde —
    // při ~27 tokenech za sekundu na zpracování promptu by se na každou
    // zprávu čekalo minuty.
    let vysledek = match &workspace {
        Some(ws) => {
            let hooks = AgentHooks::events(agent_events(&app)).with_progress(Some(progress));
            match LocalWorkspaceFs::new(ws.clone()) {
                Ok(fs) => {
                    // Sada s návrhem úprav. Zapsat nic neumí — `edit_file`
                    // jen odloží změnu do plánu, kterou pak uživatel uvidí
                    // jako diff a buď ji potvrdí, nebo zahodí.
                    let fs: Arc<dyn anvil_domain::ports::WorkspaceFs> = Arc::new(fs);
                    // Prázdná složka = zakládá se projekt. Instrukce „přečti
                    // si soubor“ by tam byla nesmysl a model by kolo promarnil
                    // hledáním něčeho, co v ní není.
                    let prazdna = fs
                        .list(None)
                        .await
                        .map(|soubory| soubory.is_empty())
                        .unwrap_or(false);
                    let system = if prazdna {
                        empty_project_system(ws)
                    } else {
                        workspace_chat_system(ws)
                    };

                    let toolbox =
                        anvil_application::agent::tools::Toolbox::for_editing(fs, edit_plan);
                    conversation.push(anvil_domain::conversation::Message::user(text.trim()));
                    conversation.derive_title();

                    // Víc tokenů na kolo než u review: obsah zakládaného
                    // souboru cestuje **uvnitř** volání nástroje, takže
                    // useknutí uprostřed nerozbije jen text, ale celý JSON
                    // a kolo se promarní na neplatném volání.
                    AgentLoop::new()
                        .with_max_tokens_per_round(2_048)
                        .run(&mut conversation, &engine, &toolbox, &system, cancel, hooks)
                        .await
                        .map(|out| anvil_application::SendOutcome {
                            outcome: anvil_domain::ports::CompletionOutcome {
                                text: out.text,
                                prompt_tokens: out.prompt_tokens,
                                generated_tokens: out.generated_tokens,
                                time_to_first_token_ms: 0,
                                total_ms: out.total_ms,
                                cancelled: false,
                            },
                            compacted: None,
                        })
                }
                Err(e) => Err(e),
            }
        }
        None => {
            state
                .chat
                .send(
                    &mut conversation,
                    &engine,
                    &text,
                    TurnContext::new(settings.active_role),
                    cancel,
                    Some(progress),
                )
                .await
        }
    };

    // Ulozit driv, nez se cokoli dalsiho stane, a bez ohledu na to, jak tah
    // dopadl. Castecna odpoved po zruseni i samotny dotaz po chybe jsou pro
    // uzivatele cennejsi nez cista databaze.
    if let Err(e) = state.conversations.save(&conversation).await {
        tracing::error!(error = %e, "Konverzaci se nepodarilo ulozit");
    }

    // Konverzaci vratit zpatky at to dopadlo jakkoli - jinak by se po chybe
    // ztratila cela historie.
    {
        let mut session = state.session.lock().await;
        session.conversation = Some(conversation);
        session.generating = false;
    }

    match vysledek {
        Ok(out) => {
            let stats = GenerationStats {
                prompt_tokens: out.outcome.prompt_tokens,
                generated_tokens: out.outcome.generated_tokens,
                time_to_first_token_ms: out.outcome.time_to_first_token_ms,
                total_ms: out.outcome.total_ms,
                tokens_per_second: out.outcome.decode_tokens_per_second(),
                cancelled: out.outcome.cancelled,
                compacted_messages: out.compacted.map(|c| c.message_count),
            };
            let _ = app.emit("generation:finished", &stats);
            session_view(&state).await
        }
        Err(e) => Err(e.into()),
    }
}

#[tauri::command]
pub async fn cancel_generation(state: State<'_, AppState>) -> CommandResult<bool> {
    Ok(state.cancel_running().await)
}

// --- Společné -------------------------------------------------------------

async fn session_view(state: &State<'_, AppState>) -> CommandResult<SessionView> {
    let session = state.session.lock().await;
    let prazdna = Conversation::new("");
    let c = session.conversation.as_ref().unwrap_or(&prazdna);

    Ok(SessionView {
        conversation_id: session.conversation.as_ref().map(|c| c.id.to_string()),
        generating: session.generating,
        loaded_model: session.loaded_model.as_ref().map(ModelId::to_string),
        plan_description: session.plan_description.clone(),
        workspace_path: session
            .workspace
            .as_ref()
            .map(|w| w.root().display().to_string()),
        workspace_name: session.workspace.as_ref().map(|w| w.name()),
        conversation_title: c.title.clone(),
        parent_id: c.branched_from.as_ref().map(|b| b.parent.to_string()),
        messages: c.messages.iter().map(MessageView::from).collect(),
        used_tokens: c.visible_token_estimate(),
        context_tokens: session
            .engine
            .as_ref()
            .map(|e| e.context_tokens())
            .unwrap_or(0),
        has_summary: c.summary.is_some(),
        engine_available: cfg!(feature = "engine"),
    })
}
