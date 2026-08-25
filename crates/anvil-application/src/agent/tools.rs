//! Nástroje, které si model může vyžádat.
//!
//! Čtecí nástroje sahají na disk rovnou. Zapisovat neumí **žádný**:
//! `edit_file` a `create_file` úpravu jen spočítají a odloží do
//! [`EditPlan`](crate::edits::EditPlan). Na disk se dostane až příkazem,
//! který spustí uživatel poté, co viděl náhled — takže ať se model splete
//! jakkoli, sám nic nepřepíše.
//!
//! Společné pravidlo výstupů: **stručnost je rychlost.** Prompt se zpracovává
//! ~27 tokenů za sekundu, takže každý řádek navíc je čas, který uživatel
//! čeká. Nástroje proto vracejí ořezané výsledky a **říkají, že ořezaly** —
//! model si další kus vyžádá sám, když ho potřebuje.

use std::sync::{Arc, Mutex};

use anvil_domain::{
    edit::EditKind,
    ports::WorkspaceFs,
    review::{Finding, Severity},
    tool::{ParamKind, ToolParam, ToolResult, ToolSpec},
    workspace::RelativePath,
};
use async_trait::async_trait;
use serde_json::Value;

use crate::edits::EditPlan;

/// Co po běhu smyčky zbyde vedle textu odpovědi.
#[derive(Debug, Default)]
pub struct RunArtifacts {
    pub findings: Vec<Finding>,
    pub files_read: Vec<RelativePath>,
}

/// Sdílený stav běhu. Nástroje do něj zapisují, smyčka ho na konci vybere.
pub type SharedArtifacts = Arc<Mutex<RunArtifacts>>;

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    /// Argumenty jsou už ověřené a normalizované podle [`Tool::spec`], takže
    /// tady se na jejich tvar dá spolehnout.
    async fn call(&self, args: &Value) -> ToolResult;
}

fn text_arg<'a>(args: &'a Value, name: &str) -> Option<&'a str> {
    args.get(name).and_then(Value::as_str)
}

fn int_arg(args: &Value, name: &str) -> Option<u32> {
    args.get(name)
        .and_then(Value::as_i64)
        .filter(|v| *v > 0)
        .map(|v| v as u32)
}

// --- list_files -----------------------------------------------------------

pub struct ListFiles {
    fs: Arc<dyn WorkspaceFs>,
}

impl ListFiles {
    pub fn new(fs: Arc<dyn WorkspaceFs>) -> Self {
        Self { fs }
    }
}

#[async_trait]
impl Tool for ListFiles {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "list_files",
            "Vypíše soubory v projektu. Volitelně filtruje vzorem, např. *.rs nebo src/**.",
            vec![ToolParam::optional(
                "glob",
                ParamKind::Text,
                "Vzor pro filtrování, např. *.rs",
            )],
        )
    }

    async fn call(&self, args: &Value) -> ToolResult {
        let glob = text_arg(args, "glob");
        match self.fs.list(glob).await {
            // Holé „nic“ model přečte jako „projekt je prázdný“ a zařídí se
            // podle toho. Když je prázdný jen výsledek filtru, musí to být
            // ze zprávy poznat — jinak si model odnese nepravdu, kterou už
            // nemá jak vyvrátit.
            Ok(soubory) if soubory.is_empty() => match glob {
                Some(g) => {
                    let celkem = self.fs.list(None).await.map(|v| v.len()).unwrap_or(0);
                    ToolResult::ok(format!(
                        "Vzoru „{g}“ neodpovídá žádný soubor. Projekt jich má {celkem} — \
                         zkus jiný vzor, nebo list_files bez vzoru."
                    ))
                }
                None => ToolResult::ok("Projekt neobsahuje žádný čitelný soubor."),
            },
            Ok(soubory) => {
                let limit = self.fs.limits().max_listed_files as usize;
                let orezano = soubory.len() > limit;
                let vypis: Vec<&str> = soubory
                    .iter()
                    .take(limit)
                    .map(RelativePath::as_str)
                    .collect();

                let mut out = vypis.join("\n");
                if orezano {
                    out.push_str(&format!(
                        "\n\n… a dalších {}. Zúži vzorem, když potřebuješ víc.",
                        soubory.len() - limit
                    ));
                }
                ToolResult::ok(out)
            }
            Err(e) => ToolResult::error(e.to_string()),
        }
    }
}

// --- read_file ------------------------------------------------------------

pub struct ReadFile {
    fs: Arc<dyn WorkspaceFs>,
    artifacts: SharedArtifacts,
}

