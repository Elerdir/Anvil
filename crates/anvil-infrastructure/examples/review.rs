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
//! * jestli hlásí nálezy k souborům, které opravdu četl — a jestli si to
//!   nedovolí aspoň v závěrečném shrnutí, které přes `report_finding` neteče
//!   a kontrolou nálezů proto projde;
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

    let workspace = Workspace::new(koren.clone())?;
    let fs: Arc<dyn WorkspaceFs> = Arc::new(LocalWorkspaceFs::new(workspace)?);
    // Kopie pro závěrečné kontroly — `run` si `fs` odnese.
    let fs_pro_kontrolu = fs.clone();
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

    // Seznam vsech souboru se pouziva na dvou mistech nize.
    let vsechny: Vec<String> = fs_pro_kontrolu
        .list(None)
        .await
        .unwrap_or_default()
        .iter()
        .map(|p| p.to_string())
        .collect();

    // --- srovnání se známými vadami ---------------------------------------
    //
    // Bez tohohle se nedá odlišit „projekt je čistý“ od „model chyby nenajde“.
    // Nad `anvil-domain` skončily tři běhy po sobě prakticky bez nálezu a
    // z toho neplyne nic — dokud není po ruce kód, o kterém se ví, co v něm
    // je, měří se jenom to, že smyčka doběhla.
    //
    // Klíč leží **vedle** kontrolované složky, ne v ní. Uvnitř by si ho model
    // přečetl `read_file` a test by měřil jeho schopnost číst zadání.
    let klic = koren.parent().map(|p| {
        p.join(format!(
            "{}-nalezy.json",
            koren.file_name().unwrap_or_default().to_string_lossy()
        ))
    });
    let ocekavane = klic
        .filter(|p| p.is_file())
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());

    let mut nenalezene: Vec<String> = Vec::new();
    if let Some(klic) = &ocekavane {
        let vady = klic["vady"].as_array().cloned().unwrap_or_default();

        // Párování je **jedna ku jedné**. První verze tuhle podmínku neměla
        // a jeden nález obsloužil dvě různé vady: hláška „ignoruje poslední
        // úlohu" obsahovala slovo „poslední", takže si připsala i vadu
        // `posledni() vrací first()` o deset řádků níž, kterou model vůbec
        // nenahlásil. Skóre pak ukázalo 5 ze 6 místo 3 ze 6 — měřidlo, které
        // nadhodnocuje, je horší než žádné, protože se podle něj rozhoduje.
        let mut obsazeny: Vec<bool> = vec![false; report.findings.len()];

        println!("\n=== Známé vady ({}) ===", vady.len());
        for vada in &vady {
            let soubor = vada["soubor"].as_str().unwrap_or_default();
            let radek = vada["radek"].as_u64().unwrap_or(0) as u32;
            let nazev = vada["nazev"].as_str().unwrap_or_default();
            let slova: Vec<String> = vada["klicova_slova"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str())
                        .map(str::to_lowercase)
                        .collect()
                })
                .unwrap_or_default();

            // Kvalita shody, menší je lepší. Řádek rozhoduje přednostně;
            // popis se bere jen tehdy, když sedí **aspoň dvě** klíčová slova.
            // Na jedno se chytne kdejaká věta — „čas" je v hlášce o systémovém
            // čase i ve vadě o expiraci tokenu, a přitom jde o dvě různé věci.
            let mut nejlepsi: Option<(usize, u32)> = None;
            for (i, f) in report.findings.iter().enumerate() {
                if obsazeny[i] || f.file.as_str() != soubor {
                    continue;
                }
                let text = format!("{} {}", f.summary, f.detail).to_lowercase();
                let shod = slova.iter().filter(|s| text.contains(s.as_str())).count();

                let kvalita = match f.line {
                    Some(l) if l.abs_diff(radek) <= 8 => l.abs_diff(radek),
                    // Bez řádku (nebo daleko od něj) rozhoduje jen popis.
                    _ if shod >= 2 => 100,
                    _ => continue,
                };
                if nejlepsi.is_none_or(|(_, k)| kvalita < k) {
                    nejlepsi = Some((i, kvalita));
                }
            }

            match nejlepsi {
                Some((i, _)) => {
                    obsazeny[i] = true;
                    println!("  ✓ {soubor}:{radek} — {nazev}");
                }
                None => {
                    println!("  ✗ {soubor}:{radek} — {nazev}");
                    nenalezene.push(format!("{soubor}:{radek} ({nazev})"));
                }
            }
        }
        println!("  našel {} z {}", vady.len() - nenalezene.len(), vady.len());

        // Nálezy, které nesedí na žádnou zasazenou vadu. Nemusí to být šum —
        // v kódu můžou být i chyby, o kterých nevím — ale je potřeba je vidět,
        // protože samotné „našel 3 ze 6" o přesnosti neříká nic.
        let navic: Vec<String> = report
            .findings
            .iter()
            .zip(&obsazeny)
            .filter(|(_, o)| !**o)
            .map(|(f, _)| format!("{} — {}", f.location(), f.summary))
            .collect();
        if !navic.is_empty() {
            println!("  mimo klíč ({}):", navic.len());
            for n in &navic {
                println!("      {n}");
            }
        }
    }

    // --- kontroly, které jednotkové testy neudělají ------------------------
    let mut problemy: Vec<String> = Vec::new();

    if let Some(klic) = &ocekavane {
        let celkem = klic["vady"].as_array().map(Vec::len).unwrap_or(0);
        let naslo = celkem - nenalezene.len();
        // Půlka je hranice smířlivá: jde o to poznat, jestli je nástroj
        // k něčemu, ne jestli je dokonalý.
        if celkem > 0 && naslo * 2 < celkem {
            problemy.push(format!(
                "Ze {celkem} známých vad model našel {naslo}. Nenašel: {}.",
                nenalezene.join("; ")
            ));
        }
    }

    // „Nic jsem nenašel“ po dvou přečtených souborech ze čtrnácti není
    // výsledek, je to předčasný konec. Sedí to i na běh, který jinak projde
    // všemi ostatními kontrolami — přesně to se stalo napotřetí.
    if report.findings.is_empty() && !vsechny.is_empty() {
        let podil = report.files_read.len() * 100 / vsechny.len();
        if podil < 25 {
            problemy.push(format!(
                "Model prohlásil projekt za čistý, ale otevřel {} z {} souborů ({podil} %). \
                 Na takovém vzorku „nic jsem nenašel“ nic neznamená.",
                report.files_read.len(),
                vsechny.len()
            ));
        }
    }

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

    // Nálezy prochází přes `report_finding`, a tam se dá kontrolovat. Závěrečné
    // shrnutí je ale obyčejný text — a přesně v něm si model dovolil tvrdit
    // věci o souborech, které nikdy neotevřel: „při načítání nastavení by
    // aplikace spadla" o souboru, kde všechny ty `unwrap()` stojí v testech.
    // Do nálezů to nedal, takže kontrola nálezů to propustila. Uživatel přitom
    // čte právě tohle shrnutí.
    //
    // Porovnává se proti skutečným cestám v projektu, ne proti tomu, co v textu
    // vypadá jako cesta — z „např. `src/model.rs`“ se tak nedá vyrobit planý
    // poplach kvůli zkratce nebo interpunkci.
    let zminene_neprectene: Vec<&String> = vsechny
        .iter()
        .filter(|p| outcome.summary.contains(p.as_str()) && !precteno.contains(&p.as_str()))
        .collect();
    if !zminene_neprectene.is_empty() {
        problemy.push(format!(
            "Shrnutí mluví o souborech, které model neotevřel: {}. Z grepu viděl \
             jen jednotlivé řádky bez okolí — tvrzení o tom, co soubor dělá, \
             z nich neplyne.",
            zminene_neprectene
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
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
