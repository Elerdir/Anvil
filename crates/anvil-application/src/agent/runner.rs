//! Agentní smyčka.
//!
//! Model dostane otázku a popis nástrojů. Když si nějaký vyžádá, smyčka ho
//! ověří, provede a výsledek pošle zpátky jako další tah. Opakuje se, dokud
//! model odpoví bez volání — nebo dokud nedojdou kola.
//!
//! Tři pojistky, bez kterých je smyčka s malým modelem nepoužitelná:
//!
//! 1. **Ověření před provedením.** Volání projde [`ToolSpec::validate`] a při
//!    chybě se model dozví přesně, co opravit. Chybné kolo se tím nepromarní.
//! 2. **Limit po sobě jdoucích chyb.** Když se model nedokáže trefit ani na
//!    několikátý pokus, smyčka skončí. Bez toho se točí, dokud nedojdou kola,
//!    a uživatel čeká minuty na nic.
//! 3. **Limit kol.** Model, který si pořád dokola čte tentýž soubor, musí
//!    narazit na strop. Že se na něj narazilo, se hlásí — „nenašel jsem nic"
//!    a „došla kola" jsou dvě velmi různé věci.

use std::sync::Arc;

use anvil_domain::{
    conversation::{Conversation, Message, Role},
    error::{DomainError, DomainResult},
    model::Sampling,
    ports::{ChatEngine, CompletionRequest, ProgressCallback},
    tool::ToolResult,
};
use tokio_util::sync::CancellationToken;

use super::{
    protocol::{self, ParsedResponse},
    tools::{RunArtifacts, Toolbox},
};

/// Co se během běhu stalo — pro UI, ať uživatel nekouká na prázdné okno.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    RoundStarted {
        round: u32,
    },
    /// Model si vyžádal nástroj.
    ToolCalled {
        name: String,
        summary: String,
    },
    /// Nástroj doběhl. `ok = false` znamená, že model dostal chybu.
    ToolFinished {
        name: String,
        ok: bool,
    },
    /// Text mimo volání nástrojů — průběžná úvaha modelu.
    Prose {
        text: String,
    },
}

pub type AgentEventCallback = Arc<dyn Fn(AgentEvent) + Send + Sync + 'static>;

/// Napojení na průběh. Obojí je volitelné a cestuje spolu — UI chce vědět
/// jak o krocích smyčky, tak o tokenech, které zrovna přibývají.
#[derive(Clone, Default)]
pub struct AgentHooks {
    /// Kroky smyčky: nové kolo, volání nástroje, jeho výsledek.
    pub on_event: Option<AgentEventCallback>,
    /// Tokeny odpovědi, jak přicházejí.
    pub on_progress: Option<ProgressCallback>,
}

impl AgentHooks {
    pub fn events(callback: AgentEventCallback) -> Self {
        Self {
            on_event: Some(callback),
            on_progress: None,
        }
    }

    pub fn with_progress(mut self, callback: Option<ProgressCallback>) -> Self {
        self.on_progress = callback;
        self
    }
}

#[derive(Debug)]
pub struct AgentOutcome {
    /// Finální odpověď modelu (text posledního kola bez volání nástrojů).
    pub text: String,
    pub artifacts: RunArtifacts,
    pub rounds: u32,
    /// Skončilo se na limitu kol, ne proto, že model dokončil práci.
    pub hit_round_limit: bool,
    /// Skončilo se na tom, že model opakovaně posílal nepoužitelná volání.
    pub gave_up_on_invalid: bool,
    /// Součet tokenů promptu přes všechna kola — hlavní složka čekání.
    pub prompt_tokens: u32,
    pub generated_tokens: u32,
    pub total_ms: u64,
}

pub struct AgentLoop {
    max_rounds: u32,
    max_consecutive_invalid: u32,
    max_tokens_per_round: u32,
}