impl ReadFile {
    pub fn new(fs: Arc<dyn WorkspaceFs>, artifacts: SharedArtifacts) -> Self {
        Self { fs, artifacts }
    }
}

#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        let max = self.fs.limits().max_lines_per_read;
        ToolSpec::new(
            "read_file",
            &format!(
                "Přečte soubor z projektu. Vrací nejvýš {max} řádků; delší soubor \
                 si vyžádej po částech přes start_line."
            ),
            vec![
                ToolParam::required(
                    "path",
                    ParamKind::Text,
                    "Cesta relativně ke složce projektu.",
                ),
                ToolParam::optional(
                    "start_line",
                    ParamKind::Integer,
                    "Od kterého řádku číst (první je 1).",
                ),
                ToolParam::optional("line_count", ParamKind::Integer, "Kolik řádků vrátit."),
            ],
        )
    }

    async fn call(&self, args: &Value) -> ToolResult {
        let Some(raw) = text_arg(args, "path") else {
            return ToolResult::error("Chybí cesta k souboru.");
        };

        let path = match RelativePath::parse(raw) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e.to_string()),
        };

        match self
            .fs
            .read(
                &path,
                int_arg(args, "start_line"),
                int_arg(args, "line_count"),
            )
            .await
        {
            Ok(slice) => {
                {
                    let mut a = self.artifacts.lock().expect("zámek");
                    if !a.files_read.contains(&slice.path) {
                        a.files_read.push(slice.path.clone());
                    }
                }

                // Čísla řádků jsou nutná: bez nich model nemá jak nález
                // umístit a hádal by je.
                let mut out = String::new();
                for (i, radek) in slice.text.lines().enumerate() {
                    out.push_str(&format!("{:>5} | {radek}\n", slice.start_line + i as u32));
                }
                if slice.truncated() {
                    out.push_str(&format!(
                        "\n… soubor má {} řádků, tohle byly {}–{}.\n",
                        slice.total_lines,
                        slice.start_line,
                        slice.end_line()
                    ));
                }
                ToolResult::ok(out)
            }
            Err(e) => ToolResult::error(e.to_string()),
        }
    }
}

// --- grep -----------------------------------------------------------------

pub struct Grep {
    fs: Arc<dyn WorkspaceFs>,
}

impl Grep {
    pub fn new(fs: Arc<dyn WorkspaceFs>) -> Self {
        Self { fs }
    }
}

#[async_trait]
impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "grep",
            "Najde text v souborech projektu. Rychlejší než číst soubory po jednom.",
            vec![
                ToolParam::required("pattern", ParamKind::Text, "Co hledat."),
                ToolParam::optional("glob", ParamKind::Text, "Kde hledat, např. *.rs"),
            ],
        )
    }

    async fn call(&self, args: &Value) -> ToolResult {
        let Some(pattern) = text_arg(args, "pattern") else {
            return ToolResult::error("Chybí hledaný vzor.");
        };

        let glob = text_arg(args, "glob");
        match self.fs.grep(pattern, glob).await {
            // „Nikde se nevyskytuje“ je tvrzení o celém projektu. Když se
            // přitom hledalo jen ve výseku — nebo v ničem, protože vzor
            // nesedl — je to lež, kterou model vezme za svou.
            Ok(hits) if hits.is_empty() => {
                let prohledano = self.fs.list(glob).await.map(|v| v.len()).unwrap_or(0);
                ToolResult::ok(match (glob, prohledano) {
                    (Some(g), 0) => format!(
                        "Vzoru „{g}“ neodpovídá žádný soubor, takže se „{pattern}“ \
                         nehledalo nikde. Zkus jiný vzor."
                    ),
                    (Some(g), n) => format!(
                        "„{pattern}“ se nevyskytuje v žádném z {n} souborů, \
                         které odpovídají vzoru „{g}“."
                    ),
                    (None, n) => {
                        format!("„{pattern}“ se nevyskytuje v žádném z {n} souborů projektu.")
                    }
                })
            }
            Ok(hits) => {
                let limit = self.fs.limits().max_grep_hits as usize;
                let orezano = hits.len() > limit;
                let mut out: String = hits
                    .iter()
                    .take(limit)
                    .map(|h| format!("{}:{}: {}\n", h.file, h.line, h.text))
                    .collect();
                if orezano {
                    out.push_str(&format!(
                        "\n… a dalších {} zásahů. Zpřesni vzor.",
                        hits.len() - limit
                    ));
                }
                ToolResult::ok(out)
            }
            Err(e) => ToolResult::error(e.to_string()),
        }
    }
}

