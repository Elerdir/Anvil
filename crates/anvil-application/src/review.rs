//! Code review nad otevřenou složkou.
//!
//! Projde se soubor po souboru a každý se modelu předloží zvlášť. Který
//! soubor přijde na řadu, rozhoduje kód, ne model — viz [ReviewService::run].
//!
//! [ReviewService::run]: ReviewService::run

use std::sync::{Arc, Mutex};

use anvil_domain::{
    conversation::{Conversation, Message},
    error::{DomainError, DomainResult},
    ports::{ChatEngine, WorkspaceFs},
    review::ReviewReport,
    workspace::{RelativePath, Workspace},
};
use tokio_util::sync::CancellationToken;

use crate::agent::{
    runner::{AgentEvent, AgentHooks, AgentLoop, AgentOutcome},
    tools::{RunArtifacts, SharedArtifacts, Toolbox},
};

/// Instrukce pro průchod jedním souborem.
///
/// Krátká schválně: posílá se znovu ke každému souboru, takže každá věta se
/// zaplatí tolikrát, kolik má projekt souborů.
const SOUBOR_SYSTEM: &str = "Jsi zkušený programátor a hledáš chyby v jednom souboru.

Hlídej hlavně: pády za běhu (indexy mimo rozsah, dělení nulou, podtečení
u odčítání na bezznaménkových typech), tiše ignorované chyby, chybějící
ověření vstupu od uživatele, a místa, kde kód dělá něco jiného, než co
slibuje jeho název nebo komentář.

Neřeš formátování ani styl — od toho jsou nástroje. Netipuj: hlas jen to,
co je v tomhle souboru vidět. Testovací kód (`#[cfg(test)]`, `mod tests`)
posuzuj mírně, `unwrap()` je v něm běžný a v pořádku.

Číslo řádku ber z výpisu vlevo. Radši méně nálezů a jistých než seznam
dohadů.";

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
    max_files: u32,
}

impl ReviewService {
    /// Kolik souborů se projde, než se to vzdá.
    ///
    /// Jeden soubor stojí pár desítek sekund, takže sto souborů je hodina.
    /// Strop je radši nízký a **přiznaný** — v shrnutí i v reportu stojí,
    /// kolik se toho prošlo z kolika — než vysoký a mlčky nedodržený.
    pub const DEFAULT_MAX_FILES: u32 = 25;

    pub fn new() -> Self {
        Self {
            // Jeden soubor, jeden nástroj: víc než pár kol není k čemu.
            // Kola tu nejsou na rozhlížení, jen na to, aby model stihl
            // nahlásit několik nálezů za sebou.
            agent: AgentLoop::new().with_max_rounds(4),
            max_files: Self::DEFAULT_MAX_FILES,
        }
    }

    pub fn with_agent(agent: AgentLoop) -> Self {
        Self {
            agent,
            max_files: Self::DEFAULT_MAX_FILES,
        }
    }

    pub fn with_max_files(mut self, files: u32) -> Self {
        self.max_files = files.max(1);
        self
    }

