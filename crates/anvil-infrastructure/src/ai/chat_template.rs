//! Skládání promptů a čištění výstupu pro jednotlivé rodiny modelů.
//!
//! Šablonu z GGUF metadat (`apply_chat_template`) záměrně nepoužíváme.
//! U Gemmy 4 vrací `ffi error -1` i pro jedinou uživatelskou zprávu, takže
//! by se stejně musela obcházet — a obecný ChatML fallback model zmate
//! natolik, že začne odpovídat ve svém interním formátu. Explicitní šablony
//! mají navíc tu výhodu, že jdou otestovat bez načteného modelu.
//!
//! Na výstupní straně řeší modul totéž z druhé strany: modely prokládají
//! odpověď značkami, které do textu nepatří (`<think>` u Qwen3, kanál
//! `thought` u Gemmy). Značka běžně přijde **rozsekaná mezi dva tokeny**,
//! takže filtr musí umět držet konec dávky zpátky, dokud neví, jestli je to
//! začátek značky nebo obyčejný text.

use anvil_domain::{
    conversation::{Message, Role},
    model::ChatTemplateKind,
};

use super::gemma;

const IM_START: &str = "<|im_start|>";
const IM_END: &str = "<|im_end|>";

/// Značka, kterou musí prompt pro Gemmu 4 končit.
///
/// Bez ní model píše do kanálu `thought` a z několika set tokenů se do
/// odpovědi nedostane nic — filtr je všechny zahodí jako vnitřní uvažování.
const GEMMA_FINAL_CHANNEL: &str = "<|channel>final<channel|>";

/// Uvození výstupu nástroje. Ani jeden z podporovaných modelů nemá vlastní
/// roli pro výsledky nástrojů, takže se vkládají jako uživatelský tah
/// s jasnou hlavičkou — model tak pozná, že to nepsal člověk.
const TOOL_RESULT_HEADER: &str = "[výstup nástroje]";

/// Poskládá prompt pro jeden tah modelu.
///
/// `summary` je souhrn starších zpráv po sloučení kontextu — připojuje se
/// k systémové instrukci, protože patří k zadání, ne k rozhovoru.
pub fn build_prompt(
    kind: ChatTemplateKind,
    system: Option<&str>,
    summary: Option<&str>,
    messages: &[Message],
) -> String {
    let system = merge_system(system, summary);
    match kind {
        ChatTemplateKind::Gemma4 => build_gemma(system.as_deref(), messages),
        ChatTemplateKind::Qwen3 | ChatTemplateKind::ChatMl => {
            build_chatml(system.as_deref(), messages)
        }
    }
}

/// Sloučí systémovou instrukci se souhrnem konverzace.
fn merge_system(system: Option<&str>, summary: Option<&str>) -> Option<String> {
    let system = system.map(str::trim).filter(|s| !s.is_empty());
    let summary = summary.map(str::trim).filter(|s| !s.is_empty());
    match (system, summary) {
        (None, None) => None,
        (Some(s), None) => Some(s.to_string()),
        (None, Some(sum)) => Some(format!("Shrnutí dosavadní konverzace:\n{sum}")),
        (Some(s), Some(sum)) => Some(format!("{s}\n\nShrnutí dosavadní konverzace:\n{sum}")),
    }
}

/// Text zprávy tak, jak má jít do promptu.
fn rendered_content(message: &Message) -> String {
    match message.role {
        Role::Tool => format!("{TOOL_RESULT_HEADER}\n{}", message.content),
        _ => message.content.clone(),
    }
}

/// Gemma 4 nezná roli `system` — systémová instrukce se předřazuje prvnímu
/// uživatelskému tahu. Když žádný uživatelský tah není, vloží se jako
/// samostatný tah, aby se instrukce neztratila.
fn build_gemma(system: Option<&str>, messages: &[Message]) -> String {
    let mut out = String::new();
    let mut system_pending = system;

    for message in messages {
        match message.role {
            Role::System => continue, // systémová instrukce se předává zvlášť
            Role::Assistant => {
                out.push_str("<start_of_turn>model\n");
                out.push_str(&message.content);
                out.push_str(gemma::END_OF_TURN);
                out.push('\n');
            }
            Role::User | Role::Tool => {
                out.push_str("<start_of_turn>user\n");
                if let Some(s) = system_pending.take() {
                    out.push_str(s);
                    out.push_str("\n\n");
                }
                out.push_str(&rendered_content(message));
                out.push_str(gemma::END_OF_TURN);
                out.push('\n');
            }
        }
    }

    if let Some(s) = system_pending {
        out.push_str("<start_of_turn>user\n");
        out.push_str(s);
        out.push_str(gemma::END_OF_TURN);
        out.push('\n');
    }

    out.push_str("<start_of_turn>model\n");
    out.push_str(GEMMA_FINAL_CHANNEL);
    out
}

