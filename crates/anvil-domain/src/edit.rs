//! Návrhy úprav souborů.
//!
//! První schopnost, kterou model může něco **rozbít**. Sandbox z fáze 2 řeší
//! jen to, *kam* se smí sáhnout; tohle řeší, *jestli* to člověk schválil.
//!
//! Tři pravidla, na kterých to stojí:
//!
//! 1. **Nic se nezapíše bez schválení.** Nástroj úpravu jen navrhne. Zápis
//!    dělá až samostatný příkaz, který spustí uživatel.
//! 2. **Úsek k nahrazení musí být jednoznačný.** Když se `old_text` v souboru
//!    vyskytuje dvakrát, není to úprava, je to hádanka — a odmítne se.
//!    Tohle je jediný důvod, proč se dá o výsledku něco tvrdit předem.
//! 3. **Náhled ukazuje, co se opravdu provede.** Počítá se z výsledného
//!    obsahu, ne z toho, co model o své úpravě napsal.

use serde::{Deserialize, Serialize};

use crate::workspace::RelativePath;

/// Co se má se souborem stát.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EditKind {
    /// Nahradí přesný úsek textu jiným.
    Replace { old_text: String, new_text: String },
    /// Založí nový soubor.
    Create { content: String },
}

/// Proč úprava neprošla.
///
/// Hlášky jsou psané **pro model** — je to jediný, kdo je uvidí, a má z nich
/// poznat, co poslat příště. „Neplatný vstup" by ho nechalo hádat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// `old_text` se v souboru nevyskytuje.
    NotFound,
    /// `old_text` se vyskytuje víckrát, takže není jasné které.
    Ambiguous { count: usize },
    /// Soubor, který se má upravit, neexistuje.
    MissingFile,
    /// Soubor, který se má založit, už existuje.
    AlreadyExists,
    /// Úprava by nic nezměnila.
    NoChange,
    /// Prázdný `old_text` — nahradilo by se „nic" a nešlo by poznat kde.
    EmptyTarget,
    /// V jednom plánu se sešlo víc souborů, než kolik jde rozumně projít.
    TooManyFiles { limit: usize },
    /// `old_text` v souboru není, protože ho změnila dřívější navržená úprava.
    NotFoundAfterEdit,
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(
                f,
                "Úsek se v souboru nevyskytuje. Zkopíruj ho přesně tak, jak stojí \
                 v souboru, včetně odsazení — nejdřív si ho přečti."
            ),
            Self::Ambiguous { count } => write!(
                f,
                "Úsek se v souboru vyskytuje {count}×, takže není jasné, který \
                 nahradit. Přidej okolní řádky, ať je jednoznačný."
            ),
            Self::MissingFile => write!(
                f,
                "Soubor neexistuje. Na založení nového použij create_file."
            ),
            Self::AlreadyExists => write!(
                f,
                "Soubor už existuje. Na změnu existujícího použij edit_file."
            ),
            Self::NoChange => write!(f, "Nový text je stejný jako starý — úprava nic nemění."),
            Self::EmptyTarget => write!(
                f,
                "Prázdný old_text nejde nahradit. Uveď úsek, který se má změnit."
            ),
            Self::NotFoundAfterEdit => write!(
                f,
                "Úsek v souboru není — změnila ho tvoje předchozí úprava, která \
                 pořád čeká na schválení. Přečti si soubor znovu; dostaneš znění \
                 po ní, ne to z disku."
            ),
            Self::TooManyFiles { limit } => write!(
                f,
                "Najednou čeká na schválení {limit} souborů, což je strop. \
                 Dodej nejdřív jádro projektu; zbytek přidáme, až tohle uživatel \
                 potvrdí."
            ),
        }
    }
}

