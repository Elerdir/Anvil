//! Seznam konverzací a jejich pořadí.
//!
//! Pořadí je **explicitní číslo**, ne odvozené z času poslední zprávy.
//! Uživatel si chce konverzace přerovnat a připnout tak, jak mu to dává
//! smysl; kdyby o pořadí rozhodovala aktivita, každá odpověď by mu seznam
//! zamíchala pod rukama.
//!
//! Přerovnání se posílá jako **celý seznam ID v požadovaném pořadí** a čísla
//! se přepočítají od nuly. U desítek konverzací je to levné a odpadá tím celá
//! třída problémů s vkládáním mezi dvě sousední hodnoty.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{conversation::Conversation, id::ConversationId, model::ModelId};

/// Položka v seznamu konverzací. Nenese zprávy — ty se načtou až při otevření,
/// aby se při startu nemusela do paměti tahat celá historie.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationSummary {
    pub id: ConversationId,
    pub title: String,
    pub pinned: bool,
    /// Menší číslo = výš v seznamu.
    pub sort_order: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    pub message_count: u32,
    #[serde(default)]
    pub model_id: Option<ModelId>,
    /// Konverzace, ze které se tahle odvětvila. Seznam ji potřebuje, aby šlo
    /// u větve ukázat, odkud vznikla — bez toho je v seznamu jen další
    /// položka s podobným názvem.
    #[serde(default)]
    pub parent_id: Option<ConversationId>,
}

/// Seřadí seznam tak, jak se má zobrazit: připnuté nahoře, uvnitř skupin
/// podle zvoleného pořadí.
///
/// Řadí se **stabilně** a s `id` jako poslední rozhodčí, aby dvě položky se
/// stejným `sort_order` (což se po chybě nebo souběžném zápisu stát může)
/// nepřeskakovaly mezi jednotlivými vykresleními.
pub fn sort_for_display(items: &mut [ConversationSummary]) {
    items.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then(a.sort_order.cmp(&b.sort_order))
            .then(a.id.cmp(&b.id))
    });
}

/// Pořadí pro nově založenou konverzaci — nad všechny existující.
///
/// Nová konverzace patří nahoru: uživatel ji právě založil a bude v ní psát.
pub fn order_for_new(existing: &[ConversationSummary]) -> i64 {
    existing
        .iter()
        .map(|c| c.sort_order)
        .min()
        .map(|min| min.saturating_sub(1))
        .unwrap_or(0)
}

/// Přípona, kterou se v názvu pozná odvětvené vlákno.
const VETEV: &str = "větev";

/// Název pro nově odvětvené vlákno.
///
/// Číslovat je potřeba proto, že z jednoho místa se větví opakovaně — bez
/// čísla by v seznamu stálo několik položek se stejným názvem a uživatel by
/// je od sebe nerozeznal. Číslo se hledá podle **skutečně obsazených** názvů,
/// ne podle počtu větví, aby se smazáním jedné z nich neuvolnilo číslo, které
/// už nikdo nečeká.
///
/// Větvení větve nepřidává druhou příponu: z `Downloader (větev 2)` vznikne
/// `Downloader (větev 3)`, ne `Downloader (větev 2) (větev)`.
pub fn branch_title(parent_title: &str, existing: &[String]) -> String {
    let zaklad = strip_branch_suffix(parent_title.trim());
    let zaklad = if zaklad.is_empty() {
        "Konverzace"
    } else {
        zaklad
    };
    // Aby se název i s příponou vešel do stejné meze jako všechny ostatní.
    let zaklad = zkratit(
        zaklad,
        Conversation::TITLE_MAX_CHARS.saturating_sub(" (větev 99)".chars().count()),
    );

    let obsazene: Vec<&str> = existing.iter().map(|t| t.trim()).collect();
    for n in 1..=99 {
        let navrh = if n == 1 {
            format!("{zaklad} ({VETEV})")
        } else {
            format!("{zaklad} ({VETEV} {n})")
        };
        if !obsazene.contains(&navrh.as_str()) {
            return navrh;
        }
    }
    // Sto větví z jednoho místa je hypotéza, ne případ k ošetření — ale
    // vracet prázdný název by bylo horší než připustit duplicitu.
    format!("{zaklad} ({VETEV})")
}