    /// Projde projekt soubor po souboru a vrátí nálezy.
    ///
    /// **Které soubory se projdou, rozhoduje tenhle kód, ne model.** Původní
    /// verze pustila model na projekt a nechala ho, ať se rozhlédne sám; dva
    /// běhy nad stejným kódem pak otevřely úplně jiné soubory (4 z 8 a 1 z 8)
    /// a ten druhý našel jedinou bezpečnostní díru v projektu právě proto, že
    /// náhodou sáhl na správný soubor. Pokrytí, které závisí na vzorkování,
    /// není pokrytí.
    ///
    /// Cenou je, že se každý soubor posuzuje sám za sebe — chyba, která je
    /// vidět až ze souvislosti dvou souborů, tímhle neprojde. Na to je běžný
    /// chat nad otevřenou složkou, kde nástroje zůstávají modelu k dispozici.
    ///
    /// `focus` je volitelné zúžení od uživatele. Bere se jako vzor cesty
    /// („src/ai/**"), a když jako vzor nesedí na nic, jako pokyn k zadání.
    pub async fn run(
        &self,
        conversation: &mut Conversation,
        engine: &Arc<dyn ChatEngine>,
        fs: Arc<dyn WorkspaceFs>,
        focus: Option<&str>,
        cancel: CancellationToken,
        hooks: AgentHooks,
    ) -> DomainResult<ReviewOutcome> {
        let focus = focus.map(str::trim).filter(|f| !f.is_empty());
        let zadani = match focus {
            Some(f) => format!("Projdi projekt a zaměř se na tohle: {f}"),
            None => "Projdi projekt a najdi problémy.".to_string(),
        };
        conversation.push(Message::user(&zadani));
        conversation.derive_title();

        let vsechny = self.soubory_k_projiti(&fs, focus).await?;
        let files_total = vsechny.len() as u32;
        let projdou: Vec<RelativePath> =
            vsechny.into_iter().take(self.max_files as usize).collect();

        let artifacts: SharedArtifacts = Arc::new(Mutex::new(RunArtifacts::default()));
        let toolbox = Toolbox::for_single_file(artifacts);
        let mut nalezy = Vec::new();

        let mut rounds = 0;
        let mut prompt_tokens = 0;
        let mut generated_tokens = 0;
        let mut total_ms = 0;
        let mut hit_round_limit = false;
        let mut precteno: Vec<RelativePath> = Vec::new();

        for (i, path) in projdou.iter().enumerate() {
            if cancel.is_cancelled() {
                return Err(DomainError::Cancelled);
            }
            oznam(
                &hooks,
                AgentEvent::Step {
                    done: i as u32,
                    total: projdou.len() as u32,
                    label: path.to_string(),
                },
            );

            let Some(zadani) = self.zadani_pro_soubor(&fs, path, focus).await else {
                continue;
            };

            // Každý soubor má vlastní konverzaci. Kdyby se navazovalo, rostl
            // by prompt s každým souborem a poslední by stál desetkrát víc
            // než první — přesně to dělalo původní review nepoužitelným.
            let mut vlakno = Conversation::new("");
            vlakno.push(Message::user(zadani));

            let outcome: AgentOutcome = self
                .agent
                .run(
                    &mut vlakno,
                    engine,
                    &toolbox,
                    SOUBOR_SYSTEM,
                    cancel.clone(),
                    hooks.clone(),
                )
                .await?;

            rounds += outcome.rounds;
            prompt_tokens += outcome.prompt_tokens;
            generated_tokens += outcome.generated_tokens;
            total_ms += outcome.total_ms;
            hit_round_limit |= outcome.hit_round_limit;
            // Smyčka si artefakty na konci odebere, takže se sbírají odsud,
            // ne ze sdílené schránky — ta je po každém průchodu prázdná.
            nalezy.extend(outcome.artifacts.findings);
            precteno.push(path.clone());
        }

        oznam(
            &hooks,
            AgentEvent::Step {
                done: projdou.len() as u32,
                total: projdou.len() as u32,
                label: "hotovo".into(),
            },
        );

        let report = ReviewReport {
            findings: nalezy,
            files_read: precteno,
            rounds,
            hit_round_limit,
            files_total,
        };

        // Shrnutí se skládá z nálezů, ne od modelu. Když ho psal model, tvrdil
        // v něm věci o souborech, které nikdy neotevřel — a bylo to první, co
        // uživatel v okně přečetl.
        let summary = shrnuti(&report);
        conversation.push(Message::assistant(&summary));

        Ok(ReviewOutcome {
            report,
            summary,
            prompt_tokens,
            generated_tokens,
            total_ms,
        })
    }

    /// Soubory k projití, vždy ve stejném pořadí.
    ///
    /// `focus` se nejdřív zkusí jako vzor cesty. Když na nic nesedne, není to
    /// vzor ale pokyn („podívej se na ošetření chyb") a projde se všechno —
    /// tiše nevrátit nic by znamenalo review, které nic neprošlo a tváří se,
    /// že je čisto.
    async fn soubory_k_projiti(
        &self,
        fs: &Arc<dyn WorkspaceFs>,
        focus: Option<&str>,
    ) -> DomainResult<Vec<RelativePath>> {
        let mut soubory = match focus {
            Some(vzor) => {
                let vybrane = fs.list(Some(vzor)).await?;
                if vybrane.is_empty() {
                    fs.list(None).await?
                } else {
                    vybrane
                }
            }
            None => fs.list(None).await?,
        };
        // Stejné pořadí při každém běhu — bez toho by se při dosažení stropu
        // pokaždé prošly jiné soubory a výsledky by se nedaly srovnat.
        soubory.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(soubory)
    }

