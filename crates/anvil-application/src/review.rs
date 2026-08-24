//! Code review nad otevřenou složkou.
//!
//! Review je agentní smyčka s vlastním zadáním a sadou nástrojů jen pro
//! čtení. Rozdíl proti běžnému chatu je hlavně v instrukci: model se má
//! **rozhlédnout sám** a nálezy hlásit nástrojem, ne prózou.

use std::sync::Arc;

use anvil_domain::{
    conversation::{Conversation, Message},
    error::DomainResult,
    ports::{ChatEngine, WorkspaceFs},
    review::ReviewReport,
    workspace::Workspace,
};
use tokio_util::sync::CancellationToken;

use crate::agent::{
    runner::{AgentHooks, AgentLoop, AgentOutcome},
    tools::Toolbox,
};

/// Instrukce pro review.
///
/// Dvě věci v ní stojí za pozornost. **„Nehádej"** je tam proto, že model bez
/// obsahu souboru klidně vymyslí věrohodně vypadající nález i s číslem řádku.
/// A **postup od hrubého k jemnému** proto, že zpracování promptu jede
/// ~27 tokenů za sekundu — přečíst deset souborů „pro jistotu" znamená
/// minuty čekání navíc.
const REVIEW_SYSTEM: &str = "Jsi zkušený programátor a děláš code review cizího projektu.

Postup:
1. Rozhlédni se — `list_files`, ať víš, co v projektu je.
2. Hledej cíleně — `grep` na typické zdroje chyb je levnější než číst soubory po jednom.
3. Čti jen to, co potřebuješ, a po částech.
4. Každý problém nahlas zvlášť přes `report_finding`.

Co hledat: chyby, které se projeví za běhu (pády, špatné ošetření chyb, souběh),
bezpečnostní díry, tiché ignorování chyb a místa, kde kód dělá něco jiného,
než co slibuje jeho název.

Čemu se vyhnout: názorům na formátování a stylu — od toho jsou nástroje.
A hlavně **nehádej**. Nález hlas jen k souboru, který jsi opravdu četl,
a s řádkem, který jsi opravdu viděl. Když si nejsi jistý, radši to neuváděj.

Až budeš hotový, napiš krátké shrnutí normálním textem bez volání nástrojů.";

/// Instrukce pro běžný chat nad otevřenou složkou.
///
/// Model nedostane obsah projektu předem — dostane nástroje. Nacpat mu do
/// promptu celý repozitář by při rychlosti zpracování promptu znamenalo
/// minuty čekání na každou zprávu.
pub fn workspace_chat_system(workspace: &Workspace) -> String {
    format!(
        "Jsi zkušený programátor a pomáháš vývojáři s projektem ve složce „{}\".

Odpovídej česky. Názvy souborů, funkcí a útržky kódu nepřekládej.

Obsah projektu předem neznáš — když potřebuješ vědět, co v souboru je, přečti
si ho nástrojem. Nehádej: raději si ověř, co tam skutečně stojí, než abys
odpověděl podle názvu souboru. Cesty uváděj relativně ke složce projektu.",
        workspace.name()
    )
}

#[derive(Debug)]
pub struct ReviewOutcome {
    pub report: ReviewReport,
    /// Závěrečné shrnutí od modelu.
    pub summary: String,
    pub prompt_tokens: u32,
    pub generated_tokens: u32,
    pub total_ms: u64,
}

pub struct ReviewService {
    agent: AgentLoop,
}

impl ReviewService {
    pub fn new() -> Self {
        Self {
            agent: AgentLoop::new(),
        }
    }

    pub fn with_agent(agent: AgentLoop) -> Self {
        Self { agent }
    }

    /// Projde projekt a vrátí nálezy.
    ///
    /// `focus` je volitelné zúžení od uživatele („podívej se hlavně na
    /// downloader"). Bez něj si model vybere sám.
    pub async fn run(
        &self,
        conversation: &mut Conversation,
        engine: &Arc<dyn ChatEngine>,
        fs: Arc<dyn WorkspaceFs>,
        focus: Option<&str>,
        cancel: CancellationToken,
        hooks: AgentHooks,
    ) -> DomainResult<ReviewOutcome> {
        let zadani = match focus.map(str::trim).filter(|f| !f.is_empty()) {
            Some(f) => format!("Projdi projekt a zaměř se na tohle: {f}"),
            None => "Projdi projekt a najdi problémy.".to_string(),
        };
        conversation.push(Message::user(&zadani));
        conversation.derive_title();

        let toolbox = Toolbox::for_review(fs);
        let outcome: AgentOutcome = self
            .agent
            .run(conversation, engine, &toolbox, REVIEW_SYSTEM, cancel, hooks)
            .await?;

        Ok(ReviewOutcome {
            report: ReviewReport {
                findings: outcome.artifacts.findings,
                files_read: outcome.artifacts.files_read,
                rounds: outcome.rounds,
                hit_round_limit: outcome.hit_round_limit,
            },
            summary: outcome.text,
            prompt_tokens: outcome.prompt_tokens,
            generated_tokens: outcome.generated_tokens,
            total_ms: outcome.total_ms,
        })
    }
}