/// Odřízne z názvu `(větev)` nebo `(větev N)` na konci.
fn strip_branch_suffix(title: &str) -> &str {
    let Some(zbytek) = title.strip_suffix(')') else {
        return title;
    };
    let Some(zacatek) = zbytek.rfind('(') else {
        return title;
    };
    let uvnitr = zbytek[zacatek + 1..].trim();
    let je_vetev = uvnitr == VETEV
        || uvnitr
            .strip_prefix(VETEV)
            .is_some_and(|n| n.trim().parse::<u32>().is_ok());
    if je_vetev {
        zbytek[..zacatek].trim_end()
    } else {
        title
    }
}

fn zkratit(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>().trim_end().into()
}

/// Přepočítá `sort_order` podle zadaného pořadí ID.
///
/// Vrací dvojice `(id, nové pořadí)`. ID, která v seznamu nejsou, si své
/// pořadí ponechají — přerovnání jedné skupiny nemá přeházet zbytek.
pub fn apply_order(ids: &[ConversationId]) -> Vec<(ConversationId, i64)> {
    ids.iter()
        .enumerate()
        .map(|(i, id)| (*id, i as i64))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn polozka(poradi: i64, pripnuta: bool) -> ConversationSummary {
        ConversationSummary {
            id: ConversationId::new(),
            title: format!("konverzace {poradi}"),
            pinned: pripnuta,
            sort_order: poradi,
            updated_at: OffsetDateTime::now_utc(),
            message_count: 2,
            model_id: None,
            parent_id: None,
        }
    }

    #[test]
    fn pripnute_jsou_nahore() {
        let mut v = vec![polozka(0, false), polozka(5, true), polozka(1, false)];
        sort_for_display(&mut v);
        assert!(v[0].pinned, "připnutá patří nahoru i s vyšším pořadím");
        assert_eq!(v[1].sort_order, 0);
        assert_eq!(v[2].sort_order, 1);
    }

    #[test]
    fn uvnitr_skupiny_rozhoduje_poradi() {
        let mut v = vec![polozka(2, true), polozka(0, true), polozka(1, true)];
        sort_for_display(&mut v);
        assert_eq!(
            v.iter().map(|c| c.sort_order).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn stejne_poradi_neposkakuje() {
        // Dvě položky se stejným sort_order nesmí měnit vzájemné pořadí mezi
        // vykresleními — jinak seznam „bliká".
        let a = polozka(3, false);
        let b = polozka(3, false);

        let mut prvni = vec![a.clone(), b.clone()];
        let mut druhy = vec![b, a];
        sort_for_display(&mut prvni);
        sort_for_display(&mut druhy);

        assert_eq!(
            prvni.iter().map(|c| c.id).collect::<Vec<_>>(),
            druhy.iter().map(|c| c.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn nova_konverzace_jde_nahoru() {
        let v = vec![polozka(0, false), polozka(3, false)];
        assert!(order_for_new(&v) < 0);
    }

    #[test]
    fn nova_konverzace_v_prazdnem_seznamu() {
        assert_eq!(order_for_new(&[]), 0);
    }

    #[test]
    fn nova_konverzace_nepretece() {
        // Po mnoha založeních se pořadí blíží dolní mezi; nesmí přetéct
        // do kladných čísel a skočit na konec seznamu.
        let mut v = vec![polozka(i64::MIN, false)];
        v[0].sort_order = i64::MIN;
        assert_eq!(order_for_new(&v), i64::MIN);
    }

    #[test]
    fn prerovnani_precisluje_od_nuly() {
        let a = ConversationId::new();
        let b = ConversationId::new();
        let c = ConversationId::new();

        assert_eq!(apply_order(&[c, a, b]), vec![(c, 0), (a, 1), (b, 2)]);
    }

    #[test]
    fn prerovnani_prazdneho_seznamu_nic_nevrati() {
        assert!(apply_order(&[]).is_empty());
    }

    #[test]
    fn prerovnani_a_serazeni_dava_zadany_vysledek() {
        // Kolečko: přerovnám podle ID, přepíšu pořadí, seřadím — musí vyjít
        // přesně to, co uživatel natáhl.
        let mut v = vec![polozka(0, false), polozka(1, false), polozka(2, false)];
        let chtene = vec![v[2].id, v[0].id, v[1].id];

        for (id, poradi) in apply_order(&chtene) {
            v.iter_mut().find(|c| c.id == id).unwrap().sort_order = poradi;
        }
        sort_for_display(&mut v);

        assert_eq!(v.iter().map(|c| c.id).collect::<Vec<_>>(), chtene);
    }

    // --- názvy větví -----------------------------------------------------

    #[test]
    fn prvni_vetev_dostane_priponu_bez_cisla() {
        assert_eq!(branch_title("Downloader", &[]), "Downloader (větev)");
    }

    #[test]
    fn dalsi_vetve_se_cisluji_od_dvojky() {
        let obsazene = vec!["Downloader (větev)".to_string()];
        assert_eq!(
            branch_title("Downloader", &obsazene),
            "Downloader (větev 2)"
        );

        let obsazene = vec![
            "Downloader (větev)".to_string(),
            "Downloader (větev 2)".to_string(),
        ];
        assert_eq!(
            branch_title("Downloader", &obsazene),
            "Downloader (větev 3)"
        );
    }

    #[test]
    fn uvolnene_cislo_se_znovu_pouzije() {
        // Prostřední větev někdo smazal — nová má sednout do díry, ne
        // pokračovat za poslední.
        let obsazene = vec![
            "Downloader (větev)".to_string(),
            "Downloader (větev 3)".to_string(),
        ];
        assert_eq!(
            branch_title("Downloader", &obsazene),
            "Downloader (větev 2)"
        );
    }

    #[test]
    fn vetveni_vetve_nepridava_druhou_priponu() {
        assert_eq!(
            branch_title("Downloader (větev 2)", &[]),
            "Downloader (větev)"
        );
        assert_eq!(
            branch_title("Downloader (větev)", &[]),
            "Downloader (větev)"
        );
    }

    #[test]
    fn zavorka_ktera_neni_vetev_zustane_v_nazvu() {
        assert_eq!(
            branch_title("Oprava CI (podruhé)", &[]),
            "Oprava CI (podruhé) (větev)"
        );
    }

    #[test]
    fn nazev_vetve_se_vejde_do_meze() {
        let dlouhy = "a".repeat(200);
        let nazev = branch_title(&dlouhy, &[]);
        assert!(
            nazev.chars().count() <= Conversation::TITLE_MAX_CHARS,
            "název větve má {} znaků",
            nazev.chars().count()
        );
        assert!(nazev.ends_with("(větev)"));
    }

    #[test]
    fn prazdny_nazev_rodice_neudela_prazdnou_vetev() {
        assert_eq!(branch_title("   ", &[]), "Konverzace (větev)");
    }

    #[test]
    fn pripnuti_nemeni_poradi_uvnitr_skupiny() {
        // Připnutí má položku vytáhnout nahoru, ne přehodit zbytek.
        let mut v = vec![polozka(0, false), polozka(1, false), polozka(2, false)];
        let druha = v[1].id;
        v[1].pinned = true;
        sort_for_display(&mut v);

        assert_eq!(v[0].id, druha);
        assert_eq!(
            v[1..].iter().map(|c| c.sort_order).collect::<Vec<_>>(),
            vec![0, 2],
            "nepřipnuté si mají zachovat vzájemné pořadí"
        );
    }
}
