//! `ChatEngine` nad llama.cpp.
//!
//! **Proč vlastní vlákno a ne `spawn_blocking`.** `LlamaContext` drží KV cache
//! a není `Send` — přes hranici tokio úlohy ho poslat nejde. Vytvářet ho pro
//! každý dotaz znovu by šlo, ale alokace KV cache při 32K kontextu jsou
//! stovky megabajtů a platily by se při každé zprávě. Kontext proto žije na
//! jednom dedikovaném vlákně, kterému se posílají příkazy.
//!
//! **Prompt se skládá celý znovu, ale nepočítá se celý znovu.** Aplikační
//! vrstva předá celou viditelnou konverzaci; engine ji tokenizuje a porovná
//! s tím, co už v KV cache leží. Shodný začátek se zachová a dopočítá se jen
//! zbytek — viz [`super::kv_reuse`].
//!
//! Bez toho by to bylo nepoužitelné. Na hybridním MoE běhu (experti v RAM)
//! jede zpracování promptu ~26 tokenů za sekundu, takže konverzace s obsahem
//! jednoho zdrojáku by znamenala minuty čekání před **každou** odpovědí.

use std::{
    num::NonZeroU32,
    sync::{mpsc, Arc, OnceLock},
    time::Instant,
};

use anvil_domain::{
    error::{DomainError, DomainResult},
    model::{ChatTemplateKind, InferenceSettings, ModelId, Sampling},
    ports::{
        ChatEngine, CompletionOutcome, CompletionRequest, GenerationProgress, ProgressCallback,
    },
};
use async_trait::async_trait;
use llama_cpp_2::{
    context::params::{KvCacheType, LlamaContextParams},
    llama_backend::LlamaBackend,
    llama_batch::LlamaBatch,
    model::{params::LlamaModelParams, AddBos, LlamaModel},
    sampling::LlamaSampler,
};
use tokio_util::sync::CancellationToken;

use super::{
    chat_template::{self, OutputFilter},
    device_catalog, gguf_meta, kv_reuse,
    offload_plan::{self, OffloadDecision},
};

/// Kolik tokenů se dekóduje v jedné dávce. llama.cpp assertne, když se do
/// batche vloží víc tokenů, než na kolik byl vyrobený — prompt se proto
/// zpracovává po kusech téhle velikosti.
const N_BATCH: usize = 512;

/// `LlamaBackend::init()` smí proběhnout jen jednou za život procesu.
static BACKEND: OnceLock<Result<LlamaBackend, String>> = OnceLock::new();

fn backend() -> DomainResult<&'static LlamaBackend> {
    BACKEND
        .get_or_init(|| LlamaBackend::init().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| DomainError::model(format!("llama.cpp se nepodařilo inicializovat: {e}")))
}

// --- Komunikace s pracovním vláknem ---------------------------------------

enum Command {
    Complete {
        prompt: String,
        max_tokens: u32,
        sampling: Sampling,
        stop: &'static [&'static str],
        template: ChatTemplateKind,
        cancel: CancellationToken,
        events: mpsc::Sender<Event>,
    },
}

enum Event {
    Token(String),
    Done(Box<CompletionOutcome>),
    Failed(String),
}

/// Načtený model připravený odpovídat.
pub struct LlamaChatEngine {
    model_id: ModelId,
    template: ChatTemplateKind,
    context_tokens: u32,
    /// Model se drží i tady, aby šlo tokenizovat bez čekání na volné vlákno.
    /// `LlamaModel` je `Send + Sync`, na rozdíl od kontextu.
    model: Arc<LlamaModel>,
    commands: mpsc::Sender<Command>,
    /// Popis zvoleného plánu offloadu — do logu a do UI.
    plan_description: String,
    _worker: WorkerHandle,
}

/// Ukončí pracovní vlákno, až engine zanikne. Bez toho by vlákno drželo
/// model (a s ním VRAM) i po přepnutí na jiný model.
struct WorkerHandle(Option<std::thread::JoinHandle<()>>);

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            // Odesílací konec už je zahozený, takže `recv` na vlákně skončí
            // chybou a smyčka se ukončí sama.
            let _ = handle.join();
        }
    }
}

