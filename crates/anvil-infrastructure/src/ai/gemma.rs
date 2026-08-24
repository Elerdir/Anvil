//! Gemma 4: formát tahů a kanály ve výstupu.
//!
//! Dvě věci, které se nedají vyčíst z dokumentace, ale bez kterých model
//! nepíše použitelnou prózu:
//!
//! 1. **Šablona z GGUF nejde použít.** `apply_chat_template` na Gemmě 4 vrací
//!    `ffi error -1` — i pro jedinou uživatelskou zprávu. Obecný ChatML
//!    fallback model zmate: začne odpovídat ve svém interním formátu.
//!    Skládáme proto tahy sami, ve tvaru `<start_of_turn>role\n…<end_of_turn>`.
//!    Roli „system" Gemma nezná, systémový prompt se předřazuje prvnímu
//!    uživatelskému tahu.
//!
//! 2. **Výstup je rozdělený do kanálů** značkami `<|channel>název<channel|>`.
//!    Text v kanálu `thought` je vnitřní uvažování modelu — do kapitoly
//!    nepatří. Formát je odvozený z reálného výstupu
//!    (`gemma-4-26B-A4B-it-UD-Q4_K_XL`), ne z dokumentace, takže filtr je
//!    schválně tolerantní: co nerozpozná, propustí. Nejhorší dopad je
//!    viditelná značka, ne zahozený text.

pub const END_OF_TURN: &str = "<end_of_turn>";
const START_USER: &str = "<start_of_turn>user\n";
const START_MODEL: &str = "<start_of_turn>model\n";

const CHANNEL_OPEN: &str = "<|channel>";
const CHANNEL_CLOSE: &str = "<channel|>";
const THOUGHT: &str = "thought";

/// Delší z obou značek — o kolik znaků musí filtr umět couvnout, když
/// značka přijde rozdělená mezi dvě dávky tokenů. (`Ord::max` v konstantě
/// zatím nejde, proto ručně.)
const LONGEST_MARKER: usize = if CHANNEL_OPEN.len() > CHANNEL_CLOSE.len() {
    CHANNEL_OPEN.len()
} else {
    CHANNEL_CLOSE.len()
};

/// Je tohle Gemma? Rozhoduje `general.architecture` z GGUF hlavičky.
pub fn is_gemma(architecture: &str) -> bool {
    architecture.to_ascii_lowercase().starts_with("gemma")
}

/// Poskládá jeden uživatelský tah a otevře tah modelu.
///
/// BOS token nepřidáváme — ten řeší tokenizer (`AddBos::Never` na volající
/// straně by jinak vedl k tomu, že BOS chybí úplně, dvojitý BOS zase Gemmě
/// znatelně kazí výstup).
pub fn build_prompt(system: Option<&str>, user: &str) -> String {
    let system = system.map(str::trim).filter(|s| !s.is_empty());
    let body = match system {
        Some(system) => format!("{system}\n\n{}", user.trim()),
        None => user.trim().to_string(),
    };

    format!("{START_USER}{body}{END_OF_TURN}\n{START_MODEL}{CHANNEL_OPEN}final{CHANNEL_CLOSE}")
}

/// Odděluje viditelný text od obsahu kanálu `thought` **za běhu streamu**.
///
/// Filtr musí umět dvě věci, které při zpracování celého textu najednou
/// nevzniknou: značka může přijít rozdělená mezi dvě dávky tokenů, a text
/// se odesílá do UI dřív, než je jasné, co bude následovat. Proto se
/// nedokončený konec drží v bufferu, dokud se nerozhodne.
#[derive(Debug, Default)]
pub struct ChannelFilter {
    /// Nezpracovaný zbytek — potenciální začátek značky.
    pending: String,
    /// Uvnitř kanálu `thought`.
    hidden: bool,
    /// Rozečtená značka, u které ještě neznáme jméno kanálu.
    in_marker: bool,
    /// Jméno kanálu, jak se postupně skládá.
    channel_name: String,
    /// Co filtr zahodil. Bez toho je „kapitola vyšla prázdná" nediagnostikovatelné
    /// — z logu není poznat, jestli model mlčel, nebo jestli si všechno odmyslel.
    hidden_text: String,
}

