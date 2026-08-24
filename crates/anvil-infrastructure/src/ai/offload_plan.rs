//! Rozhodnutí, **co poslat na GPU a co nechat v RAM**, aby šel použít
//! model větší než VRAM a přitom se negeneroval rychlostí CPU.
//!
//! Naivní `-ngl 99` u modelu, který se do VRAM nevejde, končí buď OOM,
//! nebo (na Windows/WDDM) přetečením do RAM přes PCIe — a to je pomalejší
//! než čistý CPU. Naivní „offloadni N vrstev" je u MoE modelů taky špatně:
//! do VRAM se dostanou i experti, kteří se pro každý token stejně mění,
//! takže se jen jezdí po PCIe.
//!
//! Správné řešení pro MoE (ekvivalent `--cpu-moe` v llama.cpp):
//! **všechny vrstvy na GPU, ale tenzory expertů natvrdo do RAM.** Na GPU
//! zůstane attention + KV cache (malé, počítá se pro každý token), experti
//! se počítají na CPU, kde jsou na ně rychlé AVX kernely a plná propustnost
//! paměti. Aktivní je vždy jen zlomek expertů, takže CPU práce je malá.
//!
//! Naměřeno (Gemma 4 26B A4B, Q4_K, 16 GB soubor / 8 GB VRAM):
//!
//! | konfigurace                          | tok/s |
//! |--------------------------------------|-------|
//! | všechno na CPU, výchozí vlákna       |  9,7  |
//! | všechno na CPU, laděná vlákna        | 11,2  |
//! | naivní offload 12 vrstev             |  7,1  |
//! | hybrid (experti v RAM) + op_offload  | 17,8  |
//!
//! Dvě protiintuitivní věci, které z měření plynou a jsou tu zadrátované:
//! (1) víc vláken škodí — E-jádra drží bariéru zpátky, optimum jsou zhruba
//! dvě třetiny logických jader; (2) `op_offload = false` (nedávat jednotlivé
//! operace na GPU, když tam nejsou váhy) srazí čas prvního tokenu na
//! polovinu a decode nezhorší.

use super::gguf_meta::GgufInfo;

/// „Všechny vrstvy na GPU" — llama.cpp bere velké číslo jako všechno.
pub const ALL_GPU_LAYERS: u32 = 1_000_000;

/// Bezpečnostní odstup od dostupné VRAM.
///
/// Pozor na dvojí započítání: plánovač dostává **volnou** paměť karty, ne
/// celkovou — plocha, prohlížeč a ostatní procesy už jsou z ní odečtené.
/// Tahle rezerva je jen odstup pro to, co si během běhu přiberou navíc.
/// Původních 768 MB se odečítalo, jako by šlo o celkovou paměť, a stálo to
/// jeden konkrétní případ: 24B model (13,7 GB) na kartě s 15,2 GB volnými
/// spadl do dělení vrstev, přestože se celý vejde.
const VRAM_SAFETY_MARGIN_BYTES: u64 = 512 * 1024 * 1024;

/// Rezerva na compute buffery llama.cpp (mezivýsledky, logits).
const COMPUTE_BUFFER_BYTES: u64 = 512 * 1024 * 1024;

/// Odhad podílu vah, které u MoE modelu tvoří experti. Pro 128 expertů
/// s 8 aktivními je to přes 90 %; 85 % je konzervativní střed, aby plán
/// nepodstřelil VRAM u modelů s menším počtem expertů.
const MOE_EXPERT_WEIGHT_SHARE: f64 = 0.85;

/// Když model neuvádí rozměry attention, odhadneme KV cache paušálem
/// na 1024 tokenů (odpovídá ~48 vrstvám s 4 KV hlavami v F16).
const KV_FALLBACK_BYTES_PER_1K: u64 = 200 * 1024 * 1024;