    /// Zadání pro jeden soubor: obsah s čísly řádků a co s ním.
    /// `None`, když soubor nejde přečíst — binárka, práva, zmizel mezitím.
    async fn zadani_pro_soubor(
        &self,
        fs: &Arc<dyn WorkspaceFs>,
        path: &RelativePath,
        focus: Option<&str>,
    ) -> Option<String> {
        let slice = fs.read(path, None, None).await.ok()?;
        if slice.text.trim().is_empty() {
            return None;
        }

        let cislovane: String = slice
            .text
            .lines()
            .enumerate()
            .map(|(i, r)| format!("{:>5} | {r}\n", slice.start_line as usize + i))
            .collect();

        // Že je soubor delší, než co model dostal, musí být vidět. Jinak by
        // o zbytku prohlásil, že je v pořádku.
        let orez = if slice.end_line() < slice.total_lines {
            format!(
                "\n(Soubor má {} řádků, tohle je prvních {}. O zbytku nic netvrď.)\n",
                slice.total_lines,
                slice.end_line()
            )
        } else {
            String::new()
        };

        let zamereni = match focus {
            Some(f) => format!("\nZaměř se hlavně na tohle: {f}\n"),
            None => String::new(),
        };

        Some(format!(
            "Soubor `{path}`:\n\n```\n{cislovane}```\n{orez}{zamereni}\n\
             Najdi v něm problémy a každý nahlas přes `report_finding`. \
             Když je v pořádku, odpověz jednou větou a nic nehlas."
        ))
    }
}

/// Shrnutí poskládané z nálezů.
///
/// Schválně bez modelu: tohle je věta, kterou uživatel přečte první, a nesmí
/// v ní být nic, co neplyne z toho, co se opravdu našlo.
fn shrnuti(report: &ReviewReport) -> String {
    let mut s = report.headline();

    if report.files_read.len() < report.files_total as usize {
        s.push_str(&format!(
            " Prošlo se {} souborů z {}; o zbytku tenhle výsledek nic neříká.",
            report.files_read.len(),
            report.files_total
        ));
    }

    if report.hit_round_limit {
        s.push_str(" U některého souboru došla kola, takže tam mohlo něco zůstat.");
    }

    s
}

