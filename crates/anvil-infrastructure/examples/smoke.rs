//! Ověření celého řetězce proti **skutečnému** modelu.
//!
//! Jednotkové testy pokrývají plánovač offloadu, šablony promptů i filtry
//! výstupu, ale všechny běží bez llama.cpp. Chyby, které jsou vidět až
//! na reálném modelu — špatně poskládaný prompt, filtr, který sežere celou
//! odpověď, nebo offload, po kterém je model pomalejší než na CPU — jimi
//! projdou. Na tohle je tenhle příklad.
//!
//! ```text
//! cargo run --release --example smoke --features engine-vulkan -- <model.gguf> [kontext] [tokenů]
//! cargo run --release --example smoke --features engine-metal  -- <model.gguf>
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

    use anvil_domain::{
        conversation::Message,
        model::{ChatTemplateKind, InferenceSettings, ModelId},
        ports::{ChatEngine, CompletionRequest, GenerationProgress},
    };
    use anvil_infrastructure::ai::llama_engine::LlamaChatEngine;
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
         Použití: cargo run --release --example smoke --features engine-vulkan -- <model.gguf> [kontext] [tokenů]",
    )?;
    let kontext: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(8_192);
    let max_tokenu: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(200);

    let cesta = std::path::PathBuf::from(cesta);
    if !cesta.is_file() {
        return Err(format!("{} neexistuje", cesta.display()).into());
    }

    // Šablonu odvodíme z názvu souboru — příklad se spouští i na modelech,
    // které v katalogu nejsou.
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

    let nacitani = std::time::Instant::now();
    let engine = tokio::task::spawn_blocking({
        let cesta = cesta.clone();
        move || {
            LlamaChatEngine::load(
                ModelId::parse("smoke").expect("platné ID"),
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

    // Dotazy jsou česky schválně: ověřuje se i to, že model odpovídá česky
    // a že se diakritika neztratí ve filtru výstupu.
    //
    // Kola jsou **dvě** a obě jsou potřeba:
    //  * první měří i jednorázovou režii (Vulkan si při prvním decode překládá
    //    compute pipeline), takže samo o sobě o rychlosti nic neříká;
    //  * druhé je jediné, které odhalí, že model od druhého tahu vrací nula
    //    tokenů — chyba, která na jiném projektu stála víc než den, protože
    //    jednotkovými testy prošla.
    const DOTAZY: [&str; 2] = [
        "Vysvětli mi krátce, proč je u modelu typu Mixture of Experts rychlost dána \
         počtem aktivních parametrů, a ne celkovým počtem.",
        "A jak se to projeví na tom, kolik paměti model potřebuje?",
    ];

    let mut historie: Vec<Message> = Vec::new();
    let mut vysledky = Vec::new();

    for (i, dotaz) in DOTAZY.into_iter().enumerate() {
        historie.push(Message::user(dotaz));

        let request = CompletionRequest::new(historie.clone())
            .with_system("Jsi zkušený programátor. Odpovídej česky, věcně a stručně.")
            .with_max_tokens(max_tokenu);

        println!("--- {}. kolo ---", i + 1);
        println!("Dotaz:    {dotaz}");
        print!("Odpověď:  ");
        std::io::stdout().flush().ok();

        let nasbirano = Arc::new(Mutex::new(String::new()));
        let sber = nasbirano.clone();
        let outcome = engine
            .complete(
                &request,
                CancellationToken::new(),
                Some(Arc::new(move |p: GenerationProgress| {
                    print!("{}", p.delta);
                    std::io::stdout().flush().ok();
                    *sber.lock().expect("zámek") = p.accumulated;
                })),
            )
            .await?;

        println!();
        println!("  prompt {} tokenů", outcome.prompt_tokens);
        println!("  vygenerováno {} tokenů", outcome.generated_tokens);
        println!(
            "  první token {:.2} s",
            outcome.time_to_first_token_ms as f64 / 1000.0
        );
        println!(
            "  dekódování {:.1} tok/s",
            outcome.decode_tokens_per_second()
        );
        println!("  celkem {:.1} s", outcome.total_ms as f64 / 1000.0);
        println!();

        historie.push(Message::assistant(outcome.text.clone()));
        vysledky.push(outcome);
    }

    // Kontroly, které jednotkové testy z principu neudělají.
    let mut problemy: Vec<String> = Vec::new();
    for (i, o) in vysledky.iter().enumerate() {
        let kolo = i + 1;
        if o.generated_tokens == 0 {
            problemy.push(format!(
                "{kolo}. kolo: model nevygeneroval ani jeden token — špatný prompt, \
                 šablona, nebo neuvolněná KV cache mezi tahy"
            ));
        }
        if o.generated_tokens > 0 && o.text.trim().is_empty() {
            problemy.push(format!(
                "{kolo}. kolo: model generoval, ale text je prázdný — filtr výstupu \
                 zahodil celou odpověď (u Gemmy typicky chybějící kanál `final`)"
            ));
        }
        if o.text.contains("<start_of_turn>") || o.text.contains("<|im_start|>") {
            problemy.push(format!(
                "{kolo}. kolo: do textu prosákly značky šablony — filtr nebo stop \
                 sekvence nefungují"
            ));
        }
    }

    if let [prvni, druhe] = vysledky.as_slice() {
        println!("=== srovnání kol ===");
        println!(
            "První token:  {:.1} s → {:.1} s",
            prvni.time_to_first_token_ms as f64 / 1000.0,
            druhe.time_to_first_token_ms as f64 / 1000.0
        );
        println!(
            "Dekódování:   {:.1} tok/s → {:.1} tok/s",
            prvni.decode_tokens_per_second(),
            druhe.decode_tokens_per_second()
        );
        if druhe.time_to_first_token_ms * 3 < prvni.time_to_first_token_ms {
            println!(
                "\nPozn.: první kolo platí jednorázovou režii (Vulkan si překládá compute\n\
                 pipeline). Reálné číslo pro uživatele je to z druhého kola."
            );
        }
    }

    if problemy.is_empty() {
        println!("\nOK — model odpověděl v obou kolech a text prošel filtrem čistý.");
        Ok(())
    } else {
        println!();
        for p in &problemy {
            eprintln!("CHYBA: {p}");
        }
        Err(format!("{} kontrol selhalo", problemy.len()).into())
    }
}
