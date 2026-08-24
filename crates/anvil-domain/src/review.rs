//! Nálezy z code review.
//!
//! Nálezy vznikají tak, že si je model **vyžádá nástrojem**, ne že se
//! vytahují regulárem z prózy. Rozdíl je zásadní: strukturovaný nález má
//! soubor, řádek a závažnost jako data, dá se seřadit a proklikat. Text
//! vytažený z odpovědi je jen text, který si na to hraje.

use serde::{Deserialize, Serialize};

use crate::{
    error::{DomainError, DomainResult},
    workspace::RelativePath,
};

/// Jak moc nález pálí.
///
/// Tři stupně schválně. Malý model rozdíl mezi pěti odstíny neudrží a stejně
/// by všechno označoval jako „střední".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Poznámka ke stylu nebo čitelnosti. Nic se nerozbije.
    Note,
    /// Něco je špatně a dřív nebo později se to projeví.
    Warning,
    /// Chyba, která rozbíjí funkčnost nebo bezpečnost.
    Critical,
}

impl Severity {
    /// Pořadí pro výpis: nejzávažnější první.
    pub fn rank(self) -> u8 {
        match self {
            Severity::Critical => 0,
            Severity::Warning => 1,
            Severity::Note => 2,
        }
    }

    pub fn label_cs(self) -> &'static str {
        match self {
            Severity::Critical => "kritické",
            Severity::Warning => "varování",
            Severity::Note => "poznámka",
        }
    }

    /// Rozpozná stupeň z toho, co napsal model. Tolerantní schválně —
    /// odmítnout nález kvůli tomu, že model napsal „high" místo „critical",
    /// by znamenalo zahodit práci, kterou už odvedl.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "critical" | "kriticke" | "kritické" | "high" | "error" | "bug" => {
                Some(Severity::Critical)
            }
            "warning" | "varovani" | "varování" | "medium" | "warn" => Some(Severity::Warning),
            "note" | "poznamka" | "poznámka" | "low" | "info" | "nit" => Some(Severity::Note),
            _ => None,
        }
    }
}

/// Jeden nález.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub file: RelativePath,
    /// Řádek, kterého se nález týká. `None`, když jde o soubor jako celek.
    pub line: Option<u32>,
    pub severity: Severity,
    /// Jedna věta, co je špatně.
    pub summary: String,
    /// Proč to vadí a co s tím. Může být prázdné.
    pub detail: String,
}

impl Finding {
    /// Nejdelší souhrn, který se ještě dá přečíst v seznamu.
    pub const SUMMARY_MAX_CHARS: usize = 200;

    pub fn new(
        file: RelativePath,
        severity: Severity,
        summary: impl Into<String>,
    ) -> DomainResult<Self> {
        let summary = summary.into();
        let trimmed = summary.trim();
        if trimmed.is_empty() {
            return Err(DomainError::validation("nález musí mít popis"));
        }
        Ok(Self {
            file,
            line: None,
            severity,
            summary: zkratit(trimmed, Self::SUMMARY_MAX_CHARS),
            detail: String::new(),
        })
    }

    pub fn with_line(mut self, line: Option<u32>) -> Self {
        // Řádek 0 neexistuje; model ho posílá, když neví. Bereme to jako
        // „netýká se konkrétního řádku".
        self.line = line.filter(|l| *l > 0);
        self
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into().trim().to_string();
        self
    }

    /// Odkaz do editoru ve tvaru `soubor:řádek`.
    pub fn location(&self) -> String {
        match self.line {
            Some(l) => format!("{}:{l}", self.file),
            None => self.file.to_string(),
        }
    }
}

fn zkratit(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max - 1).collect();
    format!("{cut}…")
}

/// Výsledek jednoho běhu review.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewReport {
    pub findings: Vec<Finding>,
    /// Soubory, které model během review otevřel — aby bylo vidět, co viděl
    /// a co ne. Bez toho se nedá odlišit „nic nenašel" od „nedostal se tam".
    pub files_read: Vec<RelativePath>,
    /// Kolik kol smyčka spotřebovala.
    pub rounds: u32,
    /// Smyčka skončila na limitu kol, ne proto, že model dokončil práci.
    pub hit_round_limit: bool,
}

impl ReviewReport {
    /// Nálezy seřazené k výpisu: nejzávažnější první, pak podle souboru
    /// a řádku, ať se dají procházet shora dolů.
    pub fn sorted(&self) -> Vec<&Finding> {
        let mut out: Vec<&Finding> = self.findings.iter().collect();
        out.sort_by(|a, b| {
            a.severity
                .rank()
                .cmp(&b.severity.rank())
                .then(a.file.as_str().cmp(b.file.as_str()))
                .then(a.line.unwrap_or(0).cmp(&b.line.unwrap_or(0)))
        });
        out
    }