fn oznam(hooks: &AgentHooks, event: AgentEvent) {
    if let Some(cb) = &hooks.on_event {
        cb(event);
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
    async fn projdou_se_vsechny_soubory_bez_ohledu_na_model() {
        // Jádro celé změny: pokrytí nezávisí na tom, co si model vybere.
        // Model tu neřekne o žádný soubor a přesto se projdou oba.
        let engine = Arc::new(ScriptedEngine::always("V pořádku."));
        let port: Arc<dyn ChatEngine> = engine.clone();
        let mut c = Conversation::new("");

        let out = ReviewService::new()
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

        let prosle: Vec<&str> = out.report.files_read.iter().map(|p| p.as_str()).collect();
        assert_eq!(prosle, vec!["src/lib.rs", "src/main.rs"], "{prosle:?}");
        assert_eq!(out.report.files_total, 2);
    }

    #[tokio::test]
    async fn kazdy_soubor_dostane_svuj_obsah_a_cislovane_radky() {
        let engine = Arc::new(ScriptedEngine::always("V pořádku."));
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

        // Poslední průchod byl nad `src/main.rs` (řadí se abecedně).
        let zadani = &engine.last_call().messages[0].content;
        assert!(zadani.contains("src/main.rs"), "{zadani}");
        assert!(zadani.contains("f().unwrap()"), "{zadani}");
        assert!(
            zadani.contains("    2 |"),
            "bez čísel řádků nemá model odkud vzít číslo do nálezu: {zadani}"
        );
    }

    #[tokio::test]
    async fn prompt_neroste_se_soubory() {
        // Původní review vedlo jednu konverzaci přes celý projekt, takže
        // poslední soubor stál mnohonásobek prvního. Každý průchod teď
        // začíná načisto.
        let engine = Arc::new(ScriptedEngine::always("V pořádku."));
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

        assert_eq!(
            engine.last_call().messages.len(),
            1,
            "do posledního souboru se protáhla historie předchozích"
        );
    }

    #[tokio::test]
    async fn nalezy_se_posbiraji_napric_soubory() {
        let engine = scripted(ScriptedEngine::new(vec![
            // src/lib.rs
            ScriptedResponse::text(volani(
                r#"{"name":"report_finding","arguments":{"file":"src/lib.rs","line":1,"severity":"note","summary":"vrací pořád None"}}"#,
            )),
            ScriptedResponse::text("Hotovo."),
            // src/main.rs
            ScriptedResponse::text(volani(
                r#"{"name":"report_finding","arguments":{"file":"src/main.rs","line":2,"severity":"critical","summary":"unwrap na funkci, která vrací None"}}"#,
            )),
            ScriptedResponse::text("Hotovo."),
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

        assert_eq!(out.report.findings.len(), 2);
        assert_eq!(out.report.sorted()[0].severity, Severity::Critical);
    }

    #[tokio::test]
    async fn shrnuti_se_sklada_z_nalezu_ne_od_modelu() {
        // Když shrnutí psal model, tvrdil v něm věci o souborech, které
        // nikdy neotevřel — a bylo to první, co uživatel přečetl.
        let engine = scripted(ScriptedEngine::always(
            "Projekt je v naprostém pořádku, prošel jsem úplně všechno.",
        ));
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

        assert!(
            !out.summary.contains("naprostém pořádku"),
            "text modelu se dostal do shrnutí: {}",
            out.summary
        );
        assert_eq!(out.summary, out.report.headline());
    }

    #[tokio::test]
    async fn strop_souboru_se_prizna_ve_shrnuti() {
        let engine = scripted(ScriptedEngine::always("V pořádku."));
        let mut c = Conversation::new("");

        let out = ReviewService::new()
            .with_max_files(1)
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

        assert_eq!(out.report.files_read.len(), 1);
        assert_eq!(out.report.files_total, 2);
        assert!(
            out.summary.contains("1 souborů z 2"),
            "z shrnutí nejde poznat, že se prošla jen část: {}",
            out.summary
        );
    }

    #[tokio::test]
    async fn zamereni_jako_vzor_zuzi_vyber() {
        let engine = scripted(ScriptedEngine::always("V pořádku."));
        let mut c = Conversation::new("");

        let out = ReviewService::new()
            .run(
                &mut c,
                &engine,
                fs(),
                Some("**/main.rs"),
                CancellationToken::new(),
                AgentHooks::default(),
            )
            .await
            .unwrap();

        let prosle: Vec<&str> = out.report.files_read.iter().map(|p| p.as_str()).collect();
        assert_eq!(prosle, vec!["src/main.rs"]);
    }

    #[tokio::test]
    async fn zamereni_ktere_neni_vzor_se_projde_cele() {
        // „podívej se na ošetření chyb" není glob. Tiše neprojít nic a tvářit
        // se, že je čisto, by bylo to nejhorší možné chování.
        let engine = Arc::new(ScriptedEngine::always("V pořádku."));
        let port: Arc<dyn ChatEngine> = engine.clone();
        let mut c = Conversation::new("");

        let out = ReviewService::new()
            .run(
                &mut c,
                &port,
                fs(),
                Some("ošetření chyb"),
                CancellationToken::new(),
                AgentHooks::default(),
            )
            .await
            .unwrap();

        assert_eq!(out.report.files_read.len(), 2);
        // A zaměření se stejně dostane k modelu, jen jako pokyn.
        let zadani = &engine.last_call().messages[0].content;
        assert!(zadani.contains("ošetření chyb"), "{zadani}");
    }

    #[tokio::test]
    async fn prubeh_hlasi_kazdy_soubor() {
        let engine = scripted(ScriptedEngine::always("V pořádku."));
        let kroky = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sber = kroky.clone();
        let mut c = Conversation::new("");

        ReviewService::new()
            .run(
                &mut c,
                &engine,
                fs(),
                None,
                CancellationToken::new(),
                AgentHooks::events(Arc::new(move |e: AgentEvent| {
                    if let AgentEvent::Step { done, total, label } = e {
                        sber.lock().unwrap().push(format!("{done}/{total} {label}"));
                    }
                })),
            )
            .await
            .unwrap();

        let kroky = kroky.lock().unwrap().clone();
        assert_eq!(
            kroky,
            vec!["0/2 src/lib.rs", "1/2 src/main.rs", "2/2 hotovo"],
            "{kroky:?}"
        );
    }

    #[tokio::test]
    async fn instrukce_zakazuje_hadani_a_miri_na_pady() {
        let engine = Arc::new(ScriptedEngine::always("V pořádku."));
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
        assert!(system.contains("Netipuj"), "{system}");
        assert!(system.contains("report_finding"), "{system}");
        // Testovací kód se má posuzovat mírně — jinak review utopí uživatele
        // v hláškách o `unwrap()` v testech, což se skutečnému modelu stalo.
        assert!(system.contains("cfg(test)"), "{system}");
    }

    #[tokio::test]
    async fn zruseni_uprostred_prubehu_se_projevi() {
        let engine = scripted(ScriptedEngine::always("V pořádku."));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut c = Conversation::new("");

        let err = ReviewService::new()
            .run(&mut c, &engine, fs(), None, cancel, AgentHooks::default())
            .await
            .unwrap_err();

        assert!(matches!(err, anvil_domain::error::DomainError::Cancelled));
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
