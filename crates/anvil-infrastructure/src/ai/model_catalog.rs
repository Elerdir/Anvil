//! Kurátorovaný katalog modelů.
//!
//! Výběr se řídí jediným pravidlem, které na běžném notebooku platí:
//! **rozhodují aktivní parametry, ne celková velikost.** Hustý model nad 30 B
//! jede jednotky tokenů za sekundu a je nepoužitelný; řídký MoE s ~3 B
//! aktivními běží řádově rychleji při srovnatelné kvalitě. Proto jsou tu
//! jen řídké modely.
//!
//! Druhé pravidlo je smířlivější: **nejlepší model na kód a nejlepší model
//! na češtinu jsou dnes dva různé modely.** Katalog to nezastírá — má dvě
//! role a u každé poctivě říká, v čem je daná volba slabá.
//!
//! Repozitáře, názvy souborů i velikosti jsou ověřené proti HuggingFace
//! (HEAD na `resolve/main`), ne opsané z hlavy. Když se velikost rozejde
//! s `Content-Length`, downloader to ohlásí.

use anvil_domain::{
    model::{ChatTemplateKind, ModelId, ModelRole, ModelSpec},
    ports::ModelCatalog,
};

/// Statický katalog. Za běhu se nemění.
pub struct StaticModelCatalog;

impl ModelCatalog for StaticModelCatalog {
    fn all(&self) -> Vec<ModelSpec> {
        default_catalog()
    }
}

fn id(value: &str) -> ModelId {
    ModelId::parse(value).expect("ID modelů v katalogu jsou platná (kryto testem)")
}