impl ChannelFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Přidá další kus streamu a vrátí text, který smí do kapitoly.
    pub fn push(&mut self, delta: &str) -> String {
        self.pending.push_str(delta);
        let mut visible = String::new();

        loop {
            if self.in_marker {
                // Uvnitř značky sbíráme jméno kanálu až po `<channel|>`.
                match self.pending.find(CHANNEL_CLOSE) {
                    Some(pos) => {
                        self.channel_name.push_str(&self.pending[..pos]);
                        self.pending.drain(..pos + CHANNEL_CLOSE.len());
                        self.hidden = self.channel_name.to_ascii_lowercase().contains(THOUGHT);
                        self.channel_name.clear();
                        self.in_marker = false;
                    }
                    None => {
                        // Jméno kanálu je krátké; kdyby značka nepřišla,
                        // nedržíme text donekonečna.
                        if self.pending.len() > 64 {
                            self.in_marker = false;
                            let stray = std::mem::take(&mut self.pending);
                            self.channel_name.clear();
                            if !self.hidden {
                                visible.push_str(CHANNEL_OPEN);
                                visible.push_str(&stray);
                            }
                        }
                        break;
                    }
                }
                continue;
            }

            let otevreni = self.pending.find(CHANNEL_OPEN);
            let uzavreni = self.pending.find(CHANNEL_CLOSE);

            // Zavírací značka bez otevírací je zmrzačená hlavička kanálu.
            // Gemma při review posílala `<thought <channel|>` — tedy `thought`
            // bez `<|channel>` před ním — a filtr to pouštěl ven jako text,
            // takže se uživateli v okně objevovalo `<thought <channel|>`
            // před každou odpovědí. Bere se to jako hlavička, protože nic
            // jiného to být nemůže: `<channel|>` se v české ani anglické
            // próze nevyskytuje.
            if let Some(konec) = uzavreni.filter(|k| otevreni.is_none_or(|o| *k < o)) {
                self.zpracuj_zmrzacenou_hlavicku(konec, &mut visible);
                continue;
            }

            match otevreni {
                Some(pos) => {
                    if self.hidden {
                        let dropped = self.pending[..pos].to_string();
                        self.remember_hidden(&dropped);
                    } else {
                        visible.push_str(&self.pending[..pos]);
                    }
                    self.pending.drain(..pos + CHANNEL_OPEN.len());
                    self.in_marker = true;
                }
                None => {
                    // Konec může být začátek značky — ten si necháme.
                    let keep = drzet_na_konci(&self.pending);
                    let split = self.pending.len() - keep;
                    let ready: String = self.pending.drain(..split).collect();
                    if self.hidden {
                        self.remember_hidden(&ready);
                    } else {
                        visible.push_str(&ready);
                    }
                    break;
                }
            }
        }

        visible
    }

    /// Zpracuje `…<jméno <channel|>` — hlavičku, které chybí `<|channel>`.
    ///
    /// Jméno se hledá od poslední `<` před zavírací značkou. Když tam žádná
    /// není (nebo je nesmyslně daleko), zůstane jméno prázdné a kanál se
    /// bere jako viditelný — spolknout tichem kus odpovědi je horší než
    /// nechat proklouznout pár znaků navíc.
    fn zpracuj_zmrzacenou_hlavicku(&mut self, konec: usize, visible: &mut String) {
        /// Nejdelší jméno kanálu, které ještě dává smysl.
        const MAX_JMENO: usize = 64;

        let pred = &self.pending[..konec];
        let zacatek = pred
            .rfind('<')
            .filter(|i| konec - i <= MAX_JMENO && !pred[*i..].contains('\n'))
            .unwrap_or(konec);

        let jmeno = pred[zacatek..].trim_start_matches('<').to_ascii_lowercase();
        let pred_hlavickou = self.pending[..zacatek].to_string();
        if self.hidden {
            self.remember_hidden(&pred_hlavickou);
        } else {
            visible.push_str(&pred_hlavickou);
        }

        self.pending.drain(..konec + CHANNEL_CLOSE.len());
        self.hidden = jmeno.contains(THOUGHT);
    }

    /// Text, který filtr zahodil jako vnitřní uvažování modelu.
    pub fn hidden_text(&self) -> &str {
        &self.hidden_text
    }

    /// Skryté uvažování si držíme jen do rozumné délky — jde o diagnostiku
    /// do logu, ne o druhou kopii výstupu.
    fn remember_hidden(&mut self, text: &str) {
        const LIMIT: usize = 4096;
        if self.hidden_text.len() >= LIMIT || text.is_empty() {
            return;
        }
        let room = LIMIT - self.hidden_text.len();
        let cut = text
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(text.len()))
            .take_while(|i| *i <= room)
            .last()
            .unwrap_or(0);
        self.hidden_text.push_str(&text[..cut]);
    }

    /// Uzavře stream a vrátí, co zbylo. Nedokončená značka se považuje za
    /// obyčejný text — radši viditelná značka než ztracená věta.
    pub fn finish(&mut self) -> String {
        let rest = std::mem::take(&mut self.pending);
        let name = std::mem::take(&mut self.channel_name);
        let was_marker = self.in_marker;
        self.in_marker = false;

        if self.hidden {
            self.remember_hidden(&rest);
            return String::new();
        }
        if was_marker {
            return format!("{CHANNEL_OPEN}{name}{rest}");
        }
        rest
    }
}