impl EditKind {
    /// Spočítá výsledný obsah souboru.
    ///
    /// `current` je `None`, když soubor neexistuje. Nic se nezapisuje — tohle
    /// je čistý výpočet, na kterém stojí náhled i pozdější zápis, takže obojí
    /// z definice ukazuje totéž.
    pub fn apply(&self, current: Option<&str>) -> Result<String, EditError> {
        match self {
            Self::Create { content } => match current {
                Some(_) => Err(EditError::AlreadyExists),
                None => Ok(content.clone()),
            },
            Self::Replace { old_text, new_text } => {
                let Some(current) = current else {
                    return Err(EditError::MissingFile);
                };
                if old_text.is_empty() {
                    return Err(EditError::EmptyTarget);
                }
                if old_text == new_text {
                    return Err(EditError::NoChange);
                }

                let pocet = current.matches(old_text.as_str()).count();
                match pocet {
                    0 => Err(EditError::NotFound),
                    1 => Ok(current.replacen(old_text.as_str(), new_text, 1)),
                    n => Err(EditError::Ambiguous { count: n }),
                }
            }
        }
    }
}

/// Jeden řádek náhledu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffLine {
    /// Beze změny, jen pro orientaci.
    Context {
        line: u32,
        text: String,
    },
    Removed {
        line: u32,
        text: String,
    },
    Added {
        text: String,
    },
}

/// Náhled úpravy: co přesně zmizí a co přibude.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditPreview {
    pub path: RelativePath,
    pub lines: Vec<DiffLine>,
    pub added: u32,
    pub removed: u32,
    /// Soubor se teprve zakládá.
    pub creates_file: bool,
    /// Náhled je zkrácený — u velké úpravy se ukáže jen začátek.
    pub truncated: bool,
}

impl EditPreview {
    /// Kolik řádků se ukáže, než se náhled zkrátí.
    ///
    /// Diff, který se nevejde na obrazovku, uživatel neprojde a odsouhlasí ho
    /// bez čtení — což je horší než žádné potvrzování. Zbytek si zobrazí
    /// v editoru, až se to zapíše.
    pub const MAX_LINES: usize = 200;

    /// Kolik nezměněných řádků se ukáže kolem změny.
    const CONTEXT: usize = 3;

    /// Poskládá náhled ze starého a nového obsahu.
    ///
    /// Nepočítá se obecný diff. Úprava je vždycky nahrazení **jednoho**
    /// souvislého úseku, takže se změněná oblast dá najít porovnáním
    /// společného začátku a konce — a výsledek přesně odpovídá tomu, co se
    /// zapíše, protože oboje vychází z téhož textu.
    pub fn new(path: RelativePath, old: Option<&str>, new: &str) -> Self {
        let Some(old) = old else {
            return Self::for_new_file(path, new);
        };

        let stare: Vec<&str> = old.lines().collect();
        let nove: Vec<&str> = new.lines().collect();

        // Společný začátek a konec: mezi nimi leží změna.
        let shoda_zacatek = stare.iter().zip(&nove).take_while(|(a, b)| a == b).count();
        let zbyva_stare = stare.len() - shoda_zacatek;
        let zbyva_nove = nove.len() - shoda_zacatek;
        let shoda_konec = stare
            .iter()
            .rev()
            .zip(nove.iter().rev())
            .take(zbyva_stare.min(zbyva_nove))
            .take_while(|(a, b)| a == b)
            .count();

        let od = shoda_zacatek.saturating_sub(Self::CONTEXT);
        let odebrane = &stare[shoda_zacatek..stare.len() - shoda_konec];
        let pridane = &nove[shoda_zacatek..nove.len() - shoda_konec];

        let mut lines = Vec::new();
        for (i, text) in stare[od..shoda_zacatek].iter().enumerate() {
            lines.push(DiffLine::Context {
                line: (od + i + 1) as u32,
                text: (*text).to_string(),
            });
        }
        for (i, text) in odebrane.iter().enumerate() {
            lines.push(DiffLine::Removed {
                line: (shoda_zacatek + i + 1) as u32,
                text: (*text).to_string(),
            });
        }
        for text in pridane {
            lines.push(DiffLine::Added {
                text: (*text).to_string(),
            });
        }
        let po = stare.len() - shoda_konec;
        for (i, text) in stare[po..(po + Self::CONTEXT).min(stare.len())]
            .iter()
            .enumerate()
        {
            lines.push(DiffLine::Context {
                line: (po + i + 1) as u32,
                text: (*text).to_string(),
            });
        }

        Self {
            path,
            added: pridane.len() as u32,
            removed: odebrane.len() as u32,
            creates_file: false,
            truncated: false,
            lines,
        }
        .zkratit()
    }