impl AgentLoop {
    /// Výchozí meze.
    ///
    /// Dvanáct kol vychází z toho, jak dlouho jedno trvá: při ~27 tokenech za
    /// sekundu na zpracování promptu a rostoucí historii je dvanáct kol už
    /// několik minut čekání. Víc by znamenalo, že uživatel neví, jestli to
    /// ještě běží, nebo se to zaseklo.
    pub const DEFAULT_MAX_ROUNDS: u32 = 12;
    /// Tři pokusy na jedno volání. Když se model netrefí ani potřetí,
    /// netrefí se ani po deváté — jen to bude trvat třikrát dýl.
    pub const DEFAULT_MAX_INVALID: u32 = 3;

    pub fn new() -> Self {
        Self {
            max_rounds: Self::DEFAULT_MAX_ROUNDS,
            max_consecutive_invalid: Self::DEFAULT_MAX_INVALID,
            max_tokens_per_round: 1_024,
        }
    }

    pub fn with_max_rounds(mut self, rounds: u32) -> Self {
        self.max_rounds = rounds.clamp(1, 64);
        self
    }

    pub fn with_max_invalid(mut self, attempts: u32) -> Self {
        self.max_consecutive_invalid = attempts.clamp(1, 10);
        self
    }

    pub fn with_max_tokens_per_round(mut self, tokens: u32) -> Self {
        self.max_tokens_per_round = tokens.max(128);
        self
    }

    /// Odpracuje zadání. Konverzace se mění na místě — po návratu je v ní
    /// celý průběh včetně volání nástrojů a jejich výsledků.
    pub async fn run(
        &self,
        conversation: &mut Conversation,
        engine: &Arc<dyn ChatEngine>,
        toolbox: &Toolbox,
        system_prompt: &str,
        cancel: CancellationToken,
        hooks: AgentHooks,
    ) -> DomainResult<AgentOutcome> {
        let AgentHooks {
            on_event,
            on_progress,
        } = hooks;

        let system = format!(
            "{system_prompt}\n\n{}",
            protocol::tool_instructions(&toolbox.specs())
        );

        let mut rounds = 0;
        let mut invalid_streak = 0;
        let mut prompt_tokens = 0;
        let mut generated_tokens = 0;
        let mut total_ms = 0;
        let mut posledni_text = String::new();
        let mut hit_round_limit = false;
        let mut gave_up = false;

        while rounds < self.max_rounds {
            if cancel.is_cancelled() {
                return Err(DomainError::Cancelled);
            }

            rounds += 1;
            emit(&on_event, AgentEvent::RoundStarted { round: rounds });

            let request = CompletionRequest::new(conversation.visible_messages().to_vec())
                .with_system(system.clone())
                .with_summary(conversation.summary.clone())
                .with_max_tokens(self.max_tokens_per_round)
                // Analýza kódu má být reprodukovatelná; kreativita tu škodí,
                // protože se projeví jako vymyšlené názvy souborů.
                .with_sampling(Sampling::PRECISE);

            let outcome = engine
                .complete(&request, cancel.clone(), on_progress.clone())
                .await?;

            prompt_tokens += outcome.prompt_tokens;
            generated_tokens += outcome.generated_tokens;
            total_ms += outcome.total_ms;

            let parsed = protocol::parse_response(&outcome.text);

            if !parsed.prose.is_empty() {
                emit(
                    &on_event,
                    AgentEvent::Prose {
                        text: parsed.prose.clone(),
                    },
                );
            }

            // Žádné volání = model dořekl, co chtěl.
            if !parsed.wants_tools() {
                posledni_text = parsed.prose;
                if !posledni_text.is_empty() {
                    conversation.push(
                        Message::assistant(&posledni_text)
                            .with_token_count(outcome.generated_tokens),
                    );
                }
                break;
            }

            // Do historie jde surová odpověď včetně bloků nástrojů — model
            // musí ve svém kontextu vidět, o co si řekl. Bez toho by si
            // v dalším kole vyžádal totéž.
            conversation.push(
                Message::assistant(outcome.text.trim()).with_token_count(outcome.generated_tokens),
            );

            let (vysledky, vse_spatne) = self.execute(&parsed, toolbox, &on_event).await;
            conversation.push(Message::new(Role::Tool, vysledky));

            if vse_spatne {
                invalid_streak += 1;
                if invalid_streak >= self.max_consecutive_invalid {
                    gave_up = true;
                    posledni_text = format!(
                        "Model se {}× po sobě nedokázal trefit do formátu volání nástrojů, \
                         takže jsem to zastavil. Zkus dotaz přeformulovat, nebo přepni na jiný model.",
                        invalid_streak
                    );
                    break;
                }
            } else {
                invalid_streak = 0;
            }

            if rounds >= self.max_rounds {
                hit_round_limit = true;
            }
        }

        Ok(AgentOutcome {
            text: posledni_text,
            artifacts: toolbox.take_artifacts(),
            rounds,
            hit_round_limit,
            gave_up_on_invalid: gave_up,
            prompt_tokens,
            generated_tokens,
            total_ms,
        })
    }

