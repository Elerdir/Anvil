//! Workspace — složka, se kterou model pracuje, a hlídání jejích hranic.
//!
//! Tohle je bezpečnostní jádro celé aplikace. Model dostane nástroje na čtení
//! (a od fáze 4 i na zápis) souborů a **jedinou** zárukou, že si nesáhne mimo
//! vybranou složku, je [`RelativePath::parse`]. Kontrola je záměrně čistě
//! lexikální — nesahá na disk, takže je celá pokrytá jednotkovými testy
//! a nezávisí na tom, co zrovna na disku existuje.
//!
//! Infrastruktura na to navazuje druhou obranou: po sestavení absolutní cesty
//! ji ještě kanonizuje a ověří, že výsledek pořád leží pod rootem. To chytí
//! symlinky, na které lexikální kontrola z principu nedosáhne.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{DomainError, DomainResult};

/// Cesta ověřená jako relativní a nevybočující z workspace.
///
/// Uvnitř je vždy normalizovaný tvar s `/` jako oddělovačem, bez `.` a `..`
/// segmentů. Jinak než přes [`RelativePath::parse`] ji nelze vyrobit, takže
/// kdekoli se v kódu objeví, je už prověřená.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RelativePath(String);

impl RelativePath {
    /// Názvy zařízení, které Windows rozpozná v jakékoli složce a s jakoukoli
    /// příponou. Otevření takového „souboru" by místo I/O na disk sáhlo na
    /// zařízení, proto se odmítají na všech platformách — repozitář s takovým
    /// souborem se stejně nedá na Windows rozbalit.
    const WINDOWS_DEVICE_NAMES: [&'static str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];

    pub fn parse(raw: &str) -> DomainResult<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(DomainError::validation("cesta nesmí být prázdná"));
        }
        if raw.contains('\0') {
            return Err(DomainError::validation("cesta obsahuje nulový bajt"));
        }
        if let Some(c) = raw.chars().find(|c| c.is_control()) {
            return Err(DomainError::validation(format!(
                "cesta obsahuje řídicí znak U+{:04X}",
                c as u32
            )));
        }

        // Absolutní cesty v jakékoli podobě — POSIX kořen, UNC, disk.
        if raw.starts_with('/') || raw.starts_with('\\') {
            return Err(DomainError::validation(format!(
                "cesta musí být relativní vůči workspace: {raw}"
            )));
        }
        let bytes = raw.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' && (bytes[0] as char).is_ascii_alphabetic() {
            return Err(DomainError::validation(format!(
                "cesta s písmenem disku není relativní: {raw}"
            )));
        }

        let mut segments: Vec<&str> = Vec::new();
        for part in raw.split(['/', '\\']) {
            match part {
                // `a//b` i `a/./b` jsou legální zápisy téhož, jen se zahodí.
                "" | "." => continue,
                ".." => {
                    // Únik nad root — tady se láme celý sandbox.
                    if segments.pop().is_none() {
                        return Err(DomainError::validation(format!(
                            "cesta vede mimo workspace: {raw}"
                        )));
                    }
                }
                segment => {
                    Self::validate_segment(segment)?;
                    segments.push(segment);
                }
            }
        }

        if segments.is_empty() {
            // Vzniklo z „.", „./" apod. — samotný root není platný cíl.
            return Err(DomainError::validation(format!(
                "cesta neukazuje na žádný soubor ani složku: {raw}"
            )));
        }

        Ok(Self(segments.join("/")))
    }

    fn validate_segment(segment: &str) -> DomainResult<()> {
        // Koncová tečka nebo mezera: Windows je při otevírání tiše zahodí,
        // takže „soubor.txt " a „soubor.txt" míří na totéž. To by rozbilo
        // jakoukoli úvahu o tom, na co cesta ukazuje.
        if segment.ends_with('.') || segment.ends_with(' ') {
            return Err(DomainError::validation(format!(
                "název '{segment}' nesmí končit tečkou ani mezerou"
            )));
        }

        let stem = segment.split('.').next().unwrap_or(segment);
        if Self::WINDOWS_DEVICE_NAMES
            .iter()
            .any(|d| stem.eq_ignore_ascii_case(d))
        {
            return Err(DomainError::validation(format!(
                "'{segment}' je vyhrazený název zařízení"
            )));
        }

        Ok(())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Jednotlivé části cesty odshora dolů.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// Poslední část — název souboru nebo složky.
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// Přípona bez tečky, malými písmeny. `None`, když soubor příponu nemá.
    pub fn extension(&self) -> Option<String> {
        let name = self.file_name();
        // Tečka na začátku je u `.gitignore` součást názvu, ne přípona.
        let (_, ext) = name.strip_prefix('.').unwrap_or(name).rsplit_once('.')?;
        (!ext.is_empty()).then(|| ext.to_ascii_lowercase())
    }
}