impl LlamaChatEngine {
    /// Načte model ze souboru a spustí pracovní vlákno.
    ///
    /// Rozvržení mezi GPU a RAM se spočítá z GGUF hlavičky a z toho, co je
    /// na stroji k dispozici — viz [`offload_plan`].
    pub fn load(
        model_id: ModelId,
        path: &std::path::Path,
        template: ChatTemplateKind,
        settings: InferenceSettings,
    ) -> DomainResult<Self> {
        let backend = backend()?;

        let info = gguf_meta::read_gguf_info(path)
            .map_err(|e| DomainError::model(format!("GGUF hlavičku nejde přečíst: {e}")))?;
        let model_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

        let machine = device_catalog::machine_profile();
        let mut decision = offload_plan::plan_offload(
            model_bytes,
            &info,
            &machine,
            settings.context_tokens,
            settings.use_gpu,
        );
        apply_tuning_overrides(&mut decision);
        tracing::info!(
            model = %model_id,
            plan = decision.plan.label(),
            gpu_layers = decision.gpu_layers,
            cpu_moe = decision.cpu_moe,
            threads = decision.threads,
            "{}",
            decision.reason
        );

        let model = Arc::new(load_model(backend, path, &decision)?);
        let plan_description = decision.reason.clone();

        let (tx, rx) = mpsc::channel::<Command>();
        let worker_model = model.clone();
        let context_tokens = settings.context_tokens;
        let worker_decision = decision.clone();

        let handle = std::thread::Builder::new()
            .name(format!("anvil-llama-{model_id}"))
            .spawn(move || worker_loop(backend, worker_model, worker_decision, context_tokens, rx))
            .map_err(|e| DomainError::model(format!("pracovní vlákno nelze spustit: {e}")))?;

        Ok(Self {
            model_id,
            template,
            context_tokens,
            model,
            commands: tx,
            plan_description,
            _worker: WorkerHandle(Some(handle)),
        })
    }

    /// Jak je model rozložený mezi GPU a RAM — text pro UI.
    pub fn plan_description(&self) -> &str {
        &self.plan_description
    }
}

/// Ladicí přepínače z prostředí.
///
/// Existují kvůli měření: plán offloadu je odvozený z čísel naměřených na
/// jedné konfiguraci a při jejich ověřování na jiném stroji je potřeba umět
/// jednotlivé volby přebít, aniž by se překládal jiný build. Za běhu aplikace
/// se nenastavují — bez proměnných platí, co spočítal `offload_plan`.
///
/// * `ANVIL_OP_OFFLOAD=0|1` — přebije `op_offload`
/// * `ANVIL_CPU_MOE=0|1` — přebije přesun expertů do RAM
fn apply_tuning_overrides(decision: &mut OffloadDecision) {
    fn bool_env(key: &str) -> Option<bool> {
        match std::env::var(key).ok()?.trim() {
            "1" | "true" | "on" => Some(true),
            "0" | "false" | "off" => Some(false),
            other => {
                tracing::warn!("{key}={other} není 0/1 — ignoruji");
                None
            }
        }
    }

    if let Some(v) = bool_env("ANVIL_OP_OFFLOAD") {
        tracing::warn!("ANVIL_OP_OFFLOAD={v} přebíjí plán offloadu");
        decision.op_offload = v;
    }
    if let Some(v) = bool_env("ANVIL_CPU_MOE") {
        tracing::warn!("ANVIL_CPU_MOE={v} přebíjí plán offloadu");
        decision.cpu_moe = v;
    }
}

