//! Odeslání zprávy a přijetí odpovědi.

use std::sync::Arc;

use anvil_domain::{
    conversation::{Conversation, Message},
    error::{DomainError, DomainResult},
    model::{ModelRole, Sampling},
    ports::{ChatEngine, CompletionOutcome, CompletionRequest, ProgressCallback},
    workspace::Workspace,
};
use tokio_util::sync::CancellationToken;

use crate::{
    compaction::{CompactionPlan, CompactionService},
    prompts::system_prompt,
};

/// Kolik tokenů se v okně drží stranou na odpověď.
///
/// Bez rezervy by se konverzace roztáhla přes celé okno a model by se uřízl
/// v půlce věty — z pohledu uživatele „přestal odpovídat".
const DEFAULT_RESPONSE_RESERVE: u32 = 2_048;

/// Odhad, kolik tokenů zabere systémová instrukce. Měřit ji při každém tahu
/// tokenizerem by nic nezměnilo — je krátká a v rozpočtu jde o rezervu.
const SYSTEM_PROMPT_RESERVE: u32 = 512;

/// Kde a v jakém režimu tah probíhá.
///
/// Role a workspace cestují spolu všude, kde se skládá systémová instrukce —
/// a od fáze 2 je bude potřebovat i sada nástrojů, protože workspace určuje
/// hranice, ve kterých smí model číst.
#[derive(Debug, Clone, Copy)]
pub struct TurnContext<'a> {
    pub role: ModelRole,
    pub workspace: Option<&'a Workspace>,
}

impl<'a> TurnContext<'a> {
    pub fn new(role: ModelRole) -> Self {
        Self {
            role,
            workspace: None,
        }
    }

    pub fn with_workspace(mut self, workspace: Option<&'a Workspace>) -> Self {
        self.workspace = workspace;
        self
    }

    /// Samplování odpovídající režimu. Analýza kódu musí být reprodukovatelná,
    /// konverzace může být volnější.
    fn sampling(&self) -> Sampling {
        match self.role {
            ModelRole::Coding => Sampling::PRECISE,
            ModelRole::Conversational => Sampling::BALANCED,
        }
    }
}

#[derive(Debug)]
pub struct SendOutcome {
    pub outcome: CompletionOutcome,
    /// Vyplněné, když se před odesláním slučoval kontext.
    pub compacted: Option<CompactionPlan>,
}

pub struct ChatService {
    compaction: CompactionService,
    response_reserve: u32,
}

impl ChatService {
    pub fn new() -> Self {
        Self {
            compaction: CompactionService::new(),
            response_reserve: DEFAULT_RESPONSE_RESERVE,
        }
    }

    pub fn with_compaction(mut self, compaction: CompactionService) -> Self {
        self.compaction = compaction;
        self
    }

    pub fn with_response_reserve(mut self, tokens: u32) -> Self {
        self.response_reserve = tokens.max(256);
        self
    }

    /// Kolik tokenů okna zbývá na samotnou konverzaci.
    fn budget(&self, engine: &Arc<dyn ChatEngine>) -> u32 {
        engine
            .context_tokens()
            .saturating_sub(self.response_reserve)
            .saturating_sub(SYSTEM_PROMPT_RESERVE)
    }

    /// Odešle dotaz uživatele a připojí odpověď modelu ke konverzaci.
    ///
    /// Konverzace se mění **na místě**: po návratu v ní je dotaz i odpověď,
    /// obojí se změřeným počtem tokenů. Když se okno plnilo, proběhlo před
    /// odesláním sloučení a je hlášené v [`SendOutcome::compacted`].
    ///
    /// Při zrušení se odpověď (i částečná) ke konverzaci **připojí** — text,
    /// který už uživatel viděl na obrazovce, nesmí z historie zmizet.
    pub async fn send(
        &self,
        conversation: &mut Conversation,
        engine: &Arc<dyn ChatEngine>,
        user_text: &str,
        ctx: TurnContext<'_>,
        cancel: CancellationToken,
        on_progress: Option<ProgressCallback>,
    ) -> DomainResult<SendOutcome> {
        let text = user_text.trim();
        if text.is_empty() {
            return Err(DomainError::validation("dotaz nesmí být prázdný"));
        }

        let tokenu = engine.count_tokens(text)?;
        conversation.push(Message::user(text).with_token_count(tokenu));
        conversation.derive_title();

        // Sloučit až po přidání dotazu — jinak by se rozhodovalo podle
        // rozpočtu, který o právě přidanou zprávu ještě neví.
        let compacted = self
            .compaction
            .compact_if_needed(conversation, engine, self.budget(engine), cancel.clone())
            .await?;

        let request = CompletionRequest::new(conversation.visible_messages().to_vec())
            .with_system(system_prompt(ctx.role, ctx.workspace))
            .with_summary(conversation.summary.clone())
            .with_max_tokens(self.response_reserve)
            .with_sampling(ctx.sampling());

        let outcome = engine.complete(&request, cancel, on_progress).await?;

        let odpoved = outcome.text.trim();
        if !odpoved.is_empty() {
            conversation
                .push(Message::assistant(odpoved).with_token_count(outcome.generated_tokens));
        }

        Ok(SendOutcome { outcome, compacted })
    }
}