pub fn default_catalog() -> Vec<ModelSpec> {
    vec![
        // === PROGRAMOVÁNÍ ===
        ModelSpec {
            id: id("qwen3-coder-30b-a3b-ud-q4-k-xl"),
            name: "Qwen3-Coder 30B-A3B (UD-Q4_K_XL)".into(),
            description: "Doporučená volba pro kód. Z 30,5 miliardy parametrů se pro každý \
                 token počítají jen 3,3 — díky tomu jede na běžném notebooku několikanásobně \
                 rychleji než hustý model srovnatelné kvality. Dynamický kvant od Unslothu je \
                 menší i lepší než klasický Q4_K_M. Nativní kontext 256K. \
                 Slabina: česky rozumí, ale píše kostrbatě a u delších vysvětlení sklouzává \
                 do angličtiny — na povídání si přepni roli."
                .into(),
            role: ModelRole::Coding,
            repo: "unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF".into(),
            file: "Qwen3-Coder-30B-A3B-Instruct-UD-Q4_K_XL.gguf".into(),
            size_bytes: 17_665_334_432,
            template: ChatTemplateKind::Qwen3,
            gated: false,
            recommended: true,
            active_params_b: 3.3,
            total_params_b: 30.5,
            native_context_tokens: 262_144,
        },
        ModelSpec {
            id: id("qwen3-coder-30b-a3b-q4-k-m"),
            name: "Qwen3-Coder 30B-A3B (Q4_K_M)".into(),
            description: "Tentýž model v klasickém Q4_K_M. O gigabajt větší než UD varianta \
                 a o kousek slabší — je tu pro případ, že by dynamický kvant někde dělal potíže."
                .into(),
            role: ModelRole::Coding,
            repo: "unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF".into(),
            file: "Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf".into(),
            size_bytes: 18_556_689_568,
            template: ChatTemplateKind::Qwen3,
            gated: false,
            recommended: false,
            active_params_b: 3.3,
            total_params_b: 30.5,
            native_context_tokens: 262_144,
        },
        ModelSpec {
            id: id("qwen3-coder-30b-a3b-ud-q3-k-xl"),
            name: "Qwen3-Coder 30B-A3B (UD-Q3_K_XL) — pro 16 GB stroje".into(),
            description: "Nižší kvant téhož modelu, o 4 GB menší. Volba pro MacBook s 16 GB \
                 sjednocené paměti nebo stroj, kde se Q4 nevejde do pracovní množiny. \
                 Kvalita kódu je znatelně nižší — sahat po něm až když Q4 nejde."
                .into(),
            role: ModelRole::Coding,
            repo: "unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF".into(),
            file: "Qwen3-Coder-30B-A3B-Instruct-UD-Q3_K_XL.gguf".into(),
            size_bytes: 13_806_312_608,
            template: ChatTemplateKind::Qwen3,
            gated: false,
            recommended: false,
            active_params_b: 3.3,
            total_params_b: 30.5,
            native_context_tokens: 262_144,
        },
        // === KONVERZACE A ČEŠTINA ===
        ModelSpec {
            id: id("gemma-4-26b-a4b-it-ud-q4-k-xl"),
            name: "Gemma 4 26B-A4B it (UD-Q4_K_XL)".into(),
            description: "Doporučená volba pro češtinu. Řídký MoE se 4 miliardami aktivních \
                 parametrů, výborná čeština včetně skloňování a odborné terminologie. \
                 Na generování kódu je slabší než Qwen3-Coder — hodí se na vysvětlování, \
                 návrh řešení a diskusi nad nálezy z review."
                .into(),
            role: ModelRole::Conversational,
            repo: "unsloth/gemma-4-26B-A4B-it-GGUF".into(),
            file: "gemma-4-26B-A4B-it-UD-Q4_K_XL.gguf".into(),
            size_bytes: 17_010_980_576,
            template: ChatTemplateKind::Gemma4,
            gated: false,
            recommended: true,
            active_params_b: 4.0,
            total_params_b: 26.0,
            native_context_tokens: 131_072,
        },
        ModelSpec {
            id: id("gemma-4-26b-a4b-it-uncensored-q4-k-m"),
            name: "Gemma 4 26B-A4B it uncensored (Q4_K_M)".into(),
            description: "Abliterovaná varianta téhož modelu — neodmítá dotazy, které \
                 základní verze odmítne (u bezpečnostních rozborů a exploit kódu se to hodí). \
                 KL divergence 0,090 proti základu, takže čeština i schopnosti zůstávají."
                .into(),
            role: ModelRole::Conversational,
            repo: "TrevorJS/gemma-4-26B-A4B-it-uncensored-GGUF".into(),
            file: "gemma-4-26B-A4B-it-uncensored-Q4_K_M.gguf".into(),
            size_bytes: 16_796_011_072,
            template: ChatTemplateKind::Gemma4,
            gated: false,
            recommended: false,
            active_params_b: 4.0,
            total_params_b: 26.0,
            native_context_tokens: 131_072,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn katalog_ma_platna_id() {
        // `id()` panikaří na neplatném vstupu — tenhle test to odhalí
        // při překladu testů, ne až uživateli za běhu.
        let katalog = default_catalog();
        assert!(!katalog.is_empty());
    }

    #[test]
    fn id_modelu_jsou_unikatni() {
        let katalog = default_catalog();
        let mut ids: Vec<_> = katalog.iter().map(|m| m.id.as_str()).collect();
        ids.sort_unstable();
        let pocet = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), pocet, "v katalogu je duplicitní ID");
    }

    #[test]
    fn kazda_role_ma_prave_jedno_doporuceni() {
        for role in ModelRole::ALL {
            let doporucene: Vec<_> = default_catalog()
                .into_iter()
                .filter(|m| m.role == role && m.recommended)
                .collect();
            assert_eq!(
                doporucene.len(),
                1,
                "role {:?} musí mít právě jedno doporučení, má {}",
                role,
                doporucene.len()
            );
        }
    }

    #[test]
    fn vsechny_modely_jsou_ridke() {
        // Hustý model nad 30 B je na cílovém železe nepoužitelný — kdyby se
        // do katalogu někdy dostal, tenhle test to zastaví.
        for m in default_catalog() {
            assert!(
                m.is_sparse(),
                "{} není řídký ({} aktivních z {} miliard)",
                m.id,
                m.active_params_b,
                m.total_params_b
            );
        }
    }

    #[test]
    fn odkazy_ke_stazeni_maji_spravny_tvar() {
        for m in default_catalog() {
            let url = m.download_url();
            assert!(url.starts_with("https://huggingface.co/"), "{url}");
            assert!(url.contains("/resolve/main/"), "{url}");
            assert!(url.ends_with(".gguf"), "{url}");
        }
    }

    #[test]
    fn velikosti_jsou_vyplnene_a_realne() {
        for m in default_catalog() {
            assert!(
                m.size_bytes > 5_000_000_000,
                "{} má nevěrohodnou velikost {}",
                m.id,
                m.size_bytes
            );
        }
    }

    #[test]
    fn katalog_najde_doporuceni_pro_obe_role() {
        let k = StaticModelCatalog;
        assert!(k.recommended(ModelRole::Coding).is_some());
        assert!(k.recommended(ModelRole::Conversational).is_some());
    }

    #[test]
    fn katalog_najde_model_podle_id() {
        let k = StaticModelCatalog;
        let hledane = id("qwen3-coder-30b-a3b-ud-q4-k-xl");
        assert_eq!(k.find(&hledane).unwrap().id, hledane);
        assert!(k.find(&id("neexistuje")).is_none());
    }
}