// --- edit_file / create_file ----------------------------------------------

/// Sdílený plán úprav. Nástroje do něj přidávají návrhy, zapisuje až uživatel.
pub type SharedPlan = Arc<tokio::sync::Mutex<EditPlan>>;

/// Společný základ obou nástrojů pro zápis.
///
/// **Nic nezapisuje.** Úprava se spočítá, ověří a odloží; na disk se dostane
/// až příkazem, který spustí uživatel poté, co viděl náhled.
struct Navrh {
    fs: Arc<dyn WorkspaceFs>,
    plan: SharedPlan,
}

impl Navrh {
    async fn pridej(&self, path: RelativePath, kind: EditKind) -> ToolResult {
        let mut plan = self.plan.lock().await;
        match plan.propose(&self.fs, path, kind).await {
            Ok(nahled) => ToolResult::ok(format!(
                "Návrh připraven: {}. Čeká na schválení uživatelem — zapsaný \
                 zatím není, tak s tím ve zbytku odpovědi nepočítej jako s hotovým.",
                nahled.headline()
            )),
            Err(e) => ToolResult::error(e.to_string()),
        }
    }
}

pub struct EditFile(Navrh);

impl EditFile {
    pub fn new(fs: Arc<dyn WorkspaceFs>, plan: SharedPlan) -> Self {
        Self(Navrh { fs, plan })
    }
}

#[async_trait]
impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "edit_file",
            "Navrhne úpravu souboru: nahradí přesný úsek textu jiným. \
             Nezapisuje — návrh schvaluje uživatel.",
            vec![
                ToolParam::required("path", ParamKind::Text, "Cesta ke souboru."),
                ToolParam::required(
                    "old_text",
                    ParamKind::Text,
                    "Přesné znění úseku, který se má nahradit, včetně odsazení. \
                     Musí být v souboru právě jednou — když ne, přidej okolní řádky.",
                ),
                ToolParam::required("new_text", ParamKind::Text, "Čím se úsek nahradí."),
            ],
        )
    }

    async fn call(&self, args: &Value) -> ToolResult {
        let (Some(raw), Some(old_text), Some(new_text)) = (
            text_arg(args, "path"),
            text_arg(args, "old_text"),
            text_arg(args, "new_text"),
        ) else {
            return ToolResult::error("Úprava potřebuje path, old_text i new_text.");
        };

        let path = match RelativePath::parse(raw) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        self.0
            .pridej(
                path,
                EditKind::Replace {
                    old_text: old_text.to_string(),
                    new_text: new_text.to_string(),
                },
            )
            .await
    }
}

pub struct CreateFile(Navrh);

impl CreateFile {
    pub fn new(fs: Arc<dyn WorkspaceFs>, plan: SharedPlan) -> Self {
        Self(Navrh { fs, plan })
    }
}

#[async_trait]
impl Tool for CreateFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "create_file",
            "Navrhne založení nového souboru. Existující soubor nepřepíše — \
             na to je edit_file. Nezapisuje, návrh schvaluje uživatel.",
            vec![
                ToolParam::required("path", ParamKind::Text, "Cesta nového souboru."),
                ToolParam::required("content", ParamKind::Text, "Celý obsah souboru."),
            ],
        )
    }

    async fn call(&self, args: &Value) -> ToolResult {
        let (Some(raw), Some(content)) = (text_arg(args, "path"), text_arg(args, "content")) else {
            return ToolResult::error("Založení souboru potřebuje path i content.");
        };

        let path = match RelativePath::parse(raw) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        self.0
            .pridej(
                path,
                EditKind::Create {
                    content: content.to_string(),
                },
            )
            .await
    }
}

// --- report_finding -------------------------------------------------------

pub struct ReportFinding {
    artifacts: SharedArtifacts,
}

impl ReportFinding {
    pub fn new(artifacts: SharedArtifacts) -> Self {
        Self { artifacts }
    }
}

