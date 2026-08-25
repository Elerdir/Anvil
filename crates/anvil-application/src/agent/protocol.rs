//! Protokol volání nástrojů.
//!
//! **Proč vlastní formát a ne nativní tool-calling.** Anvil podporuje dvě
//! různé rodiny modelů a u Gemmy 4 je šablona z GGUF rozbitá — nativní cesta
//! by se stejně musela obcházet. Vlastní textový formát má navíc tu výhodu,
//! že se dá otestovat bez načteného modelu, a když ho model nedodrží, je
//! z výstupu okamžitě vidět jak.
//!
//! Formát je záměrně co nejjednodušší:
//!
//! ```text
//! <tool>
//! {"name": "read_file", "arguments": {"path": "src/main.rs"}}
//! </tool>
//! ```
//!
//! Parser je tolerantní ke všemu, co model dělá **předvídatelně** a co
//! nemění význam: značky s mezerami navíc, markdownové ohraničení kolem
//! JSONu, víc bloků v jedné odpovědi. Netolerantní je k tomu, co by mohlo
//! vést k provedení něčeho jiného, než co model zamýšlel — takové bloky
//! skončí mezi `malformed` a smyčka je pošle zpátky s vysvětlením.

use anvil_domain::tool::{ToolCall, ToolSpec};

const OPEN: &str = "<tool>";
const CLOSE: &str = "</tool>";

/// Blok, který vypadal jako volání nástroje, ale nešel přečíst.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedCall {
    /// Co v bloku bylo — zkrácené, ať se hláška vejde do promptu.
    pub raw: String,
    /// Proč to neprošlo. Jde zpátky modelu.
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedResponse {
    /// Text mimo bloky nástrojů. To, co uvidí uživatel.
    pub prose: String,
    pub calls: Vec<ToolCall>,
    pub malformed: Vec<MalformedCall>,
}

impl ParsedResponse {
    /// Chce model něco provést?
    pub fn wants_tools(&self) -> bool {
        !self.calls.is_empty() || !self.malformed.is_empty()
    }
}

/// Pokusil se model zavolat nástroj **prózou**, mimo blok `<tool>`?
///
/// Vrací jméno nástroje, o který zjevně šlo. Hledá se `nazev(` nebo `nazev {`
/// — tvar, který model použije, když sklouzne do zápisu volání funkce místo
/// domluveného protokolu. Skutečná Gemma takhle poslala `report_finding(file=…)`
/// a nález se ztratil, protože smyčka odpověď bez bloku bere jako „hotovo".
///
/// Samotná **zmínka** jména nestačí: „nahlásil jsem to přes report_finding"
/// je legitimní věta v shrnutí a nesmí spustit připomínku formátu.
pub fn tool_called_as_prose(prose: &str, names: &[String]) -> Option<String> {
    let text = prose.trim();
    for name in names {
        let mut od = 0;
        while let Some(i) = text[od..].find(name.as_str()) {
            let zacatek = od + i;
            let konec = zacatek + name.len();

            // Jméno musí stát samostatně, ne uvnitř delšího slova.
            let pred_je_slovo = text[..zacatek]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
            let za = text[konec..].trim_start();
            if !pred_je_slovo && (za.starts_with('(') || za.starts_with('{')) {
                return Some(name.clone());
            }
            od = konec;
        }
    }
    None
}

/// Vytáhne z odpovědi volání nástrojů a zbytek nechá jako text.
pub fn parse_response(text: &str) -> ParsedResponse {
    let mut out = ParsedResponse::default();
    let mut prose = String::new();
    let mut rest = text;

    while let Some(start) = rest.find(OPEN) {
        prose.push_str(&rest[..start]);
        let after_open = &rest[start + OPEN.len()..];

        let Some(end) = after_open.find(CLOSE) else {
            // Neuzavřený blok na konci. Model se pravděpodobně uřízl na limitu
            // tokenů — do prózy to nepatří (uživateli by prosákl polotovar),
            // ale ohlásit se to musí, jinak by kolo tiše propadlo.
            out.malformed.push(MalformedCall {
                raw: zkratit(after_open.trim()),
                reason: format!("Blok není uzavřený značkou {CLOSE}."),
            });
            rest = "";
            break;
        };

        zpracuj_blok(&after_open[..end], &mut out);
        rest = &after_open[end + CLOSE.len()..];
    }

    prose.push_str(rest);
    out.prose = prose.trim().to_string();
    out
}

