//! Ověření **úprav souborů** proti skutečnému modelu.
//!
//! Fáze 4 stojí na jednom předpokladu, který jednotkové testy z principu
//! neprověří: že model dokáže zopakovat `old_text` **přesně tak, jak stojí
//! v souboru**, včetně odsazení. Skriptovaný dvojník to umí vždycky, protože
//! mu ten text napíšu já.
//!
//! Když se netrefí, úprava se odmítne — což je bezpečné, ale k ničemu.
//! Tenhle příklad měří, jak často se to stane.
//!
//! Nesahá na disk. Model dostane obsah souboru a popis chyby, navrhne opravu
//! a ta se jen spočítá; `EditPlan::apply` se nevolá, takže projekt zůstane
//! rozbitý pro další běh.
//!
//! ```text
//! cargo run --release --example oprava --features engine-vulkan -- <model.gguf> <složka>
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
        io::Write,
        sync::{Arc, Mutex},
    };

    use anvil_application::{
        agent::{
            runner::{AgentEvent, AgentHooks, AgentLoop},
            tools::Toolbox,
        },
        EditPlan,
    };
    use anvil_domain::{
        conversation::{Conversation, Message},
        model::{ChatTemplateKind, InferenceSettings, ModelId},
        ports::WorkspaceFs,
        workspace::{RelativePath, Workspace},
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
        "Použití: cargo run --release --example oprava --features engine-vulkan -- \
         <model.gguf> <složka>",
    )?;
    let slozka = args.next().ok_or("Chybí složka s vadným projektem.")?;
    let kontext: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(16_384);

    let cesta = std::path::PathBuf::from(cesta);
    let koren = std::fs::canonicalize(&slozka)
        .map_err(|e| format!("složku {slozka} nejde otevřít: {e}"))?;

    // Klíč leží vedle složky, ne v ní — stejně jako u `review`.
    let klic_cesta = koren.parent().map(|p| {
        p.join(format!(
            "{}-nalezy.json",
            koren.file_name().unwrap_or_default().to_string_lossy()
        ))
    });
    let klic: serde_json::Value = klic_cesta
        .filter(|p| p.is_file())
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .ok_or("Vedle složky chybí soubor <složka>-nalezy.json se seznamem vad.")?;
    let vady = klic["vady"].as_array().cloned().unwrap_or_default();

    let jmeno = cesta.file_name().unwrap_or_default().to_string_lossy();
    let sablona = if jmeno.to_lowercase().contains("gemma") {
        ChatTemplateKind::Gemma4
    } else {
        ChatTemplateKind::Qwen3
    };

    println!("Model:  {}", cesta.display());
    println!("Složka: {}", koren.display());
    println!("Vad k opravě: {}\n", vady.len());

    let engine = tokio::task::spawn_blocking({
        let cesta = cesta.clone();
        move || {
            LlamaChatEngine::load(
                ModelId::parse("oprava").expect("platné ID"),
                &cesta,
                sablona,
                InferenceSettings::default().with_context(kontext),
            )
        }
    })
    .await??;
    println!("Plán:   {}\n", engine.plan_description());

    let fs: Arc<dyn WorkspaceFs> = Arc::new(LocalWorkspaceFs::new(Workspace::new(koren)?)?);
    let engine: Arc<dyn anvil_domain::ports::ChatEngine> = Arc::new(engine);

    let mut prijato = 0usize;
    let mut odmitnuto = 0usize;
    let mut duvody: Vec<String> = Vec::new();
    let mut bez_navrhu: Vec<String> = Vec::new();

    for vada in &vady {
        let soubor = vada["soubor"].as_str().unwrap_or_default();
        let radek = vada["radek"].as_u64().unwrap_or(0);
        let nazev = vada["nazev"].as_str().unwrap_or_default();
        let proc = vada["proc"].as_str().unwrap_or_default();

        println!("--- {soubor}:{radek} — {nazev} ---");

        let path = RelativePath::parse(soubor)?;
        let Some(obsah) = fs.read_whole(&path).await? else {
            println!("  soubor neexistuje, přeskakuji\n");
            continue;
        };
        let cislovane: String = obsah
            .lines()
            .enumerate()
            .map(|(i, r)| format!("{:>5} | {r}\n", i + 1))
            .collect();

        // Plán je na každou vadu čerstvý, aby se opravy nemíchaly a šlo
        // spolehlivě spočítat, kolik jich prošlo.
        let plan = Arc::new(tokio::sync::Mutex::new(EditPlan::new()));
        let toolbox = Toolbox::for_editing(fs.clone(), plan.clone());

        let chyby = Arc::new(Mutex::new(Vec::<String>::new()));
        let sber = chyby.clone();
        let hooks = AgentHooks::events(Arc::new(move |e: AgentEvent| {
            match e {
                AgentEvent::ToolCalled { name, summary } => println!("  → {name} {summary}"),
                AgentEvent::ToolFinished { name, ok } if !ok => {
                    sber.lock().expect("zámek").push(name);
                }
                _ => {}
            }
            std::io::stdout().flush().ok();
        }));

        let mut vlakno = Conversation::new("");
        vlakno.push(Message::user(format!(
            "Soubor `{soubor}`:\n\n```\n{cislovane}```\n\n\
             Na řádku {radek} je chyba: {nazev}\n{proc}\n\n\
             Oprav ji přes `edit_file`. `old_text` musí být přesné znění z výpisu \
             výše — bez čísel řádků a svislítek, jen samotný kód včetně odsazení."
        )));

        let outcome = AgentLoop::new()
            .with_max_rounds(4)
            .run(
                &mut vlakno,
                &engine,
                &toolbox,
                "Jsi zkušený programátor. Opravíš zadanou chybu jedinou úpravou \
                 a nic dalšího neměníš.",
                CancellationToken::new(),
                hooks,
            )
            .await?;

        let plan = plan.lock().await;
        match plan.changes().first() {
            Some(zmena) => {
                let n = zmena.preview();
                prijato += 1;
                println!("  ✓ úprava prošla: +{}, −{}", n.added, n.removed);
                for l in n.lines.iter().take(12) {
                    match l {
                        anvil_domain::edit::DiffLine::Removed { line, text } => {
                            println!("      −{line:>4} | {text}")
                        }
                        anvil_domain::edit::DiffLine::Added { text } => {
                            println!("      +     | {text}")
                        }
                        anvil_domain::edit::DiffLine::Context { line, text } => {
                            println!("       {line:>4} | {text}")
                        }
                    }
                }
            }
            None => {
                let nezdary = chyby.lock().expect("zámek").len();
                if nezdary > 0 {
                    odmitnuto += 1;
                    duvody.push(format!("{soubor}: {nezdary}× odmítnutá úprava"));
                    println!("  ✗ žádná úprava neprošla ({nezdary} pokusů odmítnuto)");
                } else {
                    bez_navrhu.push(soubor.to_string());
                    println!("  ✗ model úpravu vůbec nenavrhl");
                    let t = outcome.text.trim();
                    if !t.is_empty() {
                        println!("      „{}“", t.chars().take(160).collect::<String>());
                    }
                }
            }
        }
        println!();
    }

    // --- souhrn -----------------------------------------------------------
    println!("=== Výsledek ===");
    println!("  úprava prošla:      {prijato} z {}", vady.len());
    println!("  úprava odmítnuta:   {odmitnuto}");
    println!("  bez návrhu:         {}", bez_navrhu.len());
    for d in &duvody {
        println!("      {d}");
    }
    for s in &bez_navrhu {
        println!("      {s}: model nic nenavrhl");
    }

    // Kontrola, kterou jednotkový test neudělá: projekt musí zůstat nedotčený.
    // Kdyby `edit_file` někudy sáhl na disk, poznalo by se to právě tady.
    let stale_vadny = fs
        .read_whole(&RelativePath::parse("src/fronta.rs")?)
        .await?
        .is_some_and(|o| o.contains("len() - 1"));

    println!("\n=== Kontroly ===");
    if stale_vadny {
        println!("  ✓ projekt zůstal nedotčený — návrh na disk nesáhl");
    } else {
        eprintln!(
            "[PROBLÉM] Soubor na disku se změnil, přestože se nic neschvalovalo. \
             Návrh sahá na disk — to je přesně to, čemu měla fáze 4 zabránit."
        );
        return Err("návrh zapsal na disk".into());
    }

    if prijato * 2 < vady.len() {
        eprintln!(
            "[PROBLÉM] Prošla míň než polovina úprav ({prijato} z {}). Model se \
             netrefuje do přesného znění `old_text` a úpravy jsou tím k ničemu.",
            vady.len()
        );
        return Err("příliš málo použitelných úprav".into());
    }

    println!("  ✓ model se do přesného znění trefuje");
    Ok(())
}