#[async_trait]
impl Tool for ReportFinding {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "report_finding",
            "Nahlásí jeden nález. Použij pro každý problém zvlášť, ať se dá proklikat.",
            vec![
                ToolParam::required("file", ParamKind::Text, "Kterého souboru se týká."),
                ToolParam::required("severity", ParamKind::Text, "critical, warning nebo note."),
                ToolParam::required("summary", ParamKind::Text, "Jednou větou, co je špatně."),
                ToolParam::optional("line", ParamKind::Integer, "Číslo řádku."),
                ToolParam::optional("detail", ParamKind::Text, "Proč to vadí a co s tím."),
            ],
        )
    }

    async fn call(&self, args: &Value) -> ToolResult {
        let (Some(file), Some(severity), Some(summary)) = (
            text_arg(args, "file"),
            text_arg(args, "severity"),
            text_arg(args, "summary"),
        ) else {
            return ToolResult::error("Nález potřebuje file, severity i summary.");
        };

        let path = match RelativePath::parse(file) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e.to_string()),
        };

        let Some(severity) = Severity::parse(severity) else {
            return ToolResult::error("Neznámá závažnost. Použij critical, warning nebo note.");
        };

        let nalez = match Finding::new(path, severity, summary) {
            Ok(f) => f
                .with_line(int_arg(args, "line"))
                .with_detail(text_arg(args, "detail").unwrap_or_default()),
            Err(e) => return ToolResult::error(e.to_string()),
        };

        let misto = nalez.location();
        self.artifacts.lock().expect("zámek").findings.push(nalez);

        // Krátké potvrzení schválně: model nepotřebuje echo celého nálezu,
        // jen vědět, že se to zapsalo.
        ToolResult::ok(format!("Nález zapsán ({misto})."))
    }
}

// --- sada -----------------------------------------------------------------

/// Sada nástrojů pro jeden běh review.
pub struct Toolbox {
    tools: Vec<Arc<dyn Tool>>,
    artifacts: SharedArtifacts,
}

impl Toolbox {
    /// Nástroje pro code review — všechny jen pro čtení.
    pub fn for_review(fs: Arc<dyn WorkspaceFs>) -> Self {
        let artifacts: SharedArtifacts = Arc::new(Mutex::new(RunArtifacts::default()));
        Self {
            tools: vec![
                Arc::new(ListFiles::new(fs.clone())),
                Arc::new(Grep::new(fs.clone())),
                Arc::new(ReadFile::new(fs, artifacts.clone())),
                Arc::new(ReportFinding::new(artifacts.clone())),
            ],
            artifacts,
        }
    }

    /// Nástroje pro práci s projektem včetně **návrhu úprav**.
    ///
    /// Zápis tu je, ale jen jako návrh — viz [`EditFile`]. Čtecí nástroje
    /// zůstávají, protože bez přečtení souboru model nemá odkud vzít přesné
    /// znění úseku k nahrazení.
    pub fn for_editing(fs: Arc<dyn WorkspaceFs>, plan: SharedPlan) -> Self {
        let artifacts: SharedArtifacts = Arc::new(Mutex::new(RunArtifacts::default()));
        Self {
            tools: vec![
                Arc::new(ListFiles::new(fs.clone())),
                Arc::new(Grep::new(fs.clone())),
                Arc::new(ReadFile::new(fs.clone(), artifacts.clone())),
                Arc::new(EditFile::new(fs.clone(), plan.clone())),
                Arc::new(CreateFile::new(fs, plan)),
            ],
            artifacts,
        }
    }

    /// Nástroje pro průchod **jedním souborem**.
    ///
    /// Obsah souboru dostane model rovnou v zadání, takže si ho nemá čím
    /// vyžádat — a hlavně se nemá kam zatoulat. Výběr souborů dělá
    /// [`ReviewService`], ne model; měřením vyšlo najevo, že dva běhy nad
    /// stejným kódem otevřou úplně jiné soubory a jeden z nich minul jedinou
    /// bezpečnostní díru v projektu.
    ///
    /// [`ReviewService`]: crate::review::ReviewService
    pub fn for_single_file(artifacts: SharedArtifacts) -> Self {
        Self {
            tools: vec![Arc::new(ReportFinding::new(artifacts.clone()))],
            artifacts,
        }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    pub fn find(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.spec().name == name).cloned()
    }

    /// Názvy nástrojů — do hlášky, když si model nějaký vymyslí.
    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.spec().name).collect()
    }

    pub fn artifacts(&self) -> SharedArtifacts {
        self.artifacts.clone()
    }

    /// Vybere, co nástroje za běh nasbíraly.
    pub fn take_artifacts(&self) -> RunArtifacts {
        std::mem::take(&mut *self.artifacts.lock().expect("zámek"))
    }
}

/// Testovací filesystem: obsah se dosadí z paměti, žádný disk.
#[cfg(any(test, feature = "testing"))]
pub mod fake_fs {
    use anvil_domain::{
        error::DomainResult,
        ports::{FileSlice, FsLimits, GrepHit},
    };