fn zpracuj_blok(telo: &str, out: &mut ParsedResponse) {
    let telo = strip_markdown_fence(telo.trim());

    if telo.is_empty() {
        out.malformed.push(MalformedCall {
            raw: String::new(),
            reason: "Blok nástroje je prázdný.".into(),
        });
        return;
    }

    match ToolCall::parse(telo) {
        Ok(call) if call.name.trim().is_empty() => out.malformed.push(MalformedCall {
            raw: zkratit(telo),
            reason: "Chybí název nástroje v poli \"name\".".into(),
        }),
        Ok(call) => out.calls.push(call),
        Err(e) => out.malformed.push(MalformedCall {
            raw: zkratit(telo),
            reason: format!(
                "{e}. Očekává se jeden JSON objekt: \
                 {{\"name\": \"nazev\", \"arguments\": {{…}}}}."
            ),
        }),
    }
}

/// Modely rády obalí JSON markdownem, i když se o něj nikdo neprosil.
/// Je to předvídatelné a význam to nemění, tak se to prostě sloupne.
fn strip_markdown_fence(s: &str) -> &str {
    let s = s.trim();
    let Some(bez_zacatku) = s.strip_prefix("```") else {
        return s;
    };
    // Za ohraničením bývá název jazyka; zahodí se první řádek.
    let bez_jazyka = match bez_zacatku.find('\n') {
        Some(i) => &bez_zacatku[i + 1..],
        None => bez_zacatku,
    };
    bez_jazyka.trim_end().trim_end_matches("```").trim()
}

/// Zkrátí text do hlášky. Celý blok by v promptu zabral místo, které je
/// při 27 tokenech za sekundu drahé.
fn zkratit(s: &str) -> String {
    const MAX: usize = 160;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let cut: String = s.chars().take(MAX).collect();
    format!("{cut}…")
}

/// Část systémové instrukce, která modelu vysvětlí nástroje a protokol.
pub fn tool_instructions(specs: &[ToolSpec]) -> String {
    let mut out = String::with_capacity(1024);

    out.push_str(
        "Máš k dispozici nástroje. Když něco potřebuješ zjistit, vypiš volání \
         přesně v tomhle tvaru a nic dalšího k němu nepřidávej:\n\n\
         <tool>\n\
         {\"name\": \"nazev_nastroje\", \"arguments\": {\"parametr\": \"hodnota\"}}\n\
         </tool>\n\n\
         Po každém volání dostaneš výsledek a můžeš pokračovat. Volej jeden \
         nástroj po druhém a čti jen to, co skutečně potřebuješ — každý řádek \
         navíc zpomaluje odpověď.\n\n\
         Dostupné nástroje:\n",
    );

    for spec in specs {
        out.push_str(&spec.prompt_line());
        out.push('\n');
    }

    out.push_str(
        "\nKdyž už nic nepotřebuješ, odpověz normálním textem bez bloku \
         <tool>.",
    );
    out
}

#[cfg(test)]
mod tests {
    use anvil_domain::tool::{ParamKind, ToolParam};
    use serde_json::json;

    use super::*;

    // --- volání prózou ---

    fn nastroje() -> Vec<String> {
        vec!["report_finding".to_string(), "read_file".to_string()]
    }

    /// Přesně tohle poslala Gemma při review `src/token.rs`. Chybu našla
    /// i popsala, ale zápis byl volání funkce místo bloku `<tool>`, takže se
    /// nález ztratil.
    #[test]
    fn volani_funkci_se_pozna() {
        let text = r#"report_finding(file="src/token.rs", severity="medium", summary="…")"#;
        assert_eq!(
            tool_called_as_prose(text, &nastroje()),
            Some("report_finding".to_string())
        );
    }