impl Default for ReviewService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use anvil_domain::{review::Severity, workspace::Workspace};

    use super::*;
    use crate::{
        agent::tools::fake_fs::FakeFs,
        testing::{scripted, ScriptedEngine, ScriptedResponse},
    };

    fn fs() -> Arc<dyn WorkspaceFs> {
        Arc::new(FakeFs::new(&[
            ("src/main.rs", "fn main() {\n    let x = f().unwrap();\n}"),
            ("src/lib.rs", "pub fn f() -> Option<i32> { None }"),
        ]))
    }

    fn volani(json: &str) -> String {
        format!("<tool>{json}</tool>")
    }

    #[tokio::test]
    async fn review_posbira_nalezy_a_shrnuti() {
        let engine = scripted(ScriptedEngine::new(vec![
            ScriptedResponse::text(volani(
                r#"{"name":"read_file","arguments":{"path":"src/main.rs"}}"#,
            )),
            ScriptedResponse::text(volani(
                r#"{"name":"report_finding","arguments":{"file":"src/main.rs","line":2,"severity":"critical","summary":"unwrap na funkci, která vrací None"}}"#,
            )),
            ScriptedResponse::text("Našel jsem jeden vážný problém."),
        ]));

        let mut c = Conversation::new("");
        let out = ReviewService::new()
            .run(
                &mut c,
                &engine,
                fs(),
                None,
                CancellationToken::new(),
                AgentHooks::default(),
            )
            .await
            .unwrap();

        assert_eq!(out.report.findings.len(), 1);
        assert_eq!(out.report.findings[0].severity, Severity::Critical);
        assert_eq!(out.report.files_read.len(), 1);
        assert_eq!(out.summary, "Našel jsem jeden vážný problém.");
        assert!(!out.report.hit_round_limit);
    }

    #[tokio::test]
    async fn zameren_se_dostane_do_zadani() {
        let engine = Arc::new(ScriptedEngine::with_texts(["hotovo"]));
        let port: Arc<dyn ChatEngine> = engine.clone();
        let mut c = Conversation::new("");

        ReviewService::new()
            .run(
                &mut c,
                &port,
                fs(),
                Some("  downloader  "),
                CancellationToken::new(),
                AgentHooks::default(),
            )
            .await
            .unwrap();

        let zpravy = engine.last_call().messages;
        assert!(
            zpravy.iter().any(|m| m.content.contains("downloader")),
            "{zpravy:?}"
        );
    }

    #[tokio::test]
    async fn prazdne_zamereni_se_ignoruje() {
        let engine = Arc::new(ScriptedEngine::with_texts(["hotovo"]));
        let port: Arc<dyn ChatEngine> = engine.clone();
        let mut c = Conversation::new("");

        ReviewService::new()
            .run(
                &mut c,
                &port,
                fs(),
                Some("   "),
                CancellationToken::new(),
                AgentHooks::default(),
            )
            .await
            .unwrap();

        assert!(engine.last_call().messages[0]
            .content
            .contains("najdi problémy"));
    }

    #[tokio::test]
    async fn instrukce_zakazuje_hadani() {
        // Model bez obsahu souboru si vymyslí věrohodný nález i s řádkem.
        let engine = Arc::new(ScriptedEngine::with_texts(["hotovo"]));
        let port: Arc<dyn ChatEngine> = engine.clone();
        let mut c = Conversation::new("");

        ReviewService::new()
            .run(
                &mut c,
                &port,
                fs(),
                None,
                CancellationToken::new(),
                AgentHooks::default(),
            )
            .await
            .unwrap();

        let system = engine.last_call().system.unwrap_or_default();
        assert!(
            system.contains("nehádej") || system.contains("Nehádej"),
            "{system}"
        );
        assert!(system.contains("report_finding"), "{system}");
    }

    #[tokio::test]
    async fn review_hlasi_dosazeni_limitu_kol() {
        // Uživatel musí poznat rozdíl mezi „nic nenašel" a „došla kola".
        let engine = scripted(ScriptedEngine::always(volani(
            r#"{"name":"list_files","arguments":{}}"#,
        )));
        let mut c = Conversation::new("");

        let out = ReviewService::with_agent(AgentLoop::new().with_max_rounds(3))
            .run(
                &mut c,
                &engine,
                fs(),
                None,
                CancellationToken::new(),
                AgentHooks::default(),
            )
            .await
            .unwrap();

        assert!(out.report.hit_round_limit);
        assert_eq!(out.report.rounds, 3);
        assert!(out.report.findings.is_empty());
    }

    #[test]
    fn instrukce_pro_chat_zmini_slozku_a_zakaze_hadani() {
        let root = if cfg!(windows) {
            PathBuf::from(r"E:\Projects\Anvil")
        } else {
            PathBuf::from("/home/dev/anvil")
        };
        let s = workspace_chat_system(&Workspace::new(root).unwrap());

        assert!(s.to_lowercase().contains("anvil"), "{s}");
        assert!(s.contains("Nehádej"), "{s}");
        assert!(s.contains("česky"), "{s}");
    }
}
