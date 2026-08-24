//! Ověření **agentní smyčky** proti skutečnému modelu nad skutečnou složkou.
//!
//! [`smoke`](smoke.rs) ověřuje, že model vůbec mluví. Tenhle příklad ověřuje
//! něco jiného a mnohem křehčího: že model **umí ovládat nástroje**. Celá
//! druhá fáze na tom stojí, ale jednotkové testy to z principu neprokážou —
//! běží proti skriptovanému dvojníkovi, který odpovídá přesně tak, jak jsem
//! si to představoval. Reálný model se v tom nemusí trefit ani jednou.
//!
//! Co se tu měří a v testech změřit nejde:
//!
//! * jestli model vůbec zavolá nástroj, nebo si obsah projektu vymyslí;
//! * kolik volání je nepoužitelných (špatný formát, neznámý nástroj,
//!   chybějící parametr) — tohle rozhoduje, jestli je smyčka použitelná;
//! * jestli hlásí nálezy k souborům, které opravdu četl;
//! * kolik to celé stojí času, protože zpracování promptu roste s každým
//!   kolem a je to hlavní složka čekání.
//!
//! ```text
//! cargo run --release --example review --features engine-vulkan -- <model.gguf> <složka> [kontext] [zaměření…]
//! ```

#[cfg(not(feature = "engine"))]
fn main() {
    eprintln!(
        "Tenhle příklad potřebuje engine. Spusť ho s --features engine-vulkan \
         (Windows/Linux) nebo --features engine-metal (macOS)."
    );
    std::process::exit(1);
}

