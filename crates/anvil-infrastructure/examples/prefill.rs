//! Změří **zpracování promptu** (prefill) podle jeho délky.
//!
//! Proč zvlášť od `smoke`: u hybridního MoE běhu jsou to dvě úplně různé
//! úlohy. Dekódování aktivuje pro každý token jen pár expertů, takže je
//! omezené propustností paměti a jede kolem 20 tok/s. Prefill zpracovává
//! celou dávku najednou — tam se aktivuje podstatně víc expertů a je to
//! úloha výpočetní.
//!
//! Rozhoduje to o použitelnosti celé aplikace. Díky znovupoužití prefixu
//! KV cache (`ai::kv_reuse`) se u dalšího tahu dopočítá jen nová zpráva, ale
//! **první** tah nad otevřeným souborem tuhle cenu zaplatí celou — a to je
//! přesně scénář code review.
//!
//! Měří **studený** prefill: každý prompt začíná jinak, aby si nezachoval
//! nic z KV cache po předchozím měření. Reálný přínos znovupoužití prefixu
//! ukazuje `smoke`, kde druhé kolo navazuje na první.
//!
//! ```text
//! cargo run --release --example prefill --features engine-vulkan -- <model.gguf>
//! ANVIL_OP_OFFLOAD=1 cargo run --release --example prefill --features engine-vulkan -- <model.gguf>
//! ```

#[cfg(not(feature = "engine"))]
fn main() {
    eprintln!("Potřebuje --features engine-vulkan nebo engine-metal.");
    std::process::exit(1);
}

#[cfg(feature = "engine")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use anvil_domain::{
        conversation::Message,
        model::{ChatTemplateKind, InferenceSettings, ModelId},
        ports::{ChatEngine, CompletionRequest},
    };
    use anvil_infrastructure::ai::llama_engine::LlamaChatEngine;
    use tokio_util::sync::CancellationToken;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ANVIL_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cesta = std::env::args()
        .nth(1)
        .ok_or("Použití: prefill <model.gguf>")?;
    let cesta = std::path::PathBuf::from(cesta);

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

    let engine = tokio::task::spawn_blocking({
        let cesta = cesta.clone();
        move || {
            LlamaChatEngine::load(
                ModelId::parse("prefill").expect("platné ID"),
                &cesta,
                sablona,
                InferenceSettings::default().with_context(16_384),
            )
        }
    })
    .await??;

    println!("Model: {}", cesta.display());
    println!("Plán:  {}", engine.plan_description());
    println!(
        "op_offload override: {}",
        std::env::var("ANVIL_OP_OFFLOAD").unwrap_or_else(|_| "(bez override)".into())
    );
    println!();
    println!("{:>8}  {:>10}  {:>12}", "tokenů", "prefill s", "tok/s");
    println!("{:->8}  {:->10}  {:->12}", "", "", "");

    // Výplň, kterou se prompt natahuje na požadovanou délku. Je to smysluplný
    // text, ne opakované slovo — u opakování by router posílal tokeny pořád
    // stejným expertům a měření by vyšlo optimističtěji, než jak se model
    // chová na skutečném kódu.
    const VYPLN: &str = "Funkce načte konfiguraci ze souboru, ověří povinná pole a vrátí \
         chybu, když některé chybí. Volající ji používá při startu aplikace. \
         Testy pokrývají prázdný soubor, poškozený JSON i chybějící složku. ";

    // Zahřívací kolo: první decode v procesu platí jednorázový překlad
    // Vulkan compute pipeline a zkreslilo by první měřený bod.
    let _ = engine
        .complete(
            &CompletionRequest::new(vec![Message::user("ahoj")]).with_max_tokens(1),
            CancellationToken::new(),
            None,
        )
        .await?;

    for (poradi, cil) in [128u32, 512, 1024, 2048, 4096].into_iter().enumerate() {
        // Každé měření začíná jiným textem. Bez toho by si delší prompt
        // znovupoužil KV cache od kratšího (viz `ai::kv_reuse`) a měřilo by se
        // něco jiného, než co je tu záměrem — totiž zpracování od nuly.
        let mut text = format!("Měření číslo {poradi}. ");
        while engine.count_tokens(&text)? < cil {
            text.push_str(VYPLN);
        }

        let request = CompletionRequest::new(vec![Message::user(&text)])
            // Zajímá nás jen čas do prvního tokenu, ne dekódování.
            .with_max_tokens(4);

        let outcome = engine
            .complete(&request, CancellationToken::new(), None)
            .await?;

        let s = outcome.time_to_first_token_ms as f64 / 1000.0;
        println!(
            "{:>8}  {:>10.2}  {:>12.1}",
            outcome.prompt_tokens,
            s,
            if s > 0.0 {
                outcome.prompt_tokens as f64 / s
            } else {
                0.0
            }
        );
    }

    println!();
    println!(
        "Pozn.: čas do prvního tokenu je prakticky celý prefill — u každého tahu\n\
         se prompt skládá znovu, takže tohle číslo je to, na co uživatel čeká."
    );

    Ok(())
}