/// Kolik bajtů na konci může být začátek značky (`<`, `<|`, `<|c`, …).
///
/// Značky jsou ASCII, ale text kolem nich není: v „odložil" leží uprostřed
/// dvoubajtové `ž`, a krájet řetězec naslepo po bajtech by na něm spadlo.
/// Pozice, které nejsou hranicí znaku, proto přeskakujeme — stejně na nich
/// žádná ASCII značka začínat nemůže.
/// Kolik znaků z konce si ještě nechat.
///
/// Kromě rozečtené značky drží i **jméno kanálu před ní**. Zmrzačená
/// hlavička `<thought <channel|>` se totiž rozdělí mezi dvě dávky tokenů
/// jako `<thought <chan` + `nel|>`, a kdyby se `<thought ` pustilo ven hned,
/// zavírací značka by dorazila pozdě a jméno by uživateli zůstalo v textu.
///
/// Zpátky se sahá jen tehdy, když se **už tak drží rozečtená značka** —
/// běžný text s `<` (třeba `Vec<String>`) se tím nezdržuje.
fn drzet_na_konci(text: &str) -> usize {
    /// Delší jméno kanálu už není jméno kanálu.
    const MAX_JMENO: usize = 32;

    let keep = partial_marker_len(text);
    if keep == 0 {
        return 0;
    }

    let hranice = text.len() - keep;
    let pred = &text[..hranice];
    let Some(zacatek) = pred.rfind('<') else {
        return keep;
    };
    let jmeno = &pred[zacatek + 1..];
    let vypada_jako_jmeno = hranice - zacatek <= MAX_JMENO
        && jmeno
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '_' | '-'));

    if vypada_jako_jmeno {
        text.len() - zacatek
    } else {
        keep
    }
}