    pub fn count(&self, severity: Severity) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == severity)
            .count()
    }

    /// Krátké shrnutí do hlavičky výpisu.
    pub fn headline(&self) -> String {
        if self.findings.is_empty() {
            return "Žádné nálezy.".to_string();
        }
        let mut casti = Vec::new();
        for s in [Severity::Critical, Severity::Warning, Severity::Note] {
            let n = self.count(s);
            if n > 0 {
                casti.push(format!("{n}× {}", s.label_cs()));
            }
        }
        casti.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cesta(s: &str) -> RelativePath {
        RelativePath::parse(s).unwrap()
    }

    fn nalez(soubor: &str, severity: Severity, radek: Option<u32>) -> Finding {
        Finding::new(cesta(soubor), severity, "popis")
            .unwrap()
            .with_line(radek)
    }

    #[test]
    fn nalez_bez_popisu_neprojde() {
        assert!(Finding::new(cesta("a.rs"), Severity::Note, "   ").is_err());
    }

    #[test]
    fn dlouhy_popis_se_zkrati() {
        let f = Finding::new(cesta("a.rs"), Severity::Note, "x".repeat(500)).unwrap();
        assert!(f.summary.chars().count() <= Finding::SUMMARY_MAX_CHARS);
        assert!(f.summary.ends_with('…'));
    }

    #[test]
    fn radek_nula_znamena_bez_radku() {
        // Model posílá 0, když neví — nesmí to vyjít jako odkaz na řádek 0.
        assert_eq!(nalez("a.rs", Severity::Note, Some(0)).line, None);
        assert_eq!(nalez("a.rs", Severity::Note, Some(12)).line, Some(12));
    }

    #[test]
    fn odkaz_obsahuje_radek_jen_kdyz_je() {
        assert_eq!(
            nalez("src/a.rs", Severity::Note, Some(9)).location(),
            "src/a.rs:9"
        );
        assert_eq!(
            nalez("src/a.rs", Severity::Note, None).location(),
            "src/a.rs"
        );
    }

    #[test]
    fn zavaznost_se_pozna_z_ruznych_zapisu() {
        for (vstup, ocekavano) in [
            ("critical", Severity::Critical),
            ("CRITICAL", Severity::Critical),
            ("high", Severity::Critical),
            ("  bug  ", Severity::Critical),
            ("kritické", Severity::Critical),
            ("warning", Severity::Warning),
            ("medium", Severity::Warning),
            ("note", Severity::Note),
            ("low", Severity::Note),
            ("nit", Severity::Note),
        ] {
            assert_eq!(Severity::parse(vstup), Some(ocekavano), "vstup {vstup}");
        }
    }

    #[test]
    fn nesmyslna_zavaznost_se_nepozna() {
        assert_eq!(Severity::parse("kdovíco"), None);
        assert_eq!(Severity::parse(""), None);
    }

    #[test]
    fn nalezy_se_radi_od_nejzavaznejsiho() {
        let r = ReviewReport {
            findings: vec![
                nalez("b.rs", Severity::Note, Some(1)),
                nalez("a.rs", Severity::Critical, Some(5)),
                nalez("a.rs", Severity::Warning, Some(2)),
            ],
            ..Default::default()
        };
        assert_eq!(
            r.sorted().iter().map(|f| f.severity).collect::<Vec<_>>(),
            vec![Severity::Critical, Severity::Warning, Severity::Note]
        );
    }

    #[test]
    fn stejna_zavaznost_se_radi_podle_souboru_a_radku() {
        let r = ReviewReport {
            findings: vec![
                nalez("b.rs", Severity::Warning, Some(1)),
                nalez("a.rs", Severity::Warning, Some(30)),
                nalez("a.rs", Severity::Warning, Some(2)),
            ],
            ..Default::default()
        };
        assert_eq!(
            r.sorted().iter().map(|f| f.location()).collect::<Vec<_>>(),
            vec!["a.rs:2", "a.rs:30", "b.rs:1"]
        );
    }

    #[test]
    fn hlavicka_bez_nalezu() {
        assert_eq!(ReviewReport::default().headline(), "Žádné nálezy.");
    }

    #[test]
    fn hlavicka_scita_podle_zavaznosti() {
        let r = ReviewReport {
            findings: vec![
                nalez("a.rs", Severity::Critical, None),
                nalez("b.rs", Severity::Note, None),
                nalez("c.rs", Severity::Note, None),
            ],
            ..Default::default()
        };
        let h = r.headline();
        assert!(h.contains("1× kritické"), "{h}");
        assert!(h.contains("2× poznámka"), "{h}");
        // Chybějící stupeň se nevypisuje jako „0×".
        assert!(!h.contains("varování"), "{h}");
    }

    #[test]
    fn zprava_hlasi_dosazeni_limitu_kol() {
        // Bez toho by uživatel nepoznal rozdíl mezi „hotovo" a „došla kola".
        let r = ReviewReport {
            hit_round_limit: true,
            rounds: 12,
            ..Default::default()
        };
        assert!(r.hit_round_limit);
        assert_eq!(r.rounds, 12);
    }
}