    use super::*;

    pub struct FakeFs {
        /// Za zámkem, protože `write` mění obsah a port má `&self` —
        /// dvojník musí umět totéž co disk, jinak proti němu nejde
        /// otestovat zápis.
        soubory: Mutex<Vec<(RelativePath, String)>>,
        limits: FsLimits,
        /// Když je vyplněné, každé volání selže touhle chybou.
        selhani: Option<String>,
    }

    impl FakeFs {
        pub fn new(soubory: &[(&str, &str)]) -> Self {
            Self {
                soubory: Mutex::new(
                    soubory
                        .iter()
                        .map(|(p, o)| {
                            (RelativePath::parse(p).expect("platná cesta"), o.to_string())
                        })
                        .collect(),
                ),
                limits: FsLimits::default(),
                selhani: None,
            }
        }

        pub fn with_limits(mut self, limits: FsLimits) -> Self {
            self.limits = limits;
            self
        }

        pub fn failing(message: &str) -> Self {
            Self {
                soubory: Mutex::new(Vec::new()),
                limits: FsLimits::default(),
                selhani: Some(message.to_string()),
            }
        }
    }

    #[async_trait]
    impl WorkspaceFs for FakeFs {
        async fn list(&self, glob: Option<&str>) -> DomainResult<Vec<RelativePath>> {
            if let Some(e) = &self.selhani {
                return Err(anvil_domain::error::DomainError::storage(e));
            }
            // Stejné porovnávání vzorů jako na disku. Dvojník, který vzorům
            // rozumí jinak než skutečnost, dělá ze zelených testů past —
            // `src/**/*.rs` mu procházel, zatímco na disku nesedl na nic.
            Ok(self
                .soubory
                .lock()
                .expect("zámek")
                .iter()
                .map(|(p, _)| p.clone())
                .filter(|p| glob.is_none_or(|g| anvil_domain::workspace::matches_glob(p, g)))
                .collect())
        }

        async fn read(
            &self,
            path: &RelativePath,
            start_line: Option<u32>,
            line_count: Option<u32>,
        ) -> DomainResult<FileSlice> {
            if let Some(e) = &self.selhani {
                return Err(anvil_domain::error::DomainError::storage(e));
            }
            let soubory = self.soubory.lock().expect("zámek");
            let obsah = soubory
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, o)| o.as_str())
                .ok_or_else(|| anvil_domain::error::DomainError::not_found(path.to_string()))?;

            let radky: Vec<&str> = obsah.lines().collect();
            let start = start_line.unwrap_or(1).max(1);
            let pocet = line_count
                .unwrap_or(self.limits.max_lines_per_read)
                .min(self.limits.max_lines_per_read);

            let od = (start as usize).saturating_sub(1).min(radky.len());
            let do_ = (od + pocet as usize).min(radky.len());

            Ok(FileSlice {
                path: path.clone(),
                start_line: start,
                text: radky[od..do_].join("\n"),
                total_lines: radky.len() as u32,
            })
        }

        async fn grep(&self, pattern: &str, _glob: Option<&str>) -> DomainResult<Vec<GrepHit>> {
            if let Some(e) = &self.selhani {
                return Err(anvil_domain::error::DomainError::storage(e));
            }
            let mut out = Vec::new();
            for (path, obsah) in self.soubory.lock().expect("zámek").iter() {
                for (i, radek) in obsah.lines().enumerate() {
                    if radek.contains(pattern) {
                        out.push(GrepHit {
                            file: path.clone(),
                            line: i as u32 + 1,
                            text: radek.to_string(),
                        });
                    }
                }
            }
            Ok(out)
        }

        async fn read_whole(&self, path: &RelativePath) -> DomainResult<Option<String>> {
            if let Some(e) = &self.selhani {
                return Err(anvil_domain::error::DomainError::storage(e));
            }
            Ok(self
                .soubory
                .lock()
                .expect("zámek")
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, o)| o.clone()))
        }

        async fn write(&self, path: &RelativePath, content: &str) -> DomainResult<()> {
            if let Some(e) = &self.selhani {
                return Err(anvil_domain::error::DomainError::storage(e));
            }
            let mut soubory = self.soubory.lock().expect("zámek");
            match soubory.iter_mut().find(|(p, _)| p == path) {
                Some((_, o)) => *o = content.to_string(),
                None => soubory.push((path.clone(), content.to_string())),
            }
            Ok(())
        }

        fn limits(&self) -> FsLimits {
            self.limits
        }
    }
}