fn load_model(
    backend: &LlamaBackend,
    path: &std::path::Path,
    decision: &OffloadDecision,
) -> DomainResult<LlamaModel> {
    let mut params = LlamaModelParams::default().with_n_gpu_layers(decision.gpu_layers);
    if let Some(index) = decision.device_index {
        match params.with_devices(&[index]) {
            Ok(p) => params = p,
            Err(e) => {
                tracing::warn!(error = %e, "Výběr zařízení selhal — nechávám volbu na llama.cpp");
                params = LlamaModelParams::default().with_n_gpu_layers(decision.gpu_layers);
            }
        }
    }

    let mut params = Box::pin(params);
    if decision.cpu_moe {
        // Záměrně **ne** `add_cpu_moe_override()` z knihovny: její vzor je
        // `\.ffn_(up|down|gate)_(ch|)exps`, jenže Gemma 4 má gate a up slité
        // do `ffn_gate_up_exps` — ten největší tenzor by zůstal ve VRAM
        // a načtení skončilo `ErrorOutOfDeviceMemory`. Vzor `exps` chytne
        // obojí a router `ffn_gate_inp` nechá na GPU, kam patří.
        params.as_mut().add_cpu_buft_override(c"exps");
    }

    LlamaModel::load_from_file(backend, path, &params)
        .map_err(|e| DomainError::model(format!("model se nepodařilo načíst: {e}")))
}