impl Default for ChatService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use anvil_domain::conversation::Role;

    use super::*;
    use crate::testing::{scripted, ScriptedEngine, ScriptedResponse};

    fn konverzace() -> Conversation {
        Conversation::new("")
    }

    #[tokio::test]
    async fn dotaz_i_odpoved_skonci_v_konverzaci() {
        let engine = scripted(ScriptedEngine::with_texts(["Tady je odpověď."]));
        let mut c = konverzace();

        ChatService::new()
            .send(
                &mut c,
                &engine,
                "Zkontroluj parser",
                TurnContext::new(ModelRole::Coding),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(c.messages.len(), 2);
        assert_eq!(c.messages[0].role, Role::User);
        assert_eq!(c.messages[0].content, "Zkontroluj parser");
        assert_eq!(c.messages[1].role, Role::Assistant);
        assert_eq!(c.messages[1].content, "Tady je odpověď.");
    }

    #[tokio::test]
    async fn zpravy_maji_zmereny_pocet_tokenu() {
        // Bez toho by se sloučení kontextu rozhodovalo podle odhadu ze znaků.
        let engine = scripted(ScriptedEngine::with_texts(["odpověď"]));
        let mut c = konverzace();

        ChatService::new()
            .send(
                &mut c,
                &engine,
                "dotaz na kód",
                TurnContext::new(ModelRole::Coding),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        assert!(c.messages.iter().all(|m| m.token_count.is_some()));
    }

    #[tokio::test]
    async fn prazdny_dotaz_neprojde() {
        let engine = scripted(ScriptedEngine::always("nemělo se stát"));
        let mut c = konverzace();

        for vstup in ["", "   ", "\n\t "] {
            assert!(ChatService::new()
                .send(
                    &mut c,
                    &engine,
                    vstup,
                    TurnContext::new(ModelRole::Coding),
                    CancellationToken::new(),
                    None
                )
                .await
                .is_err());
        }
        assert!(c.messages.is_empty(), "prázdný dotaz nesmí nic přidat");
    }

    #[tokio::test]
    async fn nazev_konverzace_se_odvodi_z_prvniho_dotazu() {
        let engine = scripted(ScriptedEngine::with_texts(["ok"]));
        let mut c = konverzace();

        ChatService::new()
            .send(
                &mut c,
                &engine,
                "Najdi chybu v downloaderu",
                TurnContext::new(ModelRole::Coding),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(c.title, "Najdi chybu v downloaderu");
    }

    #[tokio::test]
    async fn systemova_instrukce_jde_mimo_historii() {
        let engine = ScriptedEngine::with_texts(["ok"]);
        let engine = Arc::new(engine);
        let jako_port: Arc<dyn ChatEngine> = engine.clone();
        let mut c = konverzace();

        ChatService::new()
            .send(
                &mut c,
                &jako_port,
                "dotaz",
                TurnContext::new(ModelRole::Coding),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        let volani = engine.last_call();
        assert!(volani.system.is_some(), "instrukce má jít zvlášť");
        assert!(
            !c.messages.iter().any(|m| m.role == Role::System),
            "instrukce se do historie ukládat nemá"
        );
    }

    #[tokio::test]
    async fn role_urcuje_samplovani() {
        // Analýza kódu musí být reprodukovatelná, konverzace může být volnější.
        for (role, ocekavano) in [
            (ModelRole::Coding, Sampling::PRECISE),
            (ModelRole::Conversational, Sampling::BALANCED),
        ] {
            let engine = Arc::new(ScriptedEngine::with_texts(["ok"]));
            let port: Arc<dyn ChatEngine> = engine.clone();
            let mut c = konverzace();

            ChatService::new()
                .send(
                    &mut c,
                    &port,
                    "dotaz",
                    TurnContext::new(role),
                    CancellationToken::new(),
                    None,
                )
                .await
                .unwrap();

            assert_eq!(engine.last_call().sampling, ocekavano, "{role:?}");
        }
    }

    #[tokio::test]
    async fn prazdna_odpoved_se_do_historie_nepridava() {
        // Prázdná bublina v UI vypadá jako chyba appky.
        let engine = scripted(ScriptedEngine::with_texts(["   "]));
        let mut c = konverzace();

        ChatService::new()
            .send(
                &mut c,
                &engine,
                "dotaz",
                TurnContext::new(ModelRole::Coding),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(c.messages.len(), 1, "má tam být jen dotaz");
    }

    #[tokio::test]
    async fn zrusena_odpoved_zustane_v_historii() {
        // Text, který uživatel viděl na obrazovce, nesmí po zrušení zmizet.
        let engine = scripted(ScriptedEngine::new([ScriptedResponse::CancelledAfter(
            "začátek odpově".into(),
        )]));
        let mut c = konverzace();

        let out = ChatService::new()
            .send(
                &mut c,
                &engine,
                "dotaz",
                TurnContext::new(ModelRole::Coding),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        assert!(out.outcome.cancelled);
        assert_eq!(c.messages.len(), 2);
        assert_eq!(c.messages[1].content, "začátek odpově");
    }

    #[tokio::test]
    async fn selhani_modelu_nezanecha_odpoved() {
        let engine = scripted(ScriptedEngine::new([ScriptedResponse::Failure(
            "model spadl".into(),
        )]));
        let mut c = konverzace();

        assert!(ChatService::new()
            .send(
                &mut c,
                &engine,
                "dotaz",
                TurnContext::new(ModelRole::Coding),
                CancellationToken::new(),
                None
            )
            .await
            .is_err());

        // Dotaz zůstat má — uživatel ho napsal a chce ho vidět, aby ho mohl
        // poslat znovu. Odpověď žádná není.
        assert_eq!(c.messages.len(), 1);
        assert_eq!(c.messages[0].role, Role::User);
    }

    #[tokio::test]
    async fn dlouha_konverzace_spusti_slouceni() {
        // Malé okno + dlouhá historie → před odesláním se musí sloučit.
        let engine = Arc::new(
            ScriptedEngine::always("odpověď")
                // Rozpočet = 3000 − 2048 (rezerva) − 512 (instrukce) = 440 tokenů.
                .with_context_tokens(3_000),
        );
        let port: Arc<dyn ChatEngine> = engine.clone();

        let mut c = konverzace();
        for i in 0..12 {
            c.push(Message::user(format!("dotaz {i}")).with_token_count(60));
            c.push(Message::assistant(format!("odpověď {i}")).with_token_count(60));
        }

        let out = ChatService::new()
            .send(
                &mut c,
                &port,
                "poslední dotaz",
                TurnContext::new(ModelRole::Coding),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        let plan = out.compacted.expect("mělo dojít ke sloučení");
        assert!(plan.message_count > 0);
        assert!(c.summary.is_some(), "souhrn se má uložit ke konverzaci");
        assert!(
            c.visible_messages().len() < c.messages.len(),
            "část historie se má z promptu vypustit"
        );
    }

    #[tokio::test]
    async fn kratka_konverzace_neslucuje() {
        let engine = Arc::new(ScriptedEngine::always("odpověď").with_context_tokens(32_768));
        let port: Arc<dyn ChatEngine> = engine.clone();
        let mut c = konverzace();

        let out = ChatService::new()
            .send(
                &mut c,
                &port,
                "dotaz",
                TurnContext::new(ModelRole::Coding),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        assert!(out.compacted.is_none());
        assert_eq!(
            engine.call_count(),
            1,
            "nemá se volat model navíc kvůli shrnutí"
        );
    }

    #[tokio::test]
    async fn souhrn_se_posle_modelu_v_dalsim_tahu() {
        let engine = Arc::new(ScriptedEngine::always("odpověď").with_context_tokens(3_000));
        let port: Arc<dyn ChatEngine> = engine.clone();

        let mut c = konverzace();
        for i in 0..12 {
            c.push(Message::user(format!("dotaz {i}")).with_token_count(60));
            c.push(Message::assistant(format!("odpověď {i}")).with_token_count(60));
        }

        let sluzba = ChatService::new();
        sluzba
            .send(
                &mut c,
                &port,
                "první",
                TurnContext::new(ModelRole::Coding),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        sluzba
            .send(
                &mut c,
                &port,
                "druhý",
                TurnContext::new(ModelRole::Coding),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        assert!(
            engine.last_call().summary.is_some(),
            "po sloučení musí souhrn jít do každého dalšího promptu"
        );
    }

    #[tokio::test]
    async fn prubeh_generovani_se_hlasi() {
        use std::sync::Mutex;

        let engine = scripted(ScriptedEngine::with_texts(["jedna dvě tři čtyři"]));
        let sebrane = Arc::new(Mutex::new(String::new()));
        let sber = sebrane.clone();
        let mut c = konverzace();

        ChatService::new()
            .send(
                &mut c,
                &engine,
                "dotaz",
                TurnContext::new(ModelRole::Coding),
                CancellationToken::new(),
                Some(Arc::new(
                    move |p: anvil_domain::ports::GenerationProgress| {
                        *sber.lock().unwrap() = p.accumulated;
                    },
                )),
            )
            .await
            .unwrap();

        assert_eq!(*sebrane.lock().unwrap(), "jedna dvě tři čtyři");
    }

    #[tokio::test]
    async fn zruseni_pred_odeslanim_nevola_model() {
        let engine = Arc::new(ScriptedEngine::always("nemělo se stát"));
        let port: Arc<dyn ChatEngine> = engine.clone();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut c = konverzace();

        assert!(ChatService::new()
            .send(
                &mut c,
                &port,
                "dotaz",
                TurnContext::new(ModelRole::Coding),
                cancel,
                None
            )
            .await
            .unwrap_err()
            .is_cancelled());
    }
}