fn partial_marker_len(text: &str) -> usize {
    let max = LONGEST_MARKER.min(text.len());
    for len in (1..=max).rev() {
        let start = text.len() - len;
        if !text.is_char_boundary(start) {
            continue;
        }
        // Obě značky: zavírací může přijít i bez otevírací (viz
        // `zpracuj_zmrzacenou_hlavicku`) a rozseknutá mezi dvě dávky
        // tokenů by pak proklouzla ven jako text.
        let konec = &text[start..];
        if CHANNEL_OPEN.starts_with(konec) || CHANNEL_CLOSE.starts_with(konec) {
            return len;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&str]) -> String {
        let mut f = ChannelFilter::new();
        let mut out = String::new();
        for c in chunks {
            out.push_str(&f.push(c));
        }
        out.push_str(&f.finish());
        out
    }

    #[test]
    fn text_without_markers_passes_through() {
        assert_eq!(run(&["Marek ", "odložil ", "klíč."]), "Marek odložil klíč.");
    }

    #[test]
    fn dropped_thinking_is_kept_for_the_log() {
        let mut f = ChannelFilter::new();
        f.push("<|channel>thought<channel|>Rozmyslím si to.<|channel>final<channel|>Text.");
        f.finish();
        assert_eq!(f.hidden_text(), "Rozmyslím si to.");
    }

    #[test]
    fn hidden_capture_does_not_grow_without_bound() {
        let mut f = ChannelFilter::new();
        f.push("<|channel>thought<channel|>");
        for _ in 0..200 {
            f.push(&"á".repeat(100));
        }
        f.finish();
        assert!(f.hidden_text().len() <= 4096, "{}", f.hidden_text().len());
    }

    #[test]
    fn thought_channel_is_dropped() {
        // Přesně tvar z reálného výstupu: jméno kanálu končí novým řádkem.
        let raw = "<|channel>thought\n<channel|>Rozmyslím si scénu.\
                   <|channel>final<channel|>Marek odložil klíč.";
        assert_eq!(run(&[raw]), "Marek odložil klíč.");
    }

    #[test]
    fn marker_split_across_chunks_still_works() {
        // Tokenizer značku běžně rozseká; tohle je ten případ, kvůli kterému
        // filtr existuje jako stavový automat.
        assert_eq!(
            run(&[
                "<|chan",
                "nel>thou",
                "ght<chan",
                "nel|>skryté",
                "<|channel>",
                "final<channel|>vidět"
            ]),
            "vidět"
        );
    }

    #[test]
    fn text_before_first_marker_is_visible() {
        assert_eq!(
            run(&["Začátek. <|channel>thought<channel|>skryté"]),
            "Začátek. "
        );
    }

    /// Přesně tohle posílala Gemma 4 v každém kole review: hlavičku kanálu
    /// bez `<|channel>` na začátku. Filtr ji pouštěl ven, takže se
    /// uživateli nad každou odpovědí objevovalo `<thought <channel|>`.
    #[test]
    fn mangled_channel_header_is_not_shown() {
        assert_eq!(run(&["<thought <channel|>"]), "");
        assert_eq!(
            run(&["<thought <channel|>skryté<|channel>final<channel|>vidět"]),
            "vidět"
        );
    }

    #[test]
    fn text_before_a_mangled_header_survives() {
        assert_eq!(run(&["Začátek. <thought <channel|>skryté"]), "Začátek. ");
    }

    #[test]
    fn mangled_header_of_a_visible_channel_keeps_the_text() {
        // Neznámý kanál je vidět — spolknout kus odpovědi je horší chyba
        // než nechat proklouznout hlavičku.
        assert_eq!(
            run(&["<commentary <channel|>tohle je vidět"]),
            "tohle je vidět"
        );
    }

    #[test]
    fn mangled_header_split_across_chunks_still_works() {
        assert_eq!(run(&["<thought <chan", "nel|>skryté"]), "");
    }

    #[test]
    fn closing_marker_far_from_any_bracket_only_drops_itself() {
        // Bez rozumného jména se kanál bere jako viditelný a text zůstane.
        assert_eq!(
            run(&["obyčejná věta <channel|>a pokračování"]),
            "obyčejná věta a pokračování"
        );
    }

    #[test]
    fn unknown_channel_is_treated_as_visible() {
        assert_eq!(
            run(&["<|channel>commentary<channel|>tohle je vidět"]),
            "tohle je vidět"
        );
    }

    #[test]
    fn unterminated_marker_is_not_swallowed() {
        // Radši viditelná značka než ztracená věta.
        assert_eq!(run(&["text <|channel>ne"]), "text <|channel>ne");
    }

    #[test]
    fn lone_angle_bracket_is_not_held_forever() {
        assert_eq!(run(&["a < b"]), "a < b");
    }

    #[test]
    fn trailing_partial_marker_is_flushed_on_finish() {
        assert_eq!(run(&["hotovo<|"]), "hotovo<|");
    }

    #[test]
    fn very_long_garbage_after_open_marker_is_released() {
        let long = "x".repeat(100);
        let out = run(&[&format!("<|channel>{long}")]);
        assert!(out.contains(&long), "text se nesmí ztratit: {out}");
    }

    #[test]
    fn prompt_puts_system_before_user_turn() {
        let p = build_prompt(Some("Jsi spisovatel."), "Napiš kapitolu.");
        assert_eq!(
            p,
            "<start_of_turn>user\nJsi spisovatel.\n\nNapiš kapitolu.<end_of_turn>\n\
             <start_of_turn>model\n<|channel>final<channel|>"
        );
    }

    #[test]
    fn prompt_without_system_has_only_user_turn() {
        let p = build_prompt(None, "Napiš kapitolu.");
        assert_eq!(
            p,
            "<start_of_turn>user\nNapiš kapitolu.<end_of_turn>\n\
             <start_of_turn>model\n<|channel>final<channel|>"
        );
    }

    #[test]
    fn prompt_opens_the_final_channel() {
        // Bez tohohle model píše kapitolu do kanálu `thought`, filtr ji
        // zahodí a uživateli nepřijde ani slovo. Naměřeno na
        // gemma-4-26B-A4B: 0 viditelných tokenů ze 700.
        assert!(build_prompt(None, "Ahoj").ends_with("<|channel>final<channel|>"));
    }

    #[test]
    fn blank_system_is_ignored() {
        assert_eq!(
            build_prompt(Some("   "), "Ahoj"),
            build_prompt(None, "Ahoj")
        );
    }

    #[test]
    fn architecture_detection() {
        assert!(is_gemma("gemma4"));
        assert!(is_gemma("gemma3"));
        assert!(is_gemma("Gemma2"));
        assert!(!is_gemma("llama"));
        assert!(!is_gemma("qwen3moe"));
    }
}