/// Smyčka pracovního vlákna. Kontext se vytvoří jednou a žije, dokud engine
/// existuje.
fn worker_loop(
    backend: &'static LlamaBackend,
    model: Arc<LlamaModel>,
    decision: OffloadDecision,
    context_tokens: u32,
    commands: mpsc::Receiver<Command>,
) {
    let n_ctx = NonZeroU32::new(context_tokens.max(512));
    let mut ctx_params = LlamaContextParams::default()
        .with_n_ctx(n_ctx)
        .with_n_batch(N_BATCH as u32)
        .with_n_threads(decision.threads)
        .with_n_threads_batch(decision.threads)
        // Bez tohohle jde první token zhruba dvakrát pomaleji: llama.cpp
        // tahá operace nad CPU tenzory na GPU a zpátky. Decode to nezhorší.
        .with_op_offload(decision.op_offload);
    if decision.quantized_kv {
        ctx_params = ctx_params
            .with_type_k(KvCacheType::Q8_0)
            .with_type_v(KvCacheType::Q8_0);
    }

    let mut ctx = match model.new_context(backend, ctx_params) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "Kontext llama.cpp se nepodařilo vytvořit");
            // Odpovědět na všechny čekající příkazy, ať volající nezůstane viset.
            while let Ok(Command::Complete { events, .. }) = commands.recv() {
                let _ = events.send(Event::Failed(format!("kontext nelze vytvořit: {e}")));
            }
            return;
        }
    };

    // Co právě leží v KV cache. Prázdné = cache je čistá.
    let mut cached: Vec<i32> = Vec::new();

    while let Ok(command) = commands.recv() {
        let Command::Complete {
            prompt,
            max_tokens,
            sampling,
            stop,
            template,
            cancel,
            events,
        } = command;

        let result = generate(
            &model,
            &mut ctx,
            &mut cached,
            context_tokens,
            &prompt,
            max_tokens,
            sampling,
            stop,
            template,
            &cancel,
            &events,
        );

        match result {
            Ok(outcome) => {
                let _ = events.send(Event::Done(Box::new(outcome)));
            }
            Err(e) => {
                let _ = events.send(Event::Failed(e));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn generate(
    model: &LlamaModel,
    ctx: &mut llama_cpp_2::context::LlamaContext<'_>,
    // `cached` popisuje, co právě leží v KV cache; funkce ho udržuje aktuální.
    cached: &mut Vec<i32>,
    context_tokens: u32,
    prompt: &str,
    max_tokens: u32,
    sampling: Sampling,
    stop: &[&str],
    template: ChatTemplateKind,
    cancel: &CancellationToken,
    events: &mpsc::Sender<Event>,
) -> Result<CompletionOutcome, String> {
    let zacatek = Instant::now();

    // Šablony obsahují BOS samy — `AddBos::Never` brání zdvojení, které
    // u Gemmy znatelně zhorší výstup.
    let tokens = model
        .str_to_token(prompt, AddBos::Never)
        .map_err(|e| format!("tokenizace selhala: {e}"))?;

    let rezerva = max_tokens.max(256);
    if tokens.len() as u32 + rezerva > context_tokens {
        return Err(format!(
            "Prompt má {} tokenů a s rezervou na odpověď se nevejde do okna {context_tokens}. \
             Zvětši kontext v nastavení, nebo zkrať dotaz.",
            tokens.len()
        ));
    }

    // Kolik ze začátku promptu už v cache je. Zbytek se dopočítá.
    let hodnoty: Vec<i32> = tokens.iter().map(|t| t.0).collect();
    let plan = kv_reuse::plan_cache(cached, &hodnoty);
    let start = plan.start_position();

    // Od téhle chvíle nesmí `cached` popisovat nic, co v cache není. Kdyby
    // dekódování v půlce selhalo, zůstal by tam neúplný stav a příští tah by
    // podle něj přeskočil tokeny, které se nikdy nespočítaly — model by
    // odpovídal na prompt, který nikdy neviděl. Proto se vyprázdní hned
    // a naplní až po úspěchu.
    cached.clear();

    match plan {
        kv_reuse::CachePlan::Rebuild => ctx.clear_kv_cache(),
        kv_reuse::CachePlan::Reuse { keep } => {
            ctx.clear_kv_cache_seq(Some(0), Some(keep as u32), None)
                .map_err(|e| format!("nelze zahodit konec KV cache: {e}"))?;
            tracing::debug!(
                znovu = keep,
                dopocitat = tokens.len() - keep,
                "Znovupoužívám prefix KV cache"
            );
        }
    }

    let mut batch = LlamaBatch::new(N_BATCH, 1);
    let posledni = tokens.len() - 1;
    for zacatek_kusu in (start..tokens.len()).step_by(N_BATCH) {
        let konec = (zacatek_kusu + N_BATCH).min(tokens.len());
        batch.clear();
        // Pozice musí být absolutní vůči celému promptu, ne vůči dávce —
        // jinak by se zachovaný prefix a dopočítaný zbytek překryly.
        for (i, token) in tokens.iter().enumerate().take(konec).skip(zacatek_kusu) {
            batch
                .add(*token, i as i32, &[0], i == posledni)
                .map_err(|e| format!("batch: {e}"))?;
        }
        ctx.decode(&mut batch)
            .map_err(|e| format!("zpracování promptu selhalo: {e}"))?;
    }

    // Prompt je v cache celý.
    cached.extend_from_slice(&hodnoty);

    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(1234);
    let mut sampler = LlamaSampler::chain_simple([
        // Postih za opakování drží menší modely mimo degenerativní smyčky.
        LlamaSampler::penalties(64, sampling.repeat_penalty, 0.0, 0.0),
        LlamaSampler::top_k(sampling.top_k as i32),
        LlamaSampler::top_p(sampling.top_p, 1),
        LlamaSampler::temp(sampling.temperature.max(0.05)),
        LlamaSampler::dist(seed),
    ]);

    let mut utf8 = encoding_rs::UTF_8.new_decoder();
    let mut filter = OutputFilter::for_template(template);
    let mut n_cur = tokens.len() as i32;
    let mut vygenerovano: u32 = 0;
    let mut ttft_ms: Option<u64> = None;
    let mut cely_text = String::new();
    let mut zruseno = false;

    loop {
        if cancel.is_cancelled() {
            zruseno = true;
            break;
        }
        if vygenerovano >= max_tokens || n_cur >= context_tokens as i32 {
            break;
        }

        let token = sampler.sample(ctx, batch.n_tokens() - 1);
        sampler.accept(token);
        if model.is_eog_token(token) {
            break;
        }

        let kus = model
            .token_to_piece(token, &mut utf8, true, None)
            .unwrap_or_default();
        vygenerovano += 1;
        if ttft_ms.is_none() {
            ttft_ms = Some(zacatek.elapsed().as_millis() as u64);
        }

        let viditelne = filter.push(&kus);
        if !viditelne.is_empty() {
            cely_text.push_str(&viditelne);
            // Zmizelý příjemce znamená, že volající odešel — nemá smysl
            // pokračovat v počítání.
            if events.send(Event::Token(viditelne)).is_err() {
                zruseno = true;
                break;
            }
        }

        // Zastavovací sekvence — model některé emituje jako běžný text.
        if stop.iter().any(|s| cely_text.ends_with(s)) {
            for s in stop {
                if let Some(bez) = cely_text.strip_suffix(s) {
                    cely_text = bez.to_string();
                    break;
                }
            }
            break;
        }

        batch.clear();
        batch
            .add(token, n_cur, &[0], true)
            .map_err(|e| format!("batch: {e}"))?;
        n_cur += 1;
        ctx.decode(&mut batch)
            .map_err(|e| format!("generování selhalo: {e}"))?;
        // Vygenerovaný token je teď taky v cache — příští tah ho v promptu
        // najde jako součást odpovědi a nebude ho počítat znovu.
        cached.push(token.0);
    }

    let zbytek = filter.finish();
    if !zbytek.is_empty() {
        cely_text.push_str(&zbytek);
        let _ = events.send(Event::Token(zbytek));
    }

    let celkem_ms = zacatek.elapsed().as_millis() as u64;
    Ok(CompletionOutcome {
        text: cely_text,
        prompt_tokens: tokens.len() as u32,
        generated_tokens: vygenerovano,
        time_to_first_token_ms: ttft_ms.unwrap_or(celkem_ms),
        total_ms: celkem_ms,
        cancelled: zruseno,
    })
}

#[async_trait]
impl ChatEngine for LlamaChatEngine {
    async fn complete(
        &self,
        request: &CompletionRequest,
        cancel: CancellationToken,
        on_progress: Option<ProgressCallback>,
    ) -> DomainResult<CompletionOutcome> {
        if cancel.is_cancelled() {
            return Err(DomainError::Cancelled);
        }

        let prompt = chat_template::build_prompt(
            self.template,
            request.system.as_deref(),
            request.summary.as_deref(),
            &request.messages,
        );

        let (tx, rx) = mpsc::channel::<Event>();
        self.commands
            .send(Command::Complete {
                prompt,
                max_tokens: request.max_tokens,
                sampling: request.sampling,
                stop: chat_template::stop_sequences(self.template),
                template: self.template,
                cancel,
                events: tx,
            })
            .map_err(|_| DomainError::model("pracovní vlákno modelu neběží"))?;

        // Příjem běží na blokujícím vlákně, ať nezablokuje tokio runtime.
        tokio::task::spawn_blocking(move || {
            let mut nasbirano = String::new();
            let mut pocet: u32 = 0;
            loop {
                match rx.recv() {
                    Ok(Event::Token(kus)) => {
                        pocet += 1;
                        nasbirano.push_str(&kus);
                        if let Some(cb) = &on_progress {
                            cb(GenerationProgress {
                                delta: kus,
                                accumulated: nasbirano.clone(),
                                token_count: pocet,
                            });
                        }
                    }
                    Ok(Event::Done(outcome)) => return Ok(*outcome),
                    Ok(Event::Failed(msg)) => return Err(DomainError::model(msg)),
                    Err(_) => {
                        return Err(DomainError::model(
                            "pracovní vlákno modelu skončilo bez odpovědi",
                        ))
                    }
                }
            }
        })
        .await
        .map_err(|e| DomainError::model(format!("úloha modelu selhala: {e}")))?
    }

    fn count_tokens(&self, text: &str) -> DomainResult<u32> {
        if text.is_empty() {
            return Ok(0);
        }
        self.model
            .str_to_token(text, AddBos::Never)
            .map(|t| t.len() as u32)
            .map_err(|e| DomainError::model(format!("tokenizace selhala: {e}")))
    }

    fn context_tokens(&self) -> u32 {
        self.context_tokens
    }

    fn model_id(&self) -> &ModelId {
        &self.model_id
    }
}