    #[test]
    fn slozene_zavorky_taky() {
        assert_eq!(
            tool_called_as_prose(r#"read_file {"path": "src/main.rs"}"#, &nastroje()),
            Some("read_file".to_string())
        );
    }

    #[test]
    fn pouha_zminka_jmena_neni_volani() {
        // Tohle je legitimní věta v shrnutí a nesmí spustit připomínku formátu.
        assert_eq!(
            tool_called_as_prose("Nálezy jsem nahlásil přes report_finding.", &nastroje()),
            None
        );
        assert_eq!(
            tool_called_as_prose("Použij read_file nebo grep.", &nastroje()),
            None
        );
    }

    #[test]
    fn jmeno_uvnitr_delsiho_slova_se_nepocita() {
        assert_eq!(
            tool_called_as_prose("moje_report_finding(x)", &nastroje()),
            None
        );
    }

    #[test]
    fn spravne_volani_prochazi_parserem_a_ne_touhle_zachranou() {
        // Blok `<tool>` se rozebere normálně; tahle pojistka na něj nesmí
        // sahat, jinak by se každé správné volání hlásilo jako chyba formátu.
        let text = r#"<tool>{"name":"report_finding","arguments":{}}</tool>"#;
        let parsed = parse_response(text);
        assert!(parsed.wants_tools());
        assert_eq!(tool_called_as_prose(&parsed.prose, &nastroje()), None);
    }

    // --- co má projít ---

    #[test]
    fn jednoduche_volani_se_precte() {
        let r = parse_response(
            "<tool>\n{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.rs\"}}\n</tool>",
        );
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].name, "read_file");
        assert_eq!(r.calls[0].arguments["path"], json!("a.rs"));
        assert!(r.malformed.is_empty());
    }

    #[test]
    fn text_kolem_volani_zustane_jako_proza() {
        let r = parse_response(
            "Podívám se do souboru.\n<tool>{\"name\":\"read_file\",\"arguments\":{}}</tool>\nHotovo.",
        );
        assert_eq!(r.prose, "Podívám se do souboru.\n\nHotovo.");
        assert_eq!(r.calls.len(), 1);
    }

    #[test]
    fn vic_volani_v_jedne_odpovedi() {
        let r = parse_response("<tool>{\"name\":\"a\"}</tool> mezi <tool>{\"name\":\"b\"}</tool>");
        assert_eq!(
            r.calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(r.prose, "mezi");
    }

    #[test]
    fn markdownove_ohraniceni_se_sloupne() {
        // Modely to dělají pořád, i když se o to nikdo neprosil.
        let r =
            parse_response("<tool>\n```json\n{\"name\":\"grep\",\"arguments\":{}}\n```\n</tool>");
        assert_eq!(r.calls.len(), 1, "{:?}", r.malformed);
        assert_eq!(r.calls[0].name, "grep");
    }

    #[test]
    fn ohraniceni_bez_nazvu_jazyka() {
        let r = parse_response("<tool>```\n{\"name\":\"grep\"}\n```</tool>");
        assert_eq!(r.calls.len(), 1, "{:?}", r.malformed);
    }

    #[test]
    fn mezery_a_odradkovani_uvnitr_bloku_nevadi() {
        let r = parse_response("<tool>   \n\n  {\"name\":\"a\"}  \n\n </tool>");
        assert_eq!(r.calls.len(), 1);
    }

    #[test]
    fn odpoved_bez_nastroju_je_jen_proza() {
        let r = parse_response("Našel jsem tři problémy, tady jsou.");
        assert!(!r.wants_tools());
        assert_eq!(r.prose, "Našel jsem tři problémy, tady jsou.");
    }

    // --- co musí skončit jako malformed ---

    #[test]
    fn neuzavreny_blok_se_ohlasi() {
        // Typicky když se model uřízne na limitu tokenů.
        let r = parse_response("Podívám se.\n<tool>\n{\"name\":\"read_file\"");
        assert!(r.calls.is_empty());
        assert_eq!(r.malformed.len(), 1);
        assert!(
            r.malformed[0].reason.contains("</tool>"),
            "{:?}",
            r.malformed
        );
    }

    #[test]
    fn neuzavreny_blok_neprosakne_do_prozy() {
        // Uživateli nesmí v odpovědi zůstat půlka JSONu.
        let r = parse_response("Text.\n<tool>{\"name\":\"a\"");
        assert_eq!(r.prose, "Text.");
    }

    #[test]
    fn rozbity_json_se_ohlasi_s_navodem() {
        let r = parse_response("<tool>{\"name\": \"a\",}</tool>");
        assert!(r.calls.is_empty());
        assert_eq!(r.malformed.len(), 1);
        // Hláška musí modelu ukázat správný tvar, ne jen konstatovat chybu.
        assert!(
            r.malformed[0].reason.contains("arguments"),
            "{:?}",
            r.malformed
        );
    }

    #[test]
    fn prazdny_blok_se_ohlasi() {
        let r = parse_response("<tool>\n\n</tool>");
        assert_eq!(r.malformed.len(), 1);
        assert!(
            r.malformed[0].reason.contains("prázdný"),
            "{:?}",
            r.malformed
        );
    }

    #[test]
    fn volani_bez_nazvu_se_ohlasi() {
        let r = parse_response("<tool>{\"name\":\"\",\"arguments\":{}}</tool>");
        assert!(r.calls.is_empty());
        assert!(r.malformed[0].reason.contains("name"), "{:?}", r.malformed);
    }

    #[test]
    fn dlouhy_rozbity_blok_se_v_hlasce_zkrati() {
        // Celý blok by v promptu zabral místo, které je při 27 tok/s drahé.
        let r = parse_response(&format!("<tool>{{ {} </tool>", "x".repeat(2000)));
        assert_eq!(r.malformed.len(), 1);
        assert!(r.malformed[0].raw.chars().count() <= 161);
    }

    #[test]
    fn spatny_blok_nezastavi_zpracovani_dalsiho() {
        let r = parse_response("<tool>rozbité</tool><tool>{\"name\":\"ok\"}</tool>");
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].name, "ok");
        assert_eq!(r.malformed.len(), 1);
    }

    #[test]
    fn wants_tools_plati_i_pro_rozbita_volani() {
        // Kdyby ne, smyčka by rozbité volání tiše zahodila a skončila.
        let r = parse_response("<tool>rozbité</tool>");
        assert!(r.wants_tools());
    }

    // --- instrukce do promptu ---

    fn spec() -> ToolSpec {
        ToolSpec::new(
            "read_file",
            "Přečte soubor.",
            vec![ToolParam::required("path", ParamKind::Text, "Cesta.")],
        )
    }

    #[test]
    fn instrukce_obsahuji_format_i_seznam_nastroju() {
        let text = tool_instructions(&[spec()]);
        assert!(text.contains("<tool>"), "{text}");
        assert!(text.contains("</tool>"), "{text}");
        assert!(text.contains("read_file"), "{text}");
        assert!(text.contains("Přečte soubor."), "{text}");
    }

    #[test]
    fn instrukce_reknou_kdy_prestat() {
        // Bez toho model volá nástroje dokola, i když už má odpověď.
        let text = tool_instructions(&[spec()]);
        assert!(text.contains("Když už nic nepotřebuješ"), "{text}");
    }

    #[test]
    fn priklad_v_instrukcich_projde_vlastnim_parserem() {
        // Kdyby se formát v instrukcích rozešel s parserem, model by dostával
        // návod na něco, co appka nepřečte.
        let text = tool_instructions(&[spec()]);
        let ukazka = text
            .split("<tool>")
            .nth(1)
            .and_then(|s| s.split("</tool>").next())
            .expect("ukázka v instrukcích");

        let r = parse_response(&format!("<tool>{ukazka}</tool>"));
        assert_eq!(r.calls.len(), 1, "{:?}", r.malformed);
        assert_eq!(r.calls[0].name, "nazev_nastroje");
    }
}