    /// Provede všechna volání z jednoho kola. Vrací text pro model a `true`,
    /// když ani jedno volání neprošlo.
    async fn execute(
        &self,
        parsed: &ParsedResponse,
        toolbox: &Toolbox,
        on_event: &Option<AgentEventCallback>,
    ) -> (String, bool) {
        let mut out = String::new();
        let mut uspech = 0;

        // Nečitelné bloky první — model má vidět, co s nimi bylo, dřív než
        // výsledky toho, co prošlo.
        for spatny in &parsed.malformed {
            out.push_str(&format!(
                "CHYBA formátu: {}\nBlok: {}\n\n",
                spatny.reason, spatny.raw
            ));
        }

        for call in &parsed.calls {
            let Some(tool) = toolbox.find(&call.name) else {
                out.push_str(&format!(
                    "CHYBA: nástroj '{}' neexistuje. Dostupné: {}.\n\n",
                    call.name,
                    toolbox.names().join(", ")
                ));
                emit(
                    on_event,
                    AgentEvent::ToolFinished {
                        name: call.name.clone(),
                        ok: false,
                    },
                );
                continue;
            };

            let spec = tool.spec();
            let args = match spec.validate(call) {
                Ok(args) => args,
                Err(duvod) => {
                    out.push_str(&format!("CHYBA volání '{}': {duvod}\n\n", call.name));
                    emit(
                        on_event,
                        AgentEvent::ToolFinished {
                            name: call.name.clone(),
                            ok: false,
                        },
                    );
                    continue;
                }
            };

            emit(
                on_event,
                AgentEvent::ToolCalled {
                    name: call.name.clone(),
                    summary: shrnout_argumenty(&args),
                },
            );

            let ToolResult { content, is_error } = tool.call(&args).await;
            if !is_error {
                uspech += 1;
            }
            emit(
                on_event,
                AgentEvent::ToolFinished {
                    name: call.name.clone(),
                    ok: !is_error,
                },
            );

            out.push_str(&format!(
                "{} {}:\n{content}\n\n",
                if is_error { "CHYBA" } else { "Výsledek" },
                call.name
            ));
        }

        let vse_spatne = uspech == 0;
        (out.trim_end().to_string(), vse_spatne)
    }
}

impl Default for AgentLoop {
    fn default() -> Self {
        Self::new()
    }
}