#[cfg(test)]
mod tests {
    use anvil_domain::ports::FsLimits;
    use serde_json::json;

    use super::{fake_fs::FakeFs, *};

    fn fs() -> Arc<dyn WorkspaceFs> {
        Arc::new(FakeFs::new(&[
            (
                "src/main.rs",
                "fn main() {\n    let x = neco().unwrap();\n    println!(\"{x}\");\n}",
            ),
            (
                "src/lib.rs",
                "pub fn neco() -> Option<i32> {\n    Some(1)\n}",
            ),
            ("README.md", "# Projekt"),
        ]))
    }

    // --- list_files ---

    #[tokio::test]
    async fn list_vypise_soubory() {
        let r = ListFiles::new(fs()).call(&json!({})).await;
        assert!(!r.is_error);
        assert!(r.content.contains("src/main.rs"), "{}", r.content);
        assert!(r.content.contains("README.md"), "{}", r.content);
    }

    #[tokio::test]
    async fn list_filtruje_vzorem() {
        let r = ListFiles::new(fs()).call(&json!({"glob": "*.rs"})).await;
        assert!(r.content.contains("src/main.rs"));
        assert!(!r.content.contains("README.md"), "{}", r.content);
    }

    #[tokio::test]
    async fn list_rekne_ze_orezal() {
        // Bez toho by model nevěděl, že vidí jen část, a vyvozoval z neúplného.
        let mnoho: Vec<(String, String)> = (0..10)
            .map(|i| (format!("f{i}.rs"), String::new()))
            .collect();
        let odkazy: Vec<(&str, &str)> = mnoho
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let fs = Arc::new(FakeFs::new(&odkazy).with_limits(FsLimits {
            max_listed_files: 3,
            ..FsLimits::default()
        }));

        let r = ListFiles::new(fs).call(&json!({})).await;
        assert!(r.content.contains("a dalších 7"), "{}", r.content);
    }

    /// Prázdný výsledek filtru se nesmí dát splést s prázdným projektem —
    /// skutečný model si z „žádný soubor“ odnesl, že projekt nic neobsahuje.
    #[tokio::test]
    async fn list_bez_shody_rekne_i_kolik_souboru_projekt_ma() {
        let r = ListFiles::new(fs()).call(&json!({"glob": "*.py"})).await;
        assert!(r.content.contains("*.py"), "{}", r.content);
        assert!(
            r.content.contains("Projekt jich má"),
            "z hlášky nejde poznat, že projekt soubory má: {}",
            r.content
        );
    }

    #[tokio::test]
    async fn grep_bez_zasahu_rekne_kde_hledal() {
        // „Nikde se nevyskytuje“ je tvrzení o celém projektu. Když se
        // hledalo jen ve výseku, musí to být ze zprávy poznat.
        let r = Grep::new(fs())
            .call(&json!({"pattern": "neexistuje", "glob": "*.rs"}))
            .await;
        assert!(r.content.contains("*.rs"), "{}", r.content);
        assert!(!r.content.contains("nikde"), "{}", r.content);
    }

    #[tokio::test]
    async fn grep_se_vzorem_bez_souboru_to_prizna() {
        let r = Grep::new(fs())
            .call(&json!({"pattern": "cokoli", "glob": "*.py"}))
            .await;
        assert!(
            r.content.contains("nehledalo nikde"),
            "model musí poznat, že se nehledalo vůbec: {}",
            r.content
        );
    }

    // --- edit_file / create_file ---

    fn plan() -> SharedPlan {
        Arc::new(tokio::sync::Mutex::new(EditPlan::new()))
    }