impl std::fmt::Display for RelativePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Složka, se kterou model pracuje.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Root musí být absolutní cesta. Že opravdu existuje a je to složka,
    /// ověřuje až infrastruktura — doména na disk nesahá.
    pub fn new(root: impl Into<PathBuf>) -> DomainResult<Self> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(DomainError::validation(format!(
                "kořen workspace musí být absolutní cesta: {}",
                root.display()
            )));
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Název složky pro zobrazení v UI.
    pub fn name(&self) -> String {
        self.root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.display().to_string())
    }

    /// Absolutní cesta k souboru uvnitř workspace.
    ///
    /// Bere už ověřenou [`RelativePath`], takže tady nemůže dojít k úniku —
    /// typ nese důkaz, že kontrola proběhla.
    pub fn resolve(&self, path: &RelativePath) -> PathBuf {
        let mut out = self.root.clone();
        for segment in path.segments() {
            out.push(segment);
        }
        out
    }

    /// Převede absolutní cestu zpět na relativní vůči workspace.
    /// Chyba, pokud cesta pod root nespadá.
    pub fn relativize(&self, absolute: &Path) -> DomainResult<RelativePath> {
        let rest = absolute.strip_prefix(&self.root).map_err(|_| {
            DomainError::validation(format!(
                "cesta {} neleží ve workspace {}",
                absolute.display(),
                self.root.display()
            ))
        })?;
        RelativePath::parse(&rest.to_string_lossy())
    }
}

/// Vyhoví cesta vzoru?
///
/// Umí `*` (cokoli uvnitř jednoho segmentu), `**` (libovolně mnoho segmentů
/// včetně nuly) a `?` (jeden znak).
///
/// Předchozí verze porovnávala podřetězec a `src/**/*.rs` — nejběžnější
/// zápis pro „všechny zdrojáky" — jí **nevyhověl ani jednou**. Nevrátila
/// chybu, vrátila prázdno, takže se model doslova dozvěděl, že v projektu
/// žádný takový soubor není, a podle toho se zařídil. Filtr, který mlčky
/// nevrací nic, je horší než žádný filtr.
///
/// Porovnává se **bez ohledu na velikost písmen**: jde o vyhledávací filtr,
/// kde je `*.RS` očividně míněné jako `*.rs`, a na Windows navíc velikost
/// písmen v cestě stejně nerozhoduje.
pub fn matches_glob(path: &RelativePath, glob: &str) -> bool {
    let glob = glob.trim().trim_start_matches("./");
    if glob.is_empty() || glob == "*" || glob == "**" {
        return true;
    }

    let cesta = path.as_str().to_lowercase();
    let vzor = glob.to_lowercase();

    let segmenty_cesty: Vec<&str> = cesta.split('/').collect();
    let mut segmenty_vzoru: Vec<&str> = vzor.split('/').collect();

    // Vzor bez lomítka se týká názvu souboru kdekoli ve stromu: kdo napíše
    // `*.rs`, myslí tím celý projekt, ne jen jeho kořen.
    if segmenty_vzoru.len() == 1 {
        segmenty_vzoru.insert(0, "**");
    }

    match_segments(&segmenty_vzoru, &segmenty_cesty)
}