fn build_chatml(system: Option<&str>, messages: &[Message]) -> String {
    let mut out = String::new();

    if let Some(s) = system {
        out.push_str(IM_START);
        out.push_str("system\n");
        out.push_str(s);
        out.push_str(IM_END);
        out.push('\n');
    }

    for message in messages {
        let role = match message.role {
            Role::System => continue,
            Role::Assistant => "assistant",
            Role::User | Role::Tool => "user",
        };
        out.push_str(IM_START);
        out.push_str(role);
        out.push('\n');
        out.push_str(&rendered_content(message));
        out.push_str(IM_END);
        out.push('\n');
    }

    out.push_str(IM_START);
    out.push_str("assistant\n");
    out
}

/// Sekvence, na kterých se má generování zastavit.
pub fn stop_sequences(kind: ChatTemplateKind) -> &'static [&'static str] {
    match kind {
        ChatTemplateKind::Gemma4 => &["<end_of_turn>", "<start_of_turn>"],
        ChatTemplateKind::Qwen3 | ChatTemplateKind::ChatMl => &["<|im_end|>", "<|im_start|>"],
    }
}

// --- Čištění výstupu ------------------------------------------------------

/// Odstraňuje z proudu textu úseky mezi dvěma značkami.
///
/// Nad rámec prostého `replace` umí to podstatné: **značka může přijít
/// rozdělená mezi dvě dávky tokenů**. Filtr proto drží konec dávky zpátky,
/// dokud si není jistý, že nejde o začátek značky. Kdyby to nedělal,
/// prosákl by do textu kus `<thi` a zbytek by se zahodil.
#[derive(Debug, Clone)]
pub struct TagStripper {
    open: &'static str,
    close: &'static str,
    inside: bool,
    /// Konec vstupu, který ještě může být začátkem značky.
    pending: String,
}

impl TagStripper {
    pub fn new(open: &'static str, close: &'static str) -> Self {
        Self {
            open,
            close,
            inside: false,
            pending: String::new(),
        }
    }

    pub fn push(&mut self, delta: &str) -> String {
        self.pending.push_str(delta);
        let mut out = String::new();

        loop {
            let needle = if self.inside { self.close } else { self.open };

            if let Some(i) = self.pending.find(needle) {
                if !self.inside {
                    out.push_str(&self.pending[..i]);
                }
                self.pending.drain(..i + needle.len());
                self.inside = !self.inside;
                continue;
            }

            // Celá značka tam není. Kolik znaků na konci může být jejím
            // začátkem? Ty se podrží do příští dávky.
            let hold = longest_overlap(&self.pending, needle);
            let emit_to = self.pending.len() - hold;
            if !self.inside {
                out.push_str(&self.pending[..emit_to]);
            }
            self.pending.drain(..emit_to);
            return out;
        }
    }

    /// Uzavře proud. Nedokončená značka se pustí ven jako text — viditelný
    /// zbytek značky je menší zlo než tiše zahozený kus odpovědi.
    pub fn finish(&mut self) -> String {
        let rest = std::mem::take(&mut self.pending);
        if self.inside {
            String::new()
        } else {
            rest
        }
    }
}

/// Nejdelší konec `hay`, který je zároveň začátkem `needle` (kratší než celý
/// `needle` — celá shoda se řeší jinde).
fn longest_overlap(hay: &str, needle: &str) -> usize {
    let max = needle.len().saturating_sub(1).min(hay.len());
    for k in (1..=max).rev() {
        let start = hay.len() - k;
        if !hay.is_char_boundary(start) || !needle.is_char_boundary(k) {
            continue;
        }
        if hay.as_bytes()[start..] == needle.as_bytes()[..k] {
            return k;
        }
    }
    0
}

/// Filtr výstupu podle rodiny modelu.
#[derive(Debug)]
pub enum OutputFilter {
    /// Gemma 4 — kanály `<|channel>thought<channel|>`.
    Channels(gemma::ChannelFilter),
    /// Qwen3 — bloky `<think>…</think>`.
    Think(TagStripper),
    /// Nic se nefiltruje.
    Passthrough,
}

impl OutputFilter {
    pub fn for_template(kind: ChatTemplateKind) -> Self {
        match kind {
            ChatTemplateKind::Gemma4 => OutputFilter::Channels(gemma::ChannelFilter::new()),
            ChatTemplateKind::Qwen3 => OutputFilter::Think(TagStripper::new("<think>", "</think>")),
            ChatTemplateKind::ChatMl => OutputFilter::Passthrough,
        }
    }