    /// Nejdůležitější test celé fáze 4: nástroj na disk nesahá.
    #[tokio::test]
    async fn edit_file_nezapisuje_jen_navrhuje() {
        let fs = fs();
        let plan = plan();
        let r = EditFile::new(fs.clone(), plan.clone())
            .call(&json!({
                "path": "src/main.rs",
                "old_text": "neco().unwrap()",
                "new_text": "neco().unwrap_or(0)"
            }))
            .await;

        assert!(!r.is_error, "{}", r.content);
        let na_disku = fs
            .read_whole(&RelativePath::parse("src/main.rs").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(
            na_disku.contains("neco().unwrap()"),
            "soubor se změnil bez schválení"
        );
        assert_eq!(plan.lock().await.changes().len(), 1);
    }

    /// Model si musí z odpovědi odnést, že hotovo **není** — jinak
    /// v následujícím kole staví na změně, která na disku není.
    #[tokio::test]
    async fn odpoved_rekne_ze_se_ceka_na_schvaleni() {
        let r = EditFile::new(fs(), plan())
            .call(&json!({
                "path": "src/main.rs",
                "old_text": "neco().unwrap()",
                "new_text": "neco().unwrap_or(0)"
            }))
            .await;

        assert!(r.content.contains("schválení"), "{}", r.content);
        assert!(
            r.content.contains("+1"),
            "chybí rozsah změny: {}",
            r.content
        );
    }

    #[tokio::test]
    async fn nejednoznacna_uprava_dostane_srozumitelnou_chybu() {
        let fs = Arc::new(FakeFs::new(&[("a.txt", "x\nx\nx")])) as Arc<dyn WorkspaceFs>;
        let r = EditFile::new(fs, plan())
            .call(&json!({"path": "a.txt", "old_text": "x", "new_text": "y"}))
            .await;

        assert!(r.is_error);
        assert!(r.content.contains("3×"), "{}", r.content);
        assert!(r.content.contains("okolní řádky"), "{}", r.content);
    }

    #[tokio::test]
    async fn create_file_nepresahne_existujici() {
        let r = CreateFile::new(fs(), plan())
            .call(&json!({"path": "src/main.rs", "content": "nový obsah"}))
            .await;

        assert!(r.is_error);
        assert!(r.content.contains("edit_file"), "{}", r.content);
    }

    #[tokio::test]
    async fn cesta_ven_z_projektu_neprojde_ani_pri_zapisu() {
        let r = CreateFile::new(fs(), plan())
            .call(&json!({"path": "../mimo.txt", "content": "x"}))
            .await;
        assert!(r.is_error, "{}", r.content);
    }

    #[tokio::test]
    async fn sada_pro_upravy_ma_cteni_i_navrh() {
        // Bez čtení nemá model odkud vzít přesné znění úseku k nahrazení.
        let t = Toolbox::for_editing(fs(), plan());
        let jmena = t.names();
        for ocekavany in ["read_file", "list_files", "edit_file", "create_file"] {
            assert!(jmena.contains(&ocekavany.to_string()), "{jmena:?}");
        }
    }

    // --- read_file ---

    fn artefakty() -> SharedArtifacts {
        Arc::new(Mutex::new(RunArtifacts::default()))
    }

    #[tokio::test]
    async fn read_vraci_cislovane_radky() {
        // Bez čísel řádků nemá model jak nález umístit a hádal by je.
        let a = artefakty();
        let r = ReadFile::new(fs(), a)
            .call(&json!({"path": "src/main.rs"}))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("    1 | fn main()"), "{}", r.content);
        assert!(r.content.contains("    2 |"), "{}", r.content);
    }

    #[tokio::test]
    async fn read_si_pamatuje_ktere_soubory_precetl() {
        let a = artefakty();
        let t = ReadFile::new(fs(), a.clone());
        t.call(&json!({"path": "src/main.rs"})).await;
        t.call(&json!({"path": "src/main.rs"})).await;
        t.call(&json!({"path": "src/lib.rs"})).await;

        let precteno = &a.lock().unwrap().files_read;
        assert_eq!(precteno.len(), 2, "tentýž soubor se nemá počítat dvakrát");
    }

    #[tokio::test]
    async fn read_od_daneho_radku() {
        let r = ReadFile::new(fs(), artefakty())
            .call(&json!({"path": "src/main.rs", "start_line": 3}))
            .await;
        assert!(r.content.contains("    3 |"), "{}", r.content);
        assert!(!r.content.contains("    1 |"), "{}", r.content);
    }

    #[tokio::test]
    async fn read_hlasi_ze_soubor_pokracuje() {
        let r = ReadFile::new(fs(), artefakty())
            .call(&json!({"path": "src/main.rs", "line_count": 2}))
            .await;
        assert!(r.content.contains("soubor má 4 řádků"), "{}", r.content);
    }