/// Co je na stroji k dispozici. Odděleno od detekce, aby šla logika
/// testovat bez GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineProfile {
    /// Volná paměť zvolené karty. `None` = žádná použitelná GPU.
    ///
    /// Záměrně **volná**, ne celková: na kartě, která zároveň kreslí plochu,
    /// je rozdíl klidně přes gigabajt a plánovat podle celkové by znamenalo
    /// slíbit VRAM, kterou nikdy nedostaneme.
    pub vram_bytes: Option<u64>,
    /// Index zvoleného zařízení pro `with_devices`. `None` = nechat volbu
    /// na llama.cpp (typicky když je zařízení jediné).
    pub device_index: Option<usize>,
    /// Počet fyzických jader CPU.
    pub cpu_cores: usize,
    /// Sdílí CPU a GPU jednu fyzickou paměť? (Apple Silicon)
    ///
    /// Mění celou úvahu: na sdílené paměti není PCIe, přes které by se dalo
    /// přetéct, takže „přesunout experty do RAM" nic nepřesune — jen přidá
    /// přechod mezi backendy na každý token. Optimum je tam vždycky
    /// „všechno na GPU".
    pub unified_memory: bool,
}

impl MachineProfile {
    /// Profil stroje bez použitelné GPU. Základ pro testy i pojistka,
    /// když detekce selže.
    pub fn cpu_only(cpu_cores: usize) -> Self {
        Self {
            vram_bytes: None,
            device_index: None,
            cpu_cores,
            unified_memory: false,
        }
    }
}

/// Zvolená strategie. Slouží hlavně k logování a testům — konkrétní
/// parametry pro llama.cpp nese `OffloadDecision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffloadPlan {
    /// Bez GPU (vypnuto uživatelem nebo karta není).
    Cpu,
    /// Model se do VRAM vejde celý.
    FullGpu,
    /// MoE: všechny vrstvy na GPU, tenzory expertů v RAM.
    HybridMoe,
    /// Sdílená paměť (Apple Silicon) — všechno na GPU, žádné dělení.
    UnifiedGpu,
    /// Hustý model větší než VRAM — na GPU jde jen tolik vrstev, kolik
    /// se vejde. Pomalejší než hybrid, ale u hustých modelů není zbytí.
    PartialLayers,
}

impl OffloadPlan {
    pub fn label(self) -> &'static str {
        match self {
            OffloadPlan::Cpu => "CPU",
            OffloadPlan::FullGpu => "celý model na GPU",
            OffloadPlan::HybridMoe => "hybrid MoE (experti v RAM, attention na GPU)",
            OffloadPlan::UnifiedGpu => "celý model na GPU (sdílená paměť)",
            OffloadPlan::PartialLayers => "částečný offload vrstev",
        }
    }
}

/// Konkrétní parametry, které si vezme `LlamaModelParams` / `LlamaContextParams`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffloadDecision {
    pub plan: OffloadPlan,
    pub gpu_layers: u32,
    /// Přesunout tenzory expertů do RAM (`--cpu-moe`).
    pub cpu_moe: bool,
    /// `false` = neposílat jednotlivé operace na GPU, když tam nejsou váhy.
    pub op_offload: bool,
    pub threads: i32,
    /// Kvantovat KV cache na Q8_0 — u paměťově napjatých plánů ušetří
    /// polovinu VRAM za KV při zanedbatelné ztrátě kvality.
    pub quantized_kv: bool,
    /// Na kterém zařízení běžet (`None` = nechat volbu na llama.cpp).
    pub device_index: Option<usize>,
    /// Vysvětlení pro log a UI.
    pub reason: String,
}

/// Kolik vláken dát llama.cpp: **fyzická jádra**, ne logická vlákna.
///
/// V hybridním profilu na počtu vláken skoro nezáleží — decode je omezený
/// propustností systémové paměti, ne výpočtem, protože experti se streamují
/// z RAM. Naměřený rozptyl dvou běhů téže konfigurace (16 vláken: 16,94 a
/// 18,43 tok/s) byl větší než rozdíl mezi 6 a 22 vlákny. Fyzická jádra jsou
/// zvolená hlavně proto, že jsou předvídatelná: nepřekročí stroj, nespoléhají
/// na SMT sourozence (ti u paměťově omezené úlohy nepřidají) a odpovídají
/// tomu, co dělá llama.cpp samo.
///
/// Strop 32 je tam, kde končí měření — u strojů s víc jádry netvrdíme nic.
pub fn tune_threads(physical_cores: usize) -> i32 {
    physical_cores.clamp(1, 32) as i32
}