/// Porovnání po segmentech cesty. `**` se zkouší roztáhnout přes všechny
/// délky — vzorů je pár a cesty jsou krátké, takže se to vejde bez ohledu
/// na to, že jde o exponenciální algoritmus v nejhorším případě.
fn match_segments(vzor: &[&str], cesta: &[&str]) -> bool {
    match vzor.split_first() {
        None => cesta.is_empty(),
        Some((&"**", zbytek)) => (0..=cesta.len()).any(|i| match_segments(zbytek, &cesta[i..])),
        Some((prvni, zbytek)) => match cesta.split_first() {
            Some((segment, zbytek_cesty)) if match_segment(prvni, segment) => {
                match_segments(zbytek, zbytek_cesty)
            }
            _ => false,
        },
    }
}

/// Porovnání jednoho segmentu s `*` a `?`.
///
/// Hvězdička se řeší návratem zpět místo rekurze: `a*b*c` se jinak na dlouhém
/// názvu rozjede do stromu volání, který nikam nevede.
fn match_segment(vzor: &str, jmeno: &str) -> bool {
    let v: Vec<char> = vzor.chars().collect();
    let j: Vec<char> = jmeno.chars().collect();
    let (mut vi, mut ji) = (0usize, 0usize);
    // Kam se vrátit, když další znak po hvězdičce nesedne.
    let mut hvezdicka: Option<usize> = None;
    let mut navrat = 0usize;

    while ji < j.len() {
        if vi < v.len() && (v[vi] == '?' || v[vi] == j[ji]) {
            vi += 1;
            ji += 1;
        } else if vi < v.len() && v[vi] == '*' {
            hvezdicka = Some(vi);
            navrat = ji;
            vi += 1;
        } else if let Some(h) = hvezdicka {
            vi = h + 1;
            navrat += 1;
            ji = navrat;
        } else {
            return false;
        }
    }

    // Zbylé hvězdičky můžou spolknout prázdno, cokoli jiného ne.
    v[vi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- vzory ---

    fn sedne(vzor: &str, p: &str) -> bool {
        matches_glob(&RelativePath::parse(p).unwrap(), vzor)
    }

    /// Přesně tenhle vzor poslal skutečný model při review a starý filtr na
    /// něj nevrátil jediný soubor. Model z toho vyvodil, že v projektu žádné
    /// zdrojáky nejsou, a čtyři kola hledal jinde.
    #[test]
    fn hvezdicky_pres_slozky_najdou_zdrojaky() {
        assert!(sedne("src/**/*.rs", "src/main.rs"));
        assert!(sedne("src/**/*.rs", "src/ai/gemma.rs"));
        assert!(sedne("src/**/*.rs", "src/a/b/c/hluboko.rs"));
        assert!(!sedne("src/**/*.rs", "tests/main.rs"));
        assert!(!sedne("src/**/*.rs", "src/data.json"));
    }

    #[test]
    fn vzor_bez_lomitka_plati_v_celem_stromu() {
        // Kdo napíše `*.rs`, myslí celý projekt, ne jen jeho kořen.
        assert!(sedne("*.rs", "src/hluboko/modul.rs"));
        assert!(sedne("Cargo.toml", "crates/domain/Cargo.toml"));
        assert!(!sedne("*.rs", "src/styl.css"));
    }

    #[test]
    fn hvezdicka_neprekroci_lomitko() {
        assert!(sedne("src/*.rs", "src/main.rs"));
        assert!(
            !sedne("src/*.rs", "src/ai/gemma.rs"),
            "jedna hvězdička je jeden segment"
        );
    }

    #[test]
    fn dve_hvezdicky_spolknou_i_nula_slozek() {
        assert!(sedne("src/**/main.rs", "src/main.rs"));
        assert!(sedne("**/main.rs", "main.rs"));
    }

    #[test]
    fn otaznik_je_prave_jeden_znak() {
        assert!(sedne("src/mai?.rs", "src/main.rs"));
        assert!(!sedne("src/mai?.rs", "src/main2.rs"));
    }

    #[test]
    fn velikost_pismen_nerozhoduje() {
        assert!(sedne("*.RS", "src/main.rs"));
        assert!(sedne("SRC/**", "src/main.rs"));
    }

    #[test]
    fn prazdny_vzor_propusti_vsechno() {
        assert!(sedne("", "cokoli/jineho.txt"));
        assert!(sedne("*", "cokoli/jineho.txt"));
        assert!(sedne("**", "cokoli/jineho.txt"));
    }

    #[test]
    fn vice_hvezdicek_v_jednom_segmentu() {
        assert!(sedne("*test*.rs", "src/muj_test_modul.rs"));
        assert!(!sedne("*test*.rs", "src/modul.rs"));
    }

    fn ws() -> Workspace {
        // Absolutní tvar se liší podle platformy, ale test má běžet na obou.
        let root = if cfg!(windows) {
            PathBuf::from(r"E:\Projects\Anvil")
        } else {
            PathBuf::from("/home/dev/anvil")
        };
        Workspace::new(root).unwrap()
    }

    // --- co má projít ---

    #[test]
    fn bezna_cesta_projde() {
        assert_eq!(
            RelativePath::parse("src/main.rs").unwrap().as_str(),
            "src/main.rs"
        );
    }

    #[test]
    fn zpetne_lomitko_se_normalizuje() {
        assert_eq!(
            RelativePath::parse(r"src\ai\mod.rs").unwrap().as_str(),
            "src/ai/mod.rs"
        );
    }

    #[test]
    fn tecka_a_dvojite_lomitko_se_zahodi() {
        assert_eq!(
            RelativePath::parse("./src//./lib.rs").unwrap().as_str(),
            "src/lib.rs"
        );
    }

    #[test]
    fn dvojtecka_uvnitr_se_vyresi_a_neni_chyba() {
        // `..`, které nevybočí z workspace, je legitimní zápis.
        assert_eq!(
            RelativePath::parse("src/ai/../lib.rs").unwrap().as_str(),
            "src/lib.rs"
        );
    }

    #[test]
    fn skryty_soubor_projde() {
        assert_eq!(
            RelativePath::parse(".gitignore").unwrap().as_str(),
            ".gitignore"
        );
    }

    // --- co musí spadnout ---

    #[test]
    fn dvojtecka_ven_z_workspace_neprojde() {
        for utok in [
            "../tajne.txt",
            "../../etc/passwd",
            "src/../../mimo.rs",
            r"..\..\Windows\System32\config\SAM",
            "a/b/../../../ven",
        ] {
            let err = RelativePath::parse(utok).unwrap_err().to_string();
            assert!(
                err.contains("mimo workspace"),
                "{utok} mělo být odmítnuto jako únik, ale hláška byla: {err}"
            );
        }
    }

    #[test]
    fn absolutni_cesty_neprojdou() {
        for utok in [
            "/etc/passwd",
            r"\Windows\System32",
            r"C:\Windows\System32",
            "c:/Windows",
            r"\\server\share\soubor",
        ] {
            assert!(
                RelativePath::parse(utok).is_err(),
                "{utok} je absolutní a mělo být odmítnuto"
            );
        }
    }

    #[test]
    fn prazdna_cesta_a_samotny_root_neprojdou() {
        assert!(RelativePath::parse("").is_err());
        assert!(RelativePath::parse("   ").is_err());
        assert!(RelativePath::parse(".").is_err());
        assert!(RelativePath::parse("./").is_err());
    }

    #[test]
    fn ridici_znaky_a_nulovy_bajt_neprojdou() {
        assert!(RelativePath::parse("src/ma\0in.rs").is_err());
        assert!(RelativePath::parse("src/ma\nin.rs").is_err());
        assert!(RelativePath::parse("src/ma\tin.rs").is_err());
    }

    #[test]
    fn vyhrazene_nazvy_zarizeni_neprojdou() {
        for utok in ["CON", "nul", "src/COM1", "aux.txt", "src/LPT9.log"] {
            assert!(
                RelativePath::parse(utok).is_err(),
                "{utok} je název zařízení a měl být odmítnut"
            );
        }
        // Podobně vypadající, ale legitimní názvy projít musí.
        assert!(RelativePath::parse("console.rs").is_ok());
        assert!(RelativePath::parse("src/nullable.ts").is_ok());
        assert!(RelativePath::parse("common.rs").is_ok());
    }

    #[test]
    fn segment_koncici_teckou_nebo_mezerou_neprojde() {
        // Windows je při otevírání tiše zahodí, takže dvě různě zapsané cesty
        // by mířily na jeden soubor.
        assert!(RelativePath::parse("src/soubor.txt /dalsi.rs").is_err());
        assert!(RelativePath::parse("src/slozka./soubor.rs").is_err());
        assert!(RelativePath::parse("src/soubor.rs.").is_err());
    }

    #[test]
    fn obalujici_mezery_cele_cesty_se_jen_orizmou() {
        // Na rozdíl od mezery uvnitř cesty jde o překlep ve vstupu, ne o pokus
        // trefit jiný soubor — trimuje se, aby uživatel nemusel řešit whitespace.
        assert_eq!(
            RelativePath::parse("  src/main.rs  ").unwrap().as_str(),
            "src/main.rs"
        );
    }

    // --- Workspace ---

    #[test]
    fn relativni_root_neprojde() {
        assert!(Workspace::new("relativni/cesta").is_err());
    }

    #[test]
    fn resolve_slozi_cestu_pod_rootem() {
        let w = ws();
        let p = w.resolve(&RelativePath::parse("src/main.rs").unwrap());
        assert!(p.starts_with(w.root()));
        assert!(p.ends_with("main.rs"));
    }

    #[test]
    fn resolve_nikdy_nevyjde_z_rootu() {
        // Pojistka proti tomu, aby někdo v budoucnu obešel parse:
        // co projde přes RelativePath, nesmí z rootu vylézt.
        let w = ws();
        for vstup in ["src/../lib.rs", "a/b/../c", "./src/./ai/mod.rs"] {
            let p = w.resolve(&RelativePath::parse(vstup).unwrap());
            assert!(
                p.starts_with(w.root()),
                "{vstup} → {} vylezlo z workspace",
                p.display()
            );
            assert!(
                !p.components().any(|c| c.as_os_str() == ".."),
                "{vstup} → {} nechalo v cestě '..'",
                p.display()
            );
        }
    }

    #[test]
    fn relativize_vrati_cestu_zpatky() {
        let w = ws();
        let rel = RelativePath::parse("src/ai/mod.rs").unwrap();
        let abs = w.resolve(&rel);
        assert_eq!(w.relativize(&abs).unwrap(), rel);
    }

    #[test]
    fn relativize_odmitne_cestu_mimo() {
        let w = ws();
        let mimo = if cfg!(windows) {
            PathBuf::from(r"E:\Projects\Erato\src\main.rs")
        } else {
            PathBuf::from("/home/dev/erato/src/main.rs")
        };
        assert!(w.relativize(&mimo).is_err());
    }

    #[test]
    fn nazev_workspace_je_posledni_slozka() {
        let ocekavano = if cfg!(windows) { "Anvil" } else { "anvil" };
        assert_eq!(ws().name(), ocekavano);
    }

    #[test]
    fn pripona_se_pozna() {
        let p = |s| RelativePath::parse(s).unwrap();
        assert_eq!(p("src/main.rs").extension().as_deref(), Some("rs"));
        assert_eq!(p("src/App.TSX").extension().as_deref(), Some("tsx"));
        assert_eq!(p("Makefile").extension(), None);
        // Tečka na začátku je součást názvu, ne přípona.
        assert_eq!(p(".gitignore").extension(), None);
        assert_eq!(p(".eslintrc.json").extension().as_deref(), Some("json"));
    }

    #[test]
    fn nazev_souboru_je_posledni_segment() {
        assert_eq!(RelativePath::parse("a/b/c.rs").unwrap().file_name(), "c.rs");
        assert_eq!(RelativePath::parse("c.rs").unwrap().file_name(), "c.rs");
    }
}