    #[tokio::test]
    async fn read_odmitne_cestu_mimo_workspace() {
        let r = ReadFile::new(fs(), artefakty())
            .call(&json!({"path": "../../tajne.txt"}))
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("mimo workspace"), "{}", r.content);
    }

    #[tokio::test]
    async fn read_neexistujiciho_je_chyba_a_ne_prazdno() {
        let r = ReadFile::new(fs(), artefakty())
            .call(&json!({"path": "neni.rs"}))
            .await;
        assert!(r.is_error, "{}", r.content);
    }

    // --- grep ---

    #[tokio::test]
    async fn grep_najde_s_umistenim() {
        let r = Grep::new(fs()).call(&json!({"pattern": "unwrap"})).await;
        assert!(r.content.contains("src/main.rs:2"), "{}", r.content);
    }

    #[tokio::test]
    async fn grep_bez_shody_to_rekne() {
        let r = Grep::new(fs()).call(&json!({"pattern": "nikdejsem"})).await;
        assert!(r.content.contains("nevyskytuje"), "{}", r.content);
        assert!(!r.is_error, "prázdný výsledek není chyba");
    }

    // --- report_finding ---

    #[tokio::test]
    async fn nalez_se_zapise() {
        let a = artefakty();
        let r = ReportFinding::new(a.clone())
            .call(&json!({
                "file": "src/main.rs",
                "line": 2,
                "severity": "warning",
                "summary": "unwrap na Option může panikařit",
                "detail": "Použij match nebo ?"
            }))
            .await;

        assert!(!r.is_error, "{}", r.content);
        let nalezy = &a.lock().unwrap().findings;
        assert_eq!(nalezy.len(), 1);
        assert_eq!(nalezy[0].severity, Severity::Warning);
        assert_eq!(nalezy[0].line, Some(2));
        assert_eq!(nalezy[0].detail, "Použij match nebo ?");
    }

    #[tokio::test]
    async fn nalez_prijme_i_jinak_zapsanou_zavaznost() {
        let a = artefakty();
        ReportFinding::new(a.clone())
            .call(&json!({"file": "a.rs", "severity": "HIGH", "summary": "něco"}))
            .await;
        assert_eq!(a.lock().unwrap().findings[0].severity, Severity::Critical);
    }

    #[tokio::test]
    async fn nesmyslna_zavaznost_se_odmitne_s_navodem() {
        let a = artefakty();
        let r = ReportFinding::new(a.clone())
            .call(&json!({"file": "a.rs", "severity": "hodne", "summary": "něco"}))
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("critical"), "{}", r.content);
        assert!(a.lock().unwrap().findings.is_empty());
    }

    #[tokio::test]
    async fn nalez_mimo_workspace_se_odmitne() {
        let a = artefakty();
        let r = ReportFinding::new(a.clone())
            .call(&json!({"file": "/etc/passwd", "severity": "note", "summary": "x"}))
            .await;
        assert!(r.is_error);
        assert!(a.lock().unwrap().findings.is_empty());
    }

    // --- selhání filesystemu ---

    #[tokio::test]
    async fn selhani_filesystemu_je_chyba_nastroje_a_ne_pad() {
        let fs: Arc<dyn WorkspaceFs> = Arc::new(FakeFs::failing("disk odpojen"));
        for r in [
            ListFiles::new(fs.clone()).call(&json!({})).await,
            Grep::new(fs.clone()).call(&json!({"pattern": "x"})).await,
            ReadFile::new(fs, artefakty())
                .call(&json!({"path": "a.rs"}))
                .await,
        ] {
            assert!(r.is_error, "{}", r.content);
            assert!(r.content.contains("disk odpojen"), "{}", r.content);
        }
    }

    // --- sada ---

    #[test]
    fn sada_zna_vsechny_nastroje() {
        let t = Toolbox::for_review(fs());
        let jmena = t.names();
        for ocekavane in ["list_files", "grep", "read_file", "report_finding"] {
            assert!(jmena.iter().any(|n| n == ocekavane), "chybí {ocekavane}");
        }
        assert!(t.find("read_file").is_some());
        assert!(t.find("neexistuje").is_none());
    }

    #[tokio::test]
    async fn sada_sdili_artefakty_mezi_nastroji() {
        // `read_file` i `report_finding` musí zapisovat do téhož běhu.
        let t = Toolbox::for_review(fs());
        t.find("read_file")
            .unwrap()
            .call(&json!({"path": "src/main.rs"}))
            .await;
        t.find("report_finding")
            .unwrap()
            .call(&json!({"file": "src/main.rs", "severity": "note", "summary": "x"}))
            .await;

        let a = t.take_artifacts();
        assert_eq!(a.files_read.len(), 1);
        assert_eq!(a.findings.len(), 1);
    }

    #[test]
    fn specs_jsou_pouzitelne_v_promptu() {
        for spec in Toolbox::for_review(fs()).specs() {
            let radek = spec.prompt_line();
            assert!(radek.contains(&spec.name), "{radek}");
            assert!(!spec.description.is_empty(), "{} nemá popis", spec.name);
        }
    }
}