/// Odhad velikosti KV cache pro dané okno kontextu.
fn estimate_kv_bytes(info: &GgufInfo, context_tokens: u32, quantized: bool) -> u64 {
    let bytes_per_element: u64 = if quantized { 1 } else { 2 };
    let blocks = info.block_count.unwrap_or(0) as u64;

    // Rozměr K/V hlavy: buď explicitně z metadat (Gemma), nebo
    // embedding / počet hlav.
    let head_dim =
        info.key_length
            .map(u64::from)
            .or_else(|| match (info.embedding_length, info.head_count) {
                (Some(emb), Some(heads)) if heads > 0 => Some((emb / heads) as u64),
                _ => None,
            });
    let kv_heads = info.head_count_kv.map(u64::from);

    match (blocks, head_dim, kv_heads) {
        (b, Some(dim), Some(heads)) if b > 0 && dim > 0 && heads > 0 => {
            // K i V, tedy ×2.
            context_tokens as u64 * b * heads * dim * 2 * bytes_per_element
        }
        _ => {
            let raw = (context_tokens as u64).div_ceil(1024) * KV_FALLBACK_BYTES_PER_1K;
            if quantized {
                raw / 2
            } else {
                raw
            }
        }
    }
}

/// Naplánuje offload pro konkrétní model na konkrétním stroji.
///
/// `model_bytes` je velikost GGUF souboru — u kvantovaného modelu odpovídá
/// velikosti vah v paměti dost přesně na to, aby se podle ní rozhodovalo.
pub fn plan_offload(
    model_bytes: u64,
    info: &GgufInfo,
    machine: &MachineProfile,
    context_tokens: u32,
    use_gpu: bool,
) -> OffloadDecision {
    let threads = tune_threads(machine.cpu_cores);

    let cpu_only = |reason: String| OffloadDecision {
        plan: OffloadPlan::Cpu,
        gpu_layers: 0,
        cpu_moe: false,
        op_offload: true,
        threads,
        quantized_kv: false,
        device_index: None,
        reason,
    };

    if !use_gpu {
        return cpu_only("GPU je vypnutá v nastavení.".into());
    }
    let Some(vram) = machine.vram_bytes.filter(|v| *v > 0) else {
        return cpu_only("Nenalezena použitelná GPU — počítám na CPU.".into());
    };

    let budget = vram.saturating_sub(VRAM_SAFETY_MARGIN_BYTES);
    if budget <= COMPUTE_BUFFER_BYTES {
        return cpu_only(format!(
            "VRAM ({} MB) je po rezervě na plochu příliš malá.",
            vram / (1024 * 1024)
        ));
    }
    let weights_budget = budget - COMPUTE_BUFFER_BYTES;

    // 0) Sdílená paměť (Apple Silicon). Tady se nic nedělí: neexistuje
    //    sběrnice, přes kterou by se váhy tahaly tam a zpět, takže jediné,
    //    co dělení vrstev nebo `cpu_moe` přinese, je přechod mezi backendy
    //    na každý token. Rozhoduje se jen o tom, jestli se to vejde do
    //    pracovní množiny, kterou Metal povolí.
    if machine.unified_memory {
        let kv_f16 = estimate_kv_bytes(info, context_tokens, false);
        let (quantized_kv, kv) = if model_bytes + kv_f16 <= weights_budget {
            (false, kv_f16)
        } else {
            (true, estimate_kv_bytes(info, context_tokens, true))
        };

        if model_bytes + kv <= weights_budget {
            return OffloadDecision {
                plan: OffloadPlan::UnifiedGpu,
                gpu_layers: ALL_GPU_LAYERS,
                // Obojí záměrně jako na plné GPU: `cpu_moe` by experty
                // nikam nepřesunulo (paměť je jedna) a `op_offload = false`
                // řeší PCIe, které tu není.
                cpu_moe: false,
                op_offload: true,
                threads,
                quantized_kv,
                device_index: machine.device_index,
                reason: format!(
                    "Sdílená paměť: model ({} MB) se vejde do pracovní množiny GPU                      ({} MB) — všechno na GPU{}.",
                    model_bytes / (1024 * 1024),
                    vram / (1024 * 1024),
                    if quantized_kv { " s kvantovanou KV cache" } else { "" }
                ),
            };
        }
        // Nevejde se ani s uskrovněnou KV cache — propadne to k dělení
        // vrstev níž, ale bez `cpu_moe`.
    }

    // 1) Vejde se celý model i s KV cache? Pak žádná kouzla nepotřebujeme.
    let kv_f16 = estimate_kv_bytes(info, context_tokens, false);
    if model_bytes + kv_f16 <= weights_budget {
        return OffloadDecision {
            plan: OffloadPlan::FullGpu,
            gpu_layers: ALL_GPU_LAYERS,
            cpu_moe: false,
            op_offload: true,
            threads,
            quantized_kv: false,
            device_index: machine.device_index,
            reason: format!(
                "Model ({} MB) se vejde do VRAM ({} MB) — všechny vrstvy na GPU.",
                model_bytes / (1024 * 1024),
                vram / (1024 * 1024)
            ),
        };
    }

    // 2) Nevejde se s F16 KV cache? Zkusit ji kvantovat, než začneme dělit
    //    vrstvy. Q8_0 KV stojí zanedbatelnou kvalitu, kdežto jediná vrstva
    //    na CPU znamená u každého tokenu skok mezi GPU a CPU a zpátky —
    //    naměřeno na 24B modelu, kde 39 ze 40 vrstev na GPU jelo hůř než
    //    cokoli jiného.
    let kv_q8 = estimate_kv_bytes(info, context_tokens, true);
    if model_bytes + kv_q8 <= weights_budget {
        return OffloadDecision {
            plan: OffloadPlan::FullGpu,
            gpu_layers: ALL_GPU_LAYERS,
            cpu_moe: false,
            op_offload: true,
            threads,
            quantized_kv: true,
            device_index: machine.device_index,
            reason: format!(
                "Model ({} MB) se do VRAM ({} MB) vejde s kvantovanou KV cache —                  všechny vrstvy na GPU.",
                model_bytes / (1024 * 1024),
                vram / (1024 * 1024)
            ),
        };
    }

    // Od téhle chvíle je model větší než VRAM i s uskrovněnou KV cache.
    let after_kv = weights_budget.saturating_sub(kv_q8);

    // 3) MoE s oddělenou VRAM: experti do RAM, zbytek (attention, embeddings,
    //    normy) na GPU. Na sdílené paměti se sem nedostaneme — tam by to
    //    znamenalo jen přechody mezi backendy bez jakéhokoli zisku.
    if info.is_moe() && !machine.unified_memory {
        let resident = (model_bytes as f64 * (1.0 - MOE_EXPERT_WEIGHT_SHARE)) as u64;
        if resident <= after_kv {
            return OffloadDecision {
                plan: OffloadPlan::HybridMoe,
                gpu_layers: ALL_GPU_LAYERS,
                cpu_moe: true,
                // Klíčové: bez tohohle jde první token ~2× pomaleji,
                // protože llama.cpp tahá operace nad CPU tenzory na GPU.
                op_offload: false,
                threads,
                quantized_kv: true,
                device_index: machine.device_index,
                reason: format!(
                    "MoE model ({} MB) je větší než VRAM ({} MB) — experti zůstávají \
                     v RAM, attention a KV cache jedou na GPU.",
                    model_bytes / (1024 * 1024),
                    vram / (1024 * 1024)
                ),
            };
        }
    }

    // 3) Hustý model (nebo MoE, kde se ani attention nevejde) — offloadneme
    //    tolik vrstev, kolik se vejde.
    let blocks = info.block_count.unwrap_or(0);
    if blocks == 0 {
        return cpu_only(
            "Model je větší než VRAM a hlavička neuvádí počet vrstev — počítám na CPU.".into(),
        );
    }
    let bytes_per_layer = (model_bytes / blocks as u64).max(1);
    let fits = (after_kv / bytes_per_layer) as u32;
    if fits == 0 {
        return cpu_only(format!(
            "Do VRAM ({} MB) se nevejde ani jedna vrstva modelu — počítám na CPU.",
            vram / (1024 * 1024)
        ));
    }
    let gpu_layers = fits.min(blocks);
    OffloadDecision {
        plan: OffloadPlan::PartialLayers,
        gpu_layers,
        cpu_moe: info.is_moe() && !machine.unified_memory,
        op_offload: false,
        threads,
        quantized_kv: true,
        device_index: machine.device_index,
        reason: format!(
            "Hustý model ({} MB) je větší než VRAM ({} MB) — na GPU jde {gpu_layers} z \
             {blocks} vrstev, zbytek počítá CPU.",
            model_bytes / (1024 * 1024),
            vram / (1024 * 1024)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    fn moe_info() -> GgufInfo {
        // Gemma 4 26B A4B — 128 expertů, 48 vrstev, 4 KV hlavy à 256.
        GgufInfo {
            architecture: "gemma4".into(),
            expert_count: Some(128),
            block_count: Some(48),
            embedding_length: Some(2560),
            head_count: Some(8),
            head_count_kv: Some(4),
            key_length: Some(256),
            value_length: Some(256),
        }
    }

    fn dense_info() -> GgufInfo {
        GgufInfo {
            architecture: "llama".into(),
            expert_count: None,
            block_count: Some(32),
            embedding_length: Some(4096),
            head_count: Some(32),
            head_count_kv: Some(8),
            key_length: None,
            value_length: None,
        }
    }

    fn machine(vram_gb: u64) -> MachineProfile {
        MachineProfile {
            unified_memory: false,
            vram_bytes: Some(vram_gb * GB),
            device_index: Some(1),
            cpu_cores: 16,
        }
    }

    #[test]
    fn big_moe_on_small_card_keeps_experts_in_ram() {
        // Přesně uživatelův případ: 16 GB model, 8 GB karta.
        let d = plan_offload(16 * GB, &moe_info(), &machine(8), 8192, true);
        assert_eq!(d.plan, OffloadPlan::HybridMoe);
        assert_eq!(d.gpu_layers, ALL_GPU_LAYERS);
        assert!(d.cpu_moe);
        assert!(!d.op_offload, "op_offload musí být vypnutý (TTFT)");
        assert!(d.quantized_kv);
    }

    #[test]
    fn model_that_fits_goes_fully_to_gpu() {
        let d = plan_offload(6 * GB, &dense_info(), &machine(24), 4096, true);
        assert_eq!(d.plan, OffloadPlan::FullGpu);
        assert_eq!(d.gpu_layers, ALL_GPU_LAYERS);
        assert!(!d.cpu_moe);
        assert!(d.op_offload);
    }

    /// Mistral Small 24B na RTX 5070 Ti — přesná čísla z testerova logu.
    /// Model se na kartu vejde, ale plánovač ho poslal do dělení vrstev
    /// (39 ze 40 na GPU), takže každý token skákal na CPU a zpátky.
    #[test]
    fn dense_model_that_fits_does_not_get_split() {
        let dense_24b = GgufInfo {
            architecture: "llama".into(),
            expert_count: None,
            block_count: Some(40),
            embedding_length: Some(5120),
            head_count: Some(32),
            head_count_kv: Some(8),
            key_length: None,
            value_length: None,
        };
        let machine = MachineProfile {
            unified_memory: false,
            // Volná paměť karty, jak ji hlásí ggml (celkem 15 995 MB).
            vram_bytes: Some(15_227 * 1024 * 1024),
            device_index: Some(0),
            cpu_cores: 24,
        };

        let d = plan_offload(13_669 * 1024 * 1024, &dense_24b, &machine, 4096, true);
        assert_eq!(
            d.plan,
            OffloadPlan::FullGpu,
            "model se vejde, dělit vrstvy nemá co: {}",
            d.reason
        );
        assert_eq!(d.gpu_layers, ALL_GPU_LAYERS);
    }

    #[test]
    fn quantized_kv_is_preferred_over_splitting_layers() {
        // Model, na který F16 KV cache nezbude, ale s Q8_0 se vejde celý.
        // Jedna vrstva na CPU je horší než uskrovněná KV cache.
        // 5GB model, 16k kontext: F16 KV (2 GB) se nevejde, Q8_0 (1 GB) ano.
        let info = dense_info();
        let machine = MachineProfile {
            unified_memory: false,
            vram_bytes: Some(7 * GB + 128 * 1024 * 1024),
            device_index: Some(0),
            cpu_cores: 8,
        };
        let d = plan_offload(5 * GB, &info, &machine, 16_384, true);
        assert_eq!(d.plan, OffloadPlan::FullGpu, "{}", d.reason);
        assert!(d.quantized_kv, "KV cache se má kvantovat, ne dělit vrstvy");
    }

    #[test]
    fn big_dense_model_offloads_only_what_fits() {
        let d = plan_offload(40 * GB, &dense_info(), &machine(8), 4096, true);
        assert_eq!(d.plan, OffloadPlan::PartialLayers);
        assert!(d.gpu_layers > 0 && d.gpu_layers < 32, "{}", d.gpu_layers);
        assert!(!d.cpu_moe);
    }

    #[test]
    fn dense_model_without_layer_count_falls_back_to_cpu() {
        let info = GgufInfo {
            block_count: None,
            ..dense_info()
        };
        let d = plan_offload(40 * GB, &info, &machine(8), 4096, true);
        assert_eq!(d.plan, OffloadPlan::Cpu);
    }

    #[test]
    fn gpu_disabled_means_cpu() {
        let d = plan_offload(16 * GB, &moe_info(), &machine(24), 4096, false);
        assert_eq!(d.plan, OffloadPlan::Cpu);
        assert_eq!(d.gpu_layers, 0);
    }

    #[test]
    fn no_gpu_means_cpu() {
        let machine = MachineProfile {
            unified_memory: false,
            vram_bytes: None,
            device_index: None,
            cpu_cores: 8,
        };
        let d = plan_offload(16 * GB, &moe_info(), &machine, 4096, true);
        assert_eq!(d.plan, OffloadPlan::Cpu);
    }

    #[test]
    fn tiny_card_is_not_worth_it() {
        // 1 GB iGPU: po rezervě na plochu nezbude ani na compute buffery.
        let d = plan_offload(16 * GB, &moe_info(), &machine(1), 4096, true);
        assert_eq!(d.plan, OffloadPlan::Cpu);
    }

    #[test]
    fn threads_follow_physical_cores() {
        assert_eq!(tune_threads(16), 16); // Core Ultra 9 185H: 6 P + 8 E + 2 LP-E
        assert_eq!(tune_threads(8), 8);
        assert_eq!(tune_threads(64), 32); // strop, kde končí měření
        assert_eq!(tune_threads(1), 1);
        assert_eq!(tune_threads(0), 1);
    }

    #[test]
    fn chosen_device_is_carried_into_every_gpu_plan() {
        // Kdyby se index ztratil, llama.cpp by sáhlo po nultém zařízení —
        // na hybridním notebooku po integrované grafice.
        let moe = plan_offload(16 * GB, &moe_info(), &machine(8), 8192, true);
        let full = plan_offload(2 * GB, &dense_info(), &machine(24), 4096, true);
        let partial = plan_offload(40 * GB, &dense_info(), &machine(8), 4096, true);

        assert_eq!(moe.device_index, Some(1));
        assert_eq!(full.device_index, Some(1));
        assert_eq!(partial.device_index, Some(1));
    }

    #[test]
    fn cpu_plan_has_no_device() {
        let d = plan_offload(16 * GB, &moe_info(), &machine(8), 4096, false);
        assert_eq!(d.device_index, None);
    }

    #[test]
    fn kv_estimate_scales_with_context_and_halves_when_quantized() {
        let info = moe_info();
        let small = estimate_kv_bytes(&info, 4096, false);
        let big = estimate_kv_bytes(&info, 8192, false);
        assert_eq!(big, small * 2);
        assert_eq!(estimate_kv_bytes(&info, 4096, true), small / 2);
    }

    #[test]
    fn kv_estimate_has_fallback_without_metadata() {
        let info = GgufInfo {
            architecture: "mystery".into(),
            ..Default::default()
        };
        assert!(estimate_kv_bytes(&info, 4096, false) > 0);
    }

    #[test]
    fn growing_context_degrades_in_steps_not_all_at_once() {
        // Jak roste kontext, ubývá místa na váhy — plán ale neskáče rovnou
        // na dělení, nejdřív uskrovní KV cache.
        let info = moe_info();

        let small = plan_offload(4 * GB, &info, &machine(8), 4096, true);
        assert_eq!(small.plan, OffloadPlan::FullGpu);
        assert!(!small.quantized_kv, "při malém okně není co šetřit");

        let medium = plan_offload(4 * GB, &info, &machine(8), 32_768, true);
        assert_eq!(medium.plan, OffloadPlan::FullGpu, "{}", medium.reason);
        assert!(medium.quantized_kv, "velké okno se vejde s Q8_0 KV cache");

        // Větší model už se nevejde ani tak — experti jdou do RAM.
        let big = plan_offload(16 * GB, &info, &machine(8), 32_768, true);
        assert_eq!(big.plan, OffloadPlan::HybridMoe, "{}", big.reason);
    }

    // --- Sdílená paměť (Apple Silicon) ---

    fn apple_silicon(working_set_gb: u64) -> MachineProfile {
        MachineProfile {
            unified_memory: true,
            // Metal nehlásí „volnou VRAM", ale doporučenou pracovní množinu —
            // zhruba 75 % systémové paměti.
            vram_bytes: Some(working_set_gb * GB),
            device_index: Some(0),
            // M-čka mají výkonná a úsporná jádra jako Intel; počítají se
            // fyzická, stejně jako na x86.
            cpu_cores: 12,
        }
    }

    #[test]
    fn sdilena_pamet_da_vsechno_na_gpu() {
        // MacBook s 32 GB → Metal pustí ~24 GB, 16GB MoE model se vejde.
        let d = plan_offload(16 * GB, &moe_info(), &apple_silicon(24), 8192, true);
        assert_eq!(d.plan, OffloadPlan::UnifiedGpu, "{}", d.reason);
        assert_eq!(d.gpu_layers, ALL_GPU_LAYERS);
        assert!(
            !d.cpu_moe,
            "na sdílené paměti nemá `cpu_moe` co přesouvat — jen by přidal              přechod mezi backendy na každý token"
        );
        assert!(
            d.op_offload,
            "`op_offload = false` řeší PCIe, které na Apple Silicon není"
        );
    }

    #[test]
    fn sdilena_pamet_nikdy_nesahne_po_hybridnim_moe() {
        // Tentýž model, který na 8GB kartě skončí jako HybridMoe, nesmí
        // na sdílené paměti dopadnout stejně ani když se nevejde.
        let na_karte = plan_offload(16 * GB, &moe_info(), &machine(8), 8192, true);
        assert_eq!(na_karte.plan, OffloadPlan::HybridMoe);

        let na_macu = plan_offload(16 * GB, &moe_info(), &apple_silicon(8), 8192, true);
        assert_ne!(na_macu.plan, OffloadPlan::HybridMoe, "{}", na_macu.reason);
        assert!(!na_macu.cpu_moe, "{}", na_macu.reason);
    }

    #[test]
    fn sdilena_pamet_sahne_po_kvantovane_kv_nez_zacne_delit() {
        // Model, kterému F16 KV cache přeteče, ale s Q8_0 se vejde:
        // rozpočet 7 GB, model 5 GB, KV při 16k kontextu 3 GB / 1,5 GB.
        let d = plan_offload(5 * GB, &moe_info(), &apple_silicon(8), 16_384, true);
        assert_eq!(d.plan, OffloadPlan::UnifiedGpu, "{}", d.reason);
        assert!(d.quantized_kv, "{}", d.reason);
    }

    #[test]
    fn vypnuta_gpu_plati_i_na_sdilene_pameti() {
        let d = plan_offload(16 * GB, &moe_info(), &apple_silicon(24), 8192, false);
        assert_eq!(d.plan, OffloadPlan::Cpu);
    }

    #[test]
    fn cpu_only_profil_nema_gpu() {
        let d = plan_offload(
            4 * GB,
            &dense_info(),
            &MachineProfile::cpu_only(8),
            4096,
            true,
        );
        assert_eq!(d.plan, OffloadPlan::Cpu);
        assert_eq!(d.gpu_layers, 0);
    }
}