#[cfg(feature = "engine")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::{
        collections::BTreeMap,
        io::Write,
        sync::{Arc, Mutex},
    };

    use anvil_application::{
        agent::runner::{AgentEvent, AgentHooks},
        review::ReviewService,
    };
    use anvil_domain::{
        conversation::Conversation,
        model::{ChatTemplateKind, InferenceSettings, ModelId},
        ports::WorkspaceFs,
        workspace::Workspace,
    };
    use anvil_infrastructure::{ai::llama_engine::LlamaChatEngine, workspace_fs::LocalWorkspaceFs};
    use tokio_util::sync::CancellationToken;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ANVIL_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,llama_cpp_2=warn")),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let cesta = args.next().ok_or(
        "Chybí cesta ke GGUF souboru.\n\
         Použití: cargo run --release --example review --features engine-vulkan -- \
         <model.gguf> <složka> [kontext] [zaměření…]",
    )?;
    let slozka = args.next().ok_or("Chybí složka, kterou má model projít.")?;
    let kontext: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(16_384);
    let zamereni: String = args.collect::<Vec<_>>().join(" ");

    let cesta = std::path::PathBuf::from(cesta);
    if !cesta.is_file() {
        return Err(format!("{} neexistuje", cesta.display()).into());
    }
    // Workspace chce absolutní cestu; `.` je při ručním spouštění to nejběžnější.
    let koren = std::fs::canonicalize(&slozka)
        .map_err(|e| format!("složku {slozka} nejde otevřít: {e}"))?;

    let jmeno = cesta
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    let sablona = if jmeno.contains("gemma") {
        ChatTemplateKind::Gemma4
    } else {
        ChatTemplateKind::Qwen3
    };

    println!("Model:    {}", cesta.display());
    println!("Šablona:  {sablona:?}");
    println!("Kontext:  {kontext} tokenů");
    println!("Složka:   {}", koren.display());
    if !zamereni.is_empty() {
        println!("Zaměření: {zamereni}");
    }
    println!();

    let nacitani = std::time::Instant::now();
    let engine = tokio::task::spawn_blocking({
        let cesta = cesta.clone();
        move || {
            LlamaChatEngine::load(
                ModelId::parse("review").expect("platné ID"),
                &cesta,
                sablona,
                InferenceSettings::default().with_context(kontext),
            )
        }
    })
    .await??;

    println!("Načteno:  {:.1} s", nacitani.elapsed().as_secs_f64());
    println!("Plán:     {}", engine.plan_description());
    println!();

    let workspace = Workspace::new(koren)?;
    let fs: Arc<dyn WorkspaceFs> = Arc::new(LocalWorkspaceFs::new(workspace)?);
    let engine: Arc<dyn anvil_domain::ports::ChatEngine> = Arc::new(engine);

    // Průběh se sbírá i vypisuje. Vypisuje proto, aby bylo při desetiminutovém
    // běhu vidět, že se něco děje; sbírá proto, že teprve součty na konci
    // řeknou, jestli je smyčka použitelná.
    #[derive(Default)]
    struct Prubeh {
        volani: BTreeMap<String, u32>,
        chyby: BTreeMap<String, u32>,
        kol: u32,
    }
    let prubeh = Arc::new(Mutex::new(Prubeh::default()));

    let sber = prubeh.clone();
    let hooks = AgentHooks::events(Arc::new(move |e: AgentEvent| {
        let mut p = sber.lock().expect("zámek");
        match e {
            AgentEvent::RoundStarted { round } => {
                p.kol = round;
                println!("\n--- {round}. kolo ---");
            }
            AgentEvent::ToolCalled { name, summary } => {
                *p.volani.entry(name.clone()).or_default() += 1;
                println!("  → {name} {summary}");
            }
            AgentEvent::ToolFinished { name, ok } => {
                if !ok {
                    *p.chyby.entry(name.clone()).or_default() += 1;
                    println!("  ✗ {name} vrátil chybu");
                }
            }
            AgentEvent::Prose { text } => {
                let t = text.trim();
                if !t.is_empty() {
                    println!("  „{}“", zkratit(t, 160));
                }
            }
        }
        std::io::stdout().flush().ok();
    }));

    let mut conversation = Conversation::new("");
    let bezi = std::time::Instant::now();
    let outcome = ReviewService::new()
        .run(
            &mut conversation,
            &engine,
            fs,
            Some(zamereni.as_str()).filter(|z| !z.is_empty()),
            CancellationToken::new(),
            hooks,
        )
        .await?;

    let prubeh = prubeh.lock().expect("zámek");
    let report = &outcome.report;

    println!("\n=== Shrnutí od modelu ===");
    println!("{}", outcome.summary.trim());

    println!("\n=== Nálezy ({}) ===", report.findings.len());
    for f in report.sorted() {
        println!("  [{:?}] {} — {}", f.severity, f.location(), f.summary);
        if !f.detail.is_empty() {
            println!("        {}", zkratit(&f.detail, 200));
        }
    }

    println!("\n=== Průběh ===");
    println!("  kol:              {} z 12", report.rounds);
    println!("  přečtené soubory: {}", report.files_read.len());
    for path in &report.files_read {
        println!("      {path}");
    }
    println!("  volání nástrojů:");
    for (name, n) in &prubeh.volani {
        let chyb = prubeh.chyby.get(name).copied().unwrap_or(0);
        println!("      {name}: {n}× (z toho {chyb} s chybou)");
    }
    let volani_celkem: u32 = prubeh.volani.values().sum();
    let chyb_celkem: u32 = prubeh.chyby.values().sum();

    println!("\n=== Cena ===");
    println!("  prompt:      {} tokenů", outcome.prompt_tokens);
    println!("  vygenerováno:{} tokenů", outcome.generated_tokens);
    println!("  celkem:      {:.1} s", bezi.elapsed().as_secs_f64());

    // --- kontroly, které jednotkové testy neudělají ------------------------
    let mut problemy: Vec<String> = Vec::new();

    if volani_celkem == 0 {
        problemy.push(
            "Model nezavolal ani jeden nástroj. Buď nepochopil protokol `<tool>…</tool>`, \
             nebo mu instrukce nedošly — v obou případech je agentní smyčka s tímhle \
             modelem nepoužitelná a review je jen vymyšlený text."
                .into(),
        );
    }

    if report.files_read.is_empty() && !report.findings.is_empty() {
        problemy.push(
            "Model hlásí nálezy, ale nepřečetl ani jeden soubor. To si je nemohl \
             ověřit — hádá podle názvů z `list_files`."
                .into(),
        );
    }

    // `report_finding` schválně neověřuje, že model soubor opravdu četl —
    // nástroj by neměl modelu podsouvat, co smí nahlásit. Ověřuje se to až
    // tady, protože je to vlastnost modelu, ne nástroje.
    let precteno: Vec<&str> = report.files_read.iter().map(|p| p.as_str()).collect();
    let nepodlozene: Vec<String> = report
        .findings
        .iter()
        .filter(|f| !precteno.contains(&f.file.as_str()))
        .map(|f| f.location())
        .collect();
    if !nepodlozene.is_empty() {
        problemy.push(format!(
            "Nálezy k souborům, které model nikdy neotevřel: {}. Instrukce „nehádej“ \
             nestačí — buď ji zpřísnit, nebo takové nálezy odfiltrovat.",
            nepodlozene.join(", ")
        ));
    }

    if volani_celkem > 0 && chyb_celkem * 2 > volani_celkem {
        problemy.push(format!(
            "Víc než polovina volání skončila chybou ({chyb_celkem} z {volani_celkem}). \
             Chybové hlášky nejsou pro model dost srozumitelné, nebo mu nesedí formát."
        ));
    }

    if outcome.summary.contains("<tool>") || outcome.summary.contains("</tool>") {
        problemy
            .push("Do závěrečného shrnutí prosákly značky `<tool>` — parser je nevyzobal.".into());
    }

    if report.hit_round_limit {
        problemy.push(
            "Došla kola. Model se k závěru nedostal — buď se zacyklil, nebo je limit \
             na tenhle projekt nízký."
                .into(),
        );
    }

    println!();
    if problemy.is_empty() {
        println!("Agentní smyčka proti skutečnému modelu funguje.");
        Ok(())
    } else {
        for p in &problemy {
            eprintln!("[PROBLÉM] {p}");
        }
        Err(format!("{} problémů", problemy.len()).into())
    }
}

/// Zkrátí text na hranici znaku, ne bajtu — jinak by to na diakritice spadlo.
#[cfg(feature = "engine")]
fn zkratit(text: &str, max: usize) -> String {
    let jednoradkove = text.replace('\n', " ");
    if jednoradkove.chars().count() <= max {
        return jednoradkove;
    }
    jednoradkove.chars().take(max).collect::<String>() + "…"
}