    fn for_new_file(path: RelativePath, content: &str) -> Self {
        let lines: Vec<DiffLine> = content
            .lines()
            .map(|t| DiffLine::Added {
                text: t.to_string(),
            })
            .collect();
        Self {
            path,
            added: lines.len() as u32,
            removed: 0,
            creates_file: true,
            truncated: false,
            lines,
        }
        .zkratit()
    }

    fn zkratit(mut self) -> Self {
        if self.lines.len() > Self::MAX_LINES {
            self.lines.truncate(Self::MAX_LINES);
            self.truncated = true;
        }
        self
    }

    /// Jednořádkový popis do seznamu.
    pub fn headline(&self) -> String {
        if self.creates_file {
            return format!("nový soubor {} ({} řádků)", self.path, self.added);
        }
        format!("{} (+{}, −{})", self.path, self.added, self.removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cesta() -> RelativePath {
        RelativePath::parse("src/main.rs").unwrap()
    }

    fn nahrad(old: &str, new: &str) -> EditKind {
        EditKind::Replace {
            old_text: old.into(),
            new_text: new.into(),
        }
    }

    // --- výpočet obsahu ---

    #[test]
    fn nahrazeni_jednoznacneho_useku_projde() {
        let obsah = "fn main() {\n    let x = f().unwrap();\n}";
        let vysledek = nahrad("f().unwrap()", "f().unwrap_or(0)")
            .apply(Some(obsah))
            .unwrap();

        assert_eq!(vysledek, "fn main() {\n    let x = f().unwrap_or(0);\n}");
    }

    /// Nejdůležitější pravidlo celé fáze: nejednoznačná úprava se neprovede.
    /// Bez toho by se „oprav ten unwrap" trefilo do náhodného z pěti.
    #[test]
    fn vicenasobny_vyskyt_se_odmitne() {
        let obsah = "a.unwrap();\nb.unwrap();\nc.unwrap();";
        let err = nahrad("unwrap()", "unwrap_or_default()")
            .apply(Some(obsah))
            .unwrap_err();

        assert_eq!(err, EditError::Ambiguous { count: 3 });
        // A hláška musí modelu říct, co s tím.
        assert!(err.to_string().contains("okolní řádky"), "{err}");
    }

    #[test]
    fn chybejici_usek_se_odmitne() {
        let err = nahrad("neexistuje", "x").apply(Some("obsah")).unwrap_err();
        assert_eq!(err, EditError::NotFound);
        assert!(err.to_string().contains("odsazení"), "{err}");
    }

    #[test]
    fn uprava_ktera_nic_nemeni_se_odmitne() {
        let err = nahrad("x", "x").apply(Some("x")).unwrap_err();
        assert_eq!(err, EditError::NoChange);
    }

    #[test]
    fn prazdny_usek_se_odmitne() {
        // Prázdný řetězec se v textu „vyskytuje" všude; nahradit by šlo cokoli.
        let err = nahrad("", "novy").apply(Some("obsah")).unwrap_err();
        assert_eq!(err, EditError::EmptyTarget);
    }

    #[test]
    fn uprava_neexistujiciho_souboru_se_odmitne() {
        let err = nahrad("a", "b").apply(None).unwrap_err();
        assert_eq!(err, EditError::MissingFile);
        assert!(err.to_string().contains("create_file"), "{err}");
    }

    #[test]
    fn zalozeni_noveho_souboru_projde() {
        let vysledek = EditKind::Create {
            content: "obsah\n".into(),
        }
        .apply(None)
        .unwrap();
        assert_eq!(vysledek, "obsah\n");
    }

    #[test]
    fn zalozeni_pres_existujici_soubor_se_odmitne() {
        // Přepsat existující soubor omylem je ztráta dat, ne úprava.
        let err = EditKind::Create {
            content: "novy".into(),
        }
        .apply(Some("puvodni obsah"))
        .unwrap_err();

        assert_eq!(err, EditError::AlreadyExists);
        assert!(err.to_string().contains("edit_file"), "{err}");
    }

    #[test]
    fn nahrazeni_pres_vic_radku_projde() {
        let obsah = "a\nb\nc\nd";
        let vysledek = nahrad("b\nc", "B").apply(Some(obsah)).unwrap();
        assert_eq!(vysledek, "a\nB\nd");
    }

    // --- náhled ---

    #[test]
    fn nahled_ukaze_odebrane_i_pridane_radky() {
        let stare = "a\nb\nc";
        let nove = "a\nB\nc";
        let n = EditPreview::new(cesta(), Some(stare), nove);

        assert_eq!(n.removed, 1);
        assert_eq!(n.added, 1);
        assert!(!n.creates_file);
        assert!(n.lines.contains(&DiffLine::Removed {
            line: 2,
            text: "b".into()
        }));
        assert!(n.lines.contains(&DiffLine::Added { text: "B".into() }));
    }

    #[test]
    fn nahled_ma_cisla_radku_z_puvodniho_souboru() {
        let stare = "1\n2\n3\n4\n5\n6\n7\n8";
        let nove = "1\n2\n3\n4\nX\n6\n7\n8";
        let n = EditPreview::new(cesta(), Some(stare), nove);

        let odebrane: Vec<_> = n
            .lines
            .iter()
            .filter_map(|l| match l {
                DiffLine::Removed { line, text } => Some((*line, text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(odebrane, vec![(5, "5")]);
    }

    #[test]
    fn nahled_ukaze_okoli_zmeny() {
        let stare = "1\n2\n3\n4\n5\n6\n7\n8\n9";
        let nove = "1\n2\n3\n4\nX\n6\n7\n8\n9";
        let n = EditPreview::new(cesta(), Some(stare), nove);

        let kontext = n
            .lines
            .iter()
            .filter(|l| matches!(l, DiffLine::Context { .. }))
            .count();
        assert_eq!(kontext, 6, "tři řádky před a tři za změnou");
    }

    #[test]
    fn pridani_radku_nic_neodebere() {
        let n = EditPreview::new(cesta(), Some("a\nc"), "a\nb\nc");
        assert_eq!(n.added, 1);
        assert_eq!(n.removed, 0);
    }

    #[test]
    fn smazani_radku_nic_nepridava() {
        let n = EditPreview::new(cesta(), Some("a\nb\nc"), "a\nc");
        assert_eq!(n.added, 0);
        assert_eq!(n.removed, 1);
    }

    #[test]
    fn novy_soubor_je_cely_pridany() {
        let n = EditPreview::new(cesta(), None, "prvni\ndruhy");
        assert!(n.creates_file);
        assert_eq!(n.added, 2);
        assert_eq!(n.removed, 0);
        assert!(n.headline().contains("nový soubor"), "{}", n.headline());
    }

    #[test]
    fn velky_nahled_se_zkrati_a_prizna_to() {
        // Diff, který se nevejde na obrazovku, uživatel odsouhlasí bez čtení.
        let obsah: String = (0..500).map(|i| format!("radek {i}\n")).collect();
        let n = EditPreview::new(cesta(), None, &obsah);

        assert!(n.truncated);
        assert_eq!(n.lines.len(), EditPreview::MAX_LINES);
        // Počty ale musí zůstat pravdivé, i když se výpis zkrátil.
        assert_eq!(n.added, 500);
    }

    #[test]
    fn nahled_odpovida_tomu_co_se_zapise() {
        // Náhled i zápis vycházejí z jednoho výpočtu — tohle to drží.
        let stare = "fn a() {}\nfn b() {}\nfn c() {}";
        let uprava = nahrad("fn b() {}", "fn b() { todo!() }");
        let nove = uprava.apply(Some(stare)).unwrap();
        let n = EditPreview::new(cesta(), Some(stare), &nove);

        let pridane: Vec<&str> = n
            .lines
            .iter()
            .filter_map(|l| match l {
                DiffLine::Added { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(pridane, vec!["fn b() { todo!() }"]);
    }
}