    pub fn push(&mut self, delta: &str) -> String {
        match self {
            OutputFilter::Channels(f) => f.push(delta),
            OutputFilter::Think(f) => f.push(delta),
            OutputFilter::Passthrough => delta.to_string(),
        }
    }

    pub fn finish(&mut self) -> String {
        match self {
            OutputFilter::Channels(f) => f.finish(),
            OutputFilter::Think(f) => f.finish(),
            OutputFilter::Passthrough => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zpravy() -> Vec<Message> {
        vec![
            Message::user("první dotaz"),
            Message::assistant("první odpověď"),
            Message::user("druhý dotaz"),
        ]
    }

    // --- Gemma 4 ---

    #[test]
    fn gemma_konci_kanalem_final() {
        // Bez téhle značky model píše do kanálu `thought` a filtr pak
        // zahodí celou odpověď.
        let p = build_prompt(ChatTemplateKind::Gemma4, None, None, &zpravy());
        assert!(
            p.ends_with(GEMMA_FINAL_CHANNEL),
            "prompt musí končit kanálem final, končí: {:?}",
            &p[p.len().saturating_sub(60)..]
        );
    }

    #[test]
    fn gemma_nepouziva_roli_system() {
        // Gemma roli `system` nezná — instrukce patří do prvního user tahu.
        let p = build_prompt(
            ChatTemplateKind::Gemma4,
            Some("Jsi asistent na kód."),
            None,
            &zpravy(),
        );
        assert!(!p.contains("<start_of_turn>system"));
        let prvni_user = p.find("<start_of_turn>user\n").unwrap();
        let po = &p[prvni_user..];
        assert!(
            po.starts_with("<start_of_turn>user\nJsi asistent na kód.\n\nprvní dotaz"),
            "systémová instrukce má být na začátku prvního user tahu: {po:.120}"
        );
    }

    #[test]
    fn gemma_neztrati_instrukci_bez_uzivatelskeho_tahu() {
        let p = build_prompt(ChatTemplateKind::Gemma4, Some("Jsi asistent."), None, &[]);
        assert!(p.contains("Jsi asistent."));
    }

    #[test]
    fn gemma_strida_tahy_user_a_model() {
        let p = build_prompt(ChatTemplateKind::Gemma4, None, None, &zpravy());
        let poradi: Vec<_> = p.match_indices("<start_of_turn>").map(|(i, _)| i).collect();
        assert_eq!(poradi.len(), 4, "tři zprávy + otevřený tah modelu");
        assert!(p.contains("<start_of_turn>model\nprvní odpověď<end_of_turn>"));
    }

    // --- ChatML / Qwen3 ---

    #[test]
    fn chatml_ma_systemovy_blok_a_konci_otevrenym_asistentem() {
        let p = build_prompt(
            ChatTemplateKind::Qwen3,
            Some("Jsi asistent na kód."),
            None,
            &zpravy(),
        );
        assert!(p.starts_with("<|im_start|>system\nJsi asistent na kód.<|im_end|>\n"));
        assert!(p.ends_with("<|im_start|>assistant\n"));
    }

    #[test]
    fn chatml_bez_systemu_zacne_rovnou_uzivatelem() {
        let p = build_prompt(ChatTemplateKind::ChatMl, None, None, &zpravy());
        assert!(p.starts_with("<|im_start|>user\n"));
    }

    // --- společné ---

    #[test]
    fn souhrn_se_pripoji_k_systemove_instrukci() {
        let p = build_prompt(
            ChatTemplateKind::Qwen3,
            Some("Jsi asistent."),
            Some("Uživatel řešil chybu v parseru."),
            &zpravy(),
        );
        assert!(p.contains("Jsi asistent."));
        assert!(p.contains("Shrnutí dosavadní konverzace:\nUživatel řešil chybu v parseru."));
        // A jen jednou, ne u každého tahu.
        assert_eq!(p.matches("Shrnutí dosavadní konverzace").count(), 1);
    }

    #[test]
    fn samotny_souhrn_bez_instrukce_taky_projde() {
        let p = build_prompt(
            ChatTemplateKind::Qwen3,
            None,
            Some("Něco se dělo."),
            &zpravy(),
        );
        assert!(p.contains("Shrnutí dosavadní konverzace:\nNěco se dělo."));
    }

    #[test]
    fn prazdna_instrukce_se_nebere_jako_instrukce() {
        let p = build_prompt(ChatTemplateKind::Qwen3, Some("   "), None, &zpravy());
        assert!(!p.contains("<|im_start|>system"));
    }

    #[test]
    fn vystup_nastroje_je_oznaceny() {
        // Model musí poznat, že to nepsal člověk.
        let zpravy = vec![
            Message::user("najdi chyby"),
            Message::new(Role::Tool, "src/main.rs:12: unwrap na None"),
        ];
        for kind in [ChatTemplateKind::Gemma4, ChatTemplateKind::Qwen3] {
            let p = build_prompt(kind, None, None, &zpravy);
            assert!(p.contains(TOOL_RESULT_HEADER), "{kind:?}");
            assert!(p.contains("src/main.rs:12"), "{kind:?}");
        }
    }

    #[test]
    fn systemova_zprava_v_historii_se_ignoruje() {
        // Systémová instrukce se skládá při každém tahu znovu; kdyby prosákla
        // i z historie, byla by v promptu dvakrát.
        let zpravy = vec![
            Message::new(Role::System, "stará instrukce"),
            Message::user("ahoj"),
        ];
        for kind in [ChatTemplateKind::Gemma4, ChatTemplateKind::Qwen3] {
            let p = build_prompt(kind, None, None, &zpravy);
            assert!(!p.contains("stará instrukce"), "{kind:?}");
        }
    }

    #[test]
    fn stop_sekvence_odpovidaji_sablone() {
        assert!(stop_sequences(ChatTemplateKind::Gemma4).contains(&"<end_of_turn>"));
        assert!(stop_sequences(ChatTemplateKind::Qwen3).contains(&"<|im_end|>"));
    }

    // --- TagStripper ---

    fn strip_po_kusech(vstup: &[&str]) -> String {
        let mut f = TagStripper::new("<think>", "</think>");
        let mut out = String::new();
        for kus in vstup {
            out.push_str(&f.push(kus));
        }
        out.push_str(&f.finish());
        out
    }

    #[test]
    fn think_blok_se_odstrani() {
        assert_eq!(
            strip_po_kusech(&["před <think>uvažování</think> po"]),
            "před  po"
        );
    }

    #[test]
    fn znacka_rozsekana_mezi_tokeny_se_pozna() {
        // Tohle je ten skutečný případ: tokenizer značku rozdělí.
        assert_eq!(
            strip_po_kusech(&["před <", "thi", "nk>uvažování<", "/thi", "nk> po"]),
            "před  po"
        );
    }

    #[test]
    fn znacka_po_jednom_znaku_se_pozna() {
        let vstup: Vec<String> = "A<think>B</think>C"
            .chars()
            .map(|c| c.to_string())
            .collect();
        let refs: Vec<&str> = vstup.iter().map(String::as_str).collect();
        assert_eq!(strip_po_kusech(&refs), "AC");
    }

    #[test]
    fn text_pripominajici_znacku_se_nezahodi() {
        assert_eq!(strip_po_kusech(&["a < b a <t c"]), "a < b a <t c");
        assert_eq!(
            strip_po_kusech(&["porovnání: x <thin y"]),
            "porovnání: x <thin y"
        );
    }

    #[test]
    fn nedokoncena_znacka_na_konci_se_pusti_ven() {
        // Radši viditelný zbytek značky než tiše zahozený text.
        assert_eq!(strip_po_kusech(&["hotovo <thi"]), "hotovo <thi");
    }

    #[test]
    fn neuzavreny_think_blok_nepropusti_uvazovani() {
        // Opačný případ: blok se otevřel a nikdy nezavřel — vnitřek ven nesmí.
        assert_eq!(strip_po_kusech(&["text <think>tajné uvažování"]), "text ");
    }

    #[test]
    fn diakritika_prezije_rozsekani() {
        // Držení konce dávky nesmí rozseknout vícebajtový znak.
        assert_eq!(
            strip_po_kusech(&["příliš žluťoučký ", "kůň <think>x</think> úpěl"]),
            "příliš žluťoučký kůň  úpěl"
        );
    }

    #[test]
    fn vic_bloku_za_sebou() {
        assert_eq!(
            strip_po_kusech(&["a<think>1</think>b<think>2</think>c"]),
            "abc"
        );
    }

    #[test]
    fn passthrough_nic_nemeni() {
        let mut f = OutputFilter::for_template(ChatTemplateKind::ChatMl);
        assert_eq!(f.push("<think>ponechat</think>"), "<think>ponechat</think>");
        assert_eq!(f.finish(), "");
    }

    #[test]
    fn filtr_pro_qwen3_odstranuje_think() {
        let mut f = OutputFilter::for_template(ChatTemplateKind::Qwen3);
        let mut out = f.push("a<think>b</think>c");
        out.push_str(&f.finish());
        assert_eq!(out, "ac");
    }
}