/// Krátký popis argumentů do události pro UI. Celý JSON by se do řádku nevešel.
fn shrnout_argumenty(args: &serde_json::Value) -> String {
    args.as_object()
        .map(|o| {
            o.iter()
                .map(|(k, v)| match v.as_str() {
                    Some(s) => format!("{k}={s}"),
                    None => format!("{k}={v}"),
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn emit(cb: &Option<AgentEventCallback>, event: AgentEvent) {
    if let Some(cb) = cb {
        cb(event);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anvil_domain::ports::WorkspaceFs;

    use super::super::tools::fake_fs::FakeFs;
    use super::*;
    use crate::testing::{scripted, ScriptedEngine, ScriptedResponse};

    fn fs() -> Arc<dyn WorkspaceFs> {
        Arc::new(FakeFs::new(&[
            (
                "src/main.rs",
                "fn main() {\n    let x = neco().unwrap();\n}",
            ),
            ("src/lib.rs", "pub fn neco() -> Option<i32> { None }"),
        ]))
    }

    fn volani(json: &str) -> String {
        format!("<tool>{json}</tool>")
    }

    async fn spustit(odpovedi: Vec<ScriptedResponse>) -> (AgentOutcome, Conversation) {
        spustit_s(AgentLoop::new(), odpovedi).await
    }

    async fn spustit_s(
        smycka: AgentLoop,
        odpovedi: Vec<ScriptedResponse>,
    ) -> (AgentOutcome, Conversation) {
        let engine = scripted(ScriptedEngine::new(odpovedi));
        let toolbox = Toolbox::for_review(fs());
        let mut c = Conversation::new("test");
        c.push(Message::user("Zkontroluj projekt."));

        let out = smycka
            .run(
                &mut c,
                &engine,
                &toolbox,
                "Jsi reviewer.",
                CancellationToken::new(),
                AgentHooks::default(),
            )
            .await
            .expect("smyčka nemá selhat");
        (out, c)
    }

    // --- základní průběh ---

    #[tokio::test]
    async fn odpoved_bez_nastroju_skonci_hned() {
        let (out, c) = spustit(vec![ScriptedResponse::text("Vypadá to v pořádku.")]).await;

        assert_eq!(out.rounds, 1);
        assert_eq!(out.text, "Vypadá to v pořádku.");
        assert!(!out.hit_round_limit);
        assert_eq!(c.messages.last().unwrap().role, Role::Assistant);
    }

    #[tokio::test]
    async fn volani_nastroje_a_pak_odpoved() {
        let (out, c) = spustit(vec![
            ScriptedResponse::text(volani(
                r#"{"name":"read_file","arguments":{"path":"src/main.rs"}}"#,
            )),
            ScriptedResponse::text("Na řádku 2 je unwrap."),
        ])
        .await;

        assert_eq!(out.rounds, 2);
        assert_eq!(out.text, "Na řádku 2 je unwrap.");
        assert_eq!(out.artifacts.files_read.len(), 1);

        // Historie musí obsahovat, o co si model řekl, i co dostal — jinak
        // by si v dalším kole vyžádal totéž.
        let role: Vec<_> = c.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            role,
            vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]
        );
        assert!(
            c.messages[2].content.contains("fn main()"),
            "{}",
            c.messages[2].content
        );
    }

    #[tokio::test]
    async fn nalezy_se_posbiraji() {
        let (out, _) = spustit(vec![
            ScriptedResponse::text(volani(
                r#"{"name":"report_finding","arguments":{"file":"src/main.rs","line":2,"severity":"warning","summary":"unwrap na None"}}"#,
            )),
            ScriptedResponse::text("Hotovo."),
        ])
        .await;

        assert_eq!(out.artifacts.findings.len(), 1);
        assert_eq!(out.artifacts.findings[0].line, Some(2));
    }

    #[tokio::test]
    async fn vic_volani_v_jednom_kole() {
        let (out, _) = spustit(vec![
            ScriptedResponse::text(format!(
                "{}{}",
                volani(r#"{"name":"read_file","arguments":{"path":"src/main.rs"}}"#),
                volani(r#"{"name":"read_file","arguments":{"path":"src/lib.rs"}}"#)
            )),
            ScriptedResponse::text("Přečteno."),
        ])
        .await;

        assert_eq!(out.artifacts.files_read.len(), 2);
    }

    // --- pojistky ---

    #[tokio::test]
    async fn neplatne_volani_dostane_model_zpatky_s_navodem() {
        let (out, c) = spustit(vec![
            // Chybí povinný `path`.
            ScriptedResponse::text(volani(r#"{"name":"read_file","arguments":{}}"#)),
            ScriptedResponse::text("Aha, chyběla cesta."),
        ])
        .await;

        assert_eq!(out.rounds, 2, "chybné volání nesmí smyčku ukončit");
        let zpatky = &c.messages[2].content;
        assert!(zpatky.contains("path"), "{zpatky}");
        assert!(zpatky.contains("CHYBA"), "{zpatky}");
    }

    #[tokio::test]
    async fn vymysleny_nastroj_dostane_seznam_existujicich() {
        let (_, c) = spustit(vec![
            ScriptedResponse::text(volani(r#"{"name":"delete_everything","arguments":{}}"#)),
            ScriptedResponse::text("Dobře."),
        ])
        .await;

        let zpatky = &c.messages[2].content;
        assert!(zpatky.contains("neexistuje"), "{zpatky}");
        assert!(zpatky.contains("read_file"), "{zpatky}");
    }

    #[tokio::test]
    async fn rozbity_json_se_ohlasi_a_smycka_pokracuje() {
        let (out, c) = spustit(vec![
            ScriptedResponse::text("<tool>{tohle není JSON}</tool>"),
            ScriptedResponse::text("Zkusím to jinak."),
        ])
        .await;

        assert_eq!(out.rounds, 2);
        assert!(
            c.messages[2].content.contains("CHYBA formátu"),
            "{}",
            c.messages[2].content
        );
    }

    #[tokio::test]
    async fn opakovane_chybna_volani_smycku_zastavi() {
        // Bez tohohle se model točí, dokud nedojdou kola, a uživatel čeká
        // minuty na nic.
        let spatne = ScriptedResponse::text(volani(r#"{"name":"read_file","arguments":{}}"#));
        let (out, _) = spustit_s(
            AgentLoop::new().with_max_invalid(3),
            vec![spatne.clone(), spatne.clone(), spatne.clone(), spatne],
        )
        .await;

        assert!(out.gave_up_on_invalid);
        assert_eq!(
            out.rounds, 3,
            "má se zastavit na třetím pokusu, ne až na limitu kol"
        );
        assert!(out.text.contains("nedokázal trefit"), "{}", out.text);
    }

    #[tokio::test]
    async fn uspesne_volani_vynuluje_pocitadlo_chyb() {
        let spatne = ScriptedResponse::text(volani(r#"{"name":"read_file","arguments":{}}"#));
        let dobre = ScriptedResponse::text(volani(
            r#"{"name":"read_file","arguments":{"path":"src/main.rs"}}"#,
        ));

        let (out, _) = spustit_s(
            AgentLoop::new().with_max_invalid(2),
            vec![
                spatne.clone(),
                dobre,
                spatne.clone(),
                spatne,
                ScriptedResponse::text("konec"),
            ],
        )
        .await;

        // Kdyby se počítadlo nenulovalo, skončilo by se dřív.
        assert!(out.gave_up_on_invalid);
        assert_eq!(out.rounds, 4);
    }

    #[tokio::test]
    async fn limit_kol_se_ohlasi() {
        // „Nenašel jsem nic" a „došla kola" jsou dvě velmi různé věci.
        let porad_dokola = ScriptedEngine::always(volani(
            r#"{"name":"read_file","arguments":{"path":"src/main.rs"}}"#,
        ));
        let engine = scripted(porad_dokola);
        let toolbox = Toolbox::for_review(fs());
        let mut c = Conversation::new("t");
        c.push(Message::user("dotaz"));

        let out = AgentLoop::new()
            .with_max_rounds(4)
            .run(
                &mut c,
                &engine,
                &toolbox,
                "systém",
                CancellationToken::new(),
                AgentHooks::default(),
            )
            .await
            .unwrap();

        assert_eq!(out.rounds, 4);
        assert!(out.hit_round_limit);
    }

    #[tokio::test]
    async fn zruseni_smycku_ukonci() {
        let cancel = CancellationToken::new();
        cancel.cancel();

        let engine = scripted(ScriptedEngine::always("nemělo se stát"));
        let toolbox = Toolbox::for_review(fs());
        let mut c = Conversation::new("t");

        let err = AgentLoop::new()
            .run(
                &mut c,
                &engine,
                &toolbox,
                "s",
                cancel,
                AgentHooks::default(),
            )
            .await
            .unwrap_err();
        assert!(err.is_cancelled());
    }

    // --- instrukce a události ---

    #[tokio::test]
    async fn model_dostane_popis_nastroju_v_systemovem_promptu() {
        let engine = Arc::new(ScriptedEngine::with_texts(["ok"]));
        let port: Arc<dyn ChatEngine> = engine.clone();
        let toolbox = Toolbox::for_review(fs());
        let mut c = Conversation::new("t");
        c.push(Message::user("dotaz"));

        AgentLoop::new()
            .run(
                &mut c,
                &port,
                &toolbox,
                "Jsi reviewer.",
                CancellationToken::new(),
                AgentHooks::default(),
            )
            .await
            .unwrap();

        let system = engine.last_call().system.unwrap_or_default();
        assert!(system.contains("Jsi reviewer."), "{system}");
        assert!(system.contains("read_file"), "{system}");
        assert!(system.contains("<tool>"), "{system}");
    }

    #[tokio::test]
    async fn udalosti_hlasi_prubeh() {
        let sebrane = Arc::new(Mutex::new(Vec::<String>::new()));
        let sber = sebrane.clone();

        let engine = scripted(ScriptedEngine::new(vec![
            ScriptedResponse::text(format!(
                "Podívám se.{}",
                volani(r#"{"name":"read_file","arguments":{"path":"src/main.rs"}}"#)
            )),
            ScriptedResponse::text("Hotovo."),
        ]));
        let toolbox = Toolbox::for_review(fs());
        let mut c = Conversation::new("t");
        c.push(Message::user("dotaz"));

        AgentLoop::new()
            .run(
                &mut c,
                &engine,
                &toolbox,
                "s",
                CancellationToken::new(),
                AgentHooks::events(Arc::new(move |e: AgentEvent| {
                    sber.lock().unwrap().push(match e {
                        AgentEvent::RoundStarted { round } => format!("kolo {round}"),
                        AgentEvent::ToolCalled { name, .. } => format!("volám {name}"),
                        AgentEvent::ToolFinished { name, ok } => format!("hotovo {name} ok={ok}"),
                        AgentEvent::Prose { .. } => "text".into(),
                    });
                })),
            )
            .await
            .unwrap();

        let u = sebrane.lock().unwrap().clone();
        assert!(u.contains(&"kolo 1".to_string()), "{u:?}");
        assert!(u.contains(&"volám read_file".to_string()), "{u:?}");
        assert!(u.contains(&"hotovo read_file ok=true".to_string()), "{u:?}");
        assert!(u.contains(&"text".to_string()), "{u:?}");
    }

    #[tokio::test]
    async fn statistiky_scitaji_vsechna_kola() {
        let (out, _) = spustit(vec![
            ScriptedResponse::text(volani(
                r#"{"name":"read_file","arguments":{"path":"src/main.rs"}}"#,
            )),
            ScriptedResponse::text("Hotovo."),
        ])
        .await;

        assert!(out.generated_tokens > 0);
        assert!(out.prompt_tokens > 0);
        assert_eq!(out.rounds, 2);
    }

    #[tokio::test]
    async fn meze_se_orizavaji_do_rozumu() {
        assert_eq!(AgentLoop::new().with_max_rounds(0).max_rounds, 1);
        assert_eq!(AgentLoop::new().with_max_rounds(999).max_rounds, 64);
        assert_eq!(
            AgentLoop::new().with_max_invalid(0).max_consecutive_invalid,
            1
        );
    }
}
