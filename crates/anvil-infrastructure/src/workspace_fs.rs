//! Čtení souborů uvnitř workspace — **druhá obrana sandboxu**.
//!
//! První obranou je [`RelativePath::parse`] v doméně: čistě lexikální
//! kontrola, která odmítne absolutní cesty, únik přes `..`, řídicí znaky
//! a vyhrazené názvy zařízení. Ta ale z principu nevidí **symlinky** —
//! `odkaz.txt` může být lexikálně nevinná cesta ukazující kamkoli.
//!
//! Proto se tady každá výsledná cesta ještě kanonizuje a ověří, že pořád
//! leží pod rootem. Bez toho by stačil jediný symlink v projektu, aby model
//! přečetl `~/.ssh/id_rsa`.
//!
//! Druhé téma je **objem**. Prompt se zpracovává ~27 tokenů za sekundu, takže
//! každý řádek navíc je čas, který uživatel čeká. Ignorují se proto složky
//! se závislostmi a buildy, binární soubory a příliš dlouhé řádky.

use std::path::PathBuf;

use anvil_domain::{
    error::{DomainError, DomainResult},
    ports::{FileSlice, FsLimits, GrepHit, WorkspaceFs},
    workspace::{matches_glob, RelativePath, Workspace},
};
use async_trait::async_trait;

/// Složky, které se nikdy neprocházejí.
///
/// Nejde o názor na to, co je důležité — jsou to místa, kde leží cizí kód
/// a artefakty buildu. Projít `node_modules` znamená statisíce souborů,
/// ve kterých uživatelův problém není.
const IGNORED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".svelte-kit",
    "vendor",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".gradle",
    ".idea",
    ".vs",
    "bin",
    "obj",
    "Pods",
    ".terraform",
];

/// Přípony, které nemá smysl posílat modelu jako text.
const BINARY_EXTENSIONS: &[&str] = &[
    "png",
    "jpg",
    "jpeg",
    "gif",
    "bmp",
    "ico",
    "icns",
    "webp",
    "svgz",
    "pdf",
    "zip",
    "gz",
    "tar",
    "7z",
    "rar",
    "exe",
    "dll",
    "so",
    "dylib",
    "bin",
    "gguf",
    "safetensors",
    "onnx",
    "pt",
    "pth",
    "db",
    "sqlite",
    "wasm",
    "class",
    "jar",
    "pyc",
    "o",
    "a",
    "lib",
    "pdb",
    "mp3",
    "mp4",
    "mov",
    "avi",
    "wav",
    "ttf",
    "otf",
    "woff",
    "woff2",
];

/// Soubor větší než tohle se nečte — do kontextu se stejně nevejde a jen by
/// se na něj čekalo.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

pub struct LocalWorkspaceFs {
    workspace: Workspace,
    /// Kanonizovaný kořen. Porovnává se proti němu každá cesta.
    root: PathBuf,
    limits: FsLimits,
}

impl LocalWorkspaceFs {
    pub fn new(workspace: Workspace) -> DomainResult<Self> {
        let root = std::fs::canonicalize(workspace.root()).map_err(|e| {
            DomainError::storage(format!(
                "složku {} nelze otevřít: {e}",
                workspace.root().display()
            ))
        })?;
        Ok(Self {
            workspace,
            root,
            limits: FsLimits::default(),
        })
    }

    pub fn with_limits(mut self, limits: FsLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Absolutní cesta k souboru — **jediné místo**, kde se cesta překládá.
    ///
    /// Kanonizace tu není kosmetika: rozbaluje symlinky, na které lexikální
    /// kontrola v doméně nedosáhne. Když výsledek neleží pod rootem, je to
    /// pokus o únik, ať už úmyslný nebo ne.
    fn resolve(&self, path: &RelativePath) -> DomainResult<PathBuf> {
        let kandidat = self.workspace.resolve(path);

        let skutecna = std::fs::canonicalize(&kandidat).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                DomainError::not_found(format!("soubor {path} ve složce projektu není"))
            } else {
                DomainError::storage(format!("{path}: {e}"))
            }
        })?;

        if !skutecna.starts_with(&self.root) {
            return Err(DomainError::validation(format!(
                "cesta {path} vede přes odkaz mimo složku projektu"
            )));
        }
        Ok(skutecna)
    }

    /// Projde workspace a vrátí cesty souborů, které mají smysl číst.
    fn walk(&self) -> Vec<RelativePath> {
        let mut out = Vec::new();
        let mut fronta = vec![self.root.clone()];

        while let Some(dir) = fronta.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };

                let Ok(meta) = entry.metadata() else { continue };

                if meta.is_dir() {
                    if !is_ignored_dir(name) {
                        fronta.push(path);
                    }
                    continue;
                }
                if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
                    continue;
                }

                // Přes `relativize` — projde tím i doménová kontrola, takže
                // se do seznamu nedostane nic, co by pak `resolve` odmítl.
                if let Ok(rel) = self.workspace.relativize(&path) {
                    if !is_binary(&rel) {
                        out.push(rel);
                    }
                }
            }
        }

        out.sort();
        out
    }

    fn oriznout_radek(&self, radek: &str) -> String {
        if radek.chars().count() <= self.limits.max_line_chars {
            return radek.to_string();
        }
        let cut: String = radek.chars().take(self.limits.max_line_chars).collect();
        format!("{cut}… (řádek zkrácen)")
    }

    async fn read_text(&self, path: &RelativePath) -> DomainResult<String> {
        let abs = self.resolve(path)?;
        let bytes = tokio::fs::read(&abs)
            .await
            .map_err(|e| DomainError::storage(format!("{path}: {e}")))?;

        // Neplatné UTF-8 se nahradí, místo aby čtení selhalo — soubor
        // s jedním rozbitým bajtem je pořád užitečný.
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn is_ignored_dir(name: &str) -> bool {
    IGNORED_DIRS.iter().any(|d| d.eq_ignore_ascii_case(name))
}

fn is_binary(path: &RelativePath) -> bool {
    path.extension()
        .is_some_and(|e| BINARY_EXTENSIONS.contains(&e.as_str()))
}

#[async_trait]
impl WorkspaceFs for LocalWorkspaceFs {
    async fn list(&self, glob: Option<&str>) -> DomainResult<Vec<RelativePath>> {
        let vsechny = self.walk();
        Ok(match glob {
            Some(g) => vsechny.into_iter().filter(|p| matches_glob(p, g)).collect(),
            None => vsechny,
        })
    }

    async fn read(
        &self,
        path: &RelativePath,
        start_line: Option<u32>,
        line_count: Option<u32>,
    ) -> DomainResult<FileSlice> {
        if is_binary(path) {
            return Err(DomainError::validation(format!(
                "{path} je binární soubor, jako text nedává smysl"
            )));
        }

        let obsah = self.read_text(path).await?;
        let radky: Vec<&str> = obsah.lines().collect();

        let start = start_line.unwrap_or(1).max(1);
        let pocet = line_count
            .unwrap_or(self.limits.max_lines_per_read)
            .clamp(1, self.limits.max_lines_per_read);

        let od = (start as usize).saturating_sub(1).min(radky.len());
        let do_ = (od + pocet as usize).min(radky.len());

        let text = radky[od..do_]
            .iter()
            .map(|r| self.oriznout_radek(r))
            .collect::<Vec<_>>()
            .join("\n");

        Ok(FileSlice {
            path: path.clone(),
            start_line: start,
            text,
            total_lines: radky.len() as u32,
        })
    }

    async fn grep(&self, pattern: &str, glob: Option<&str>) -> DomainResult<Vec<GrepHit>> {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return Err(DomainError::validation("hledaný vzor nesmí být prázdný"));
        }

        let soubory = self.list(glob).await?;
        let mut out = Vec::new();
        // Strop je násobek limitu: hledá se dál i po jeho dosažení, aby se
        // dal ohlásit počet, ale ne donekonečna.
        let strop = self.limits.max_grep_hits as usize * 4;

        for path in soubory {
            if out.len() >= strop {
                break;
            }
            let Ok(obsah) = self.read_text(&path).await else {
                continue;
            };
            for (i, radek) in obsah.lines().enumerate() {
                if radek.contains(pattern) {
                    out.push(GrepHit {
                        file: path.clone(),
                        line: i as u32 + 1,
                        text: self.oriznout_radek(radek.trim()),
                    });
                    if out.len() >= strop {
                        break;
                    }
                }
            }
        }
        Ok(out)
    }

    async fn read_whole(&self, path: &RelativePath) -> DomainResult<Option<String>> {
        match self.resolve(path) {
            Ok(_) => self.read_text(path).await.map(Some),
            // Neexistující soubor není chyba — je to odpověď „ještě tam není",
            // ze které se pozná, že jde o založení nového.
            Err(DomainError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn write(&self, path: &RelativePath, content: &str) -> DomainResult<()> {
        // `resolve` kanonizuje, což u neexistujícího souboru selže. Ověřuje se
        // proto **složka**, do které se zapisuje: ta existovat musí, nebo se
        // vytvoří pod už ověřeným rodičem. Bez téhle kontroly by symlink
        // uvnitř projektu odvedl zápis kamkoli na disk.
        let cil = self.workspace.resolve(path);
        let Some(rodic) = cil.parent() else {
            return Err(DomainError::validation(format!(
                "{path} nemá nadřazenou složku"
            )));
        };

        if !rodic.exists() {
            std::fs::create_dir_all(rodic)
                .map_err(|e| DomainError::storage(format!("{path}: složku nejde vytvořit: {e}")))?;
        }
        let rodic = std::fs::canonicalize(rodic)
            .map_err(|e| DomainError::storage(format!("{path}: {e}")))?;
        if !rodic.starts_with(&self.root) {
            return Err(DomainError::validation(format!(
                "cesta {path} vede přes odkaz mimo složku projektu"
            )));
        }

        tokio::fs::write(&cil, content)
            .await
            .map_err(|e| DomainError::storage(format!("{path} nejde zapsat: {e}")))
    }

    fn limits(&self) -> FsLimits {
        self.limits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Prostredi {
        _dir: tempfile::TempDir,
        fs: LocalWorkspaceFs,
        root: PathBuf,
    }

    fn prostredi() -> Prostredi {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        let zapis = |rel: &str, obsah: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, obsah).unwrap();
        };

        zapis(
            "src/main.rs",
            "fn main() {\n    let x = f().unwrap();\n    dbg!(x);\n}",
        );
        zapis("src/lib.rs", "pub fn f() -> Option<i32> { None }");
        zapis("README.md", "# Projekt\n\nPopis.");
        zapis("node_modules/balik/index.js", "module.exports = 1;");
        zapis("target/debug/artefakt.txt", "smetí");
        zapis(".git/config", "[core]");
        zapis("assets/logo.png", "PNG-jako-binarka");

        let ws = Workspace::new(std::fs::canonicalize(&root).unwrap()).unwrap();
        let fs = LocalWorkspaceFs::new(ws).unwrap();
        Prostredi {
            _dir: dir,
            fs,
            root,
        }
    }

    fn cesta(s: &str) -> RelativePath {
        RelativePath::parse(s).unwrap()
    }

    #[tokio::test]
    async fn vypis_vzorem_pres_slozky_vrati_soubory() {
        // Totéž, ale přes skutečné `list` — vzor musí projít celou cestou
        // od nástroje k disku, ne jen porovnávací funkcí.
        let p = prostredi();
        let soubory = p.fs.list(Some("src/**/*.rs")).await.unwrap();
        let jako_text: Vec<&str> = soubory.iter().map(RelativePath::as_str).collect();

        assert!(jako_text.contains(&"src/main.rs"), "{jako_text:?}");
        assert!(jako_text.contains(&"src/lib.rs"), "{jako_text:?}");
        assert!(!jako_text.contains(&"README.md"), "{jako_text:?}");
    }

    #[tokio::test]
    async fn grep_vzorem_pres_slozky_najde_vyskyty() {
        let p = prostredi();
        let hity = p.fs.grep("unwrap", Some("src/**/*.rs")).await.unwrap();
        assert!(!hity.is_empty(), "grep se vzorem přes složky nic nenašel");
    }

    // --- výpis ---

    #[tokio::test]
    async fn vypis_najde_zdrojaky() {
        let p = prostredi();
        let soubory = p.fs.list(None).await.unwrap();
        let jako_text: Vec<&str> = soubory.iter().map(RelativePath::as_str).collect();

        assert!(jako_text.contains(&"src/main.rs"), "{jako_text:?}");
        assert!(jako_text.contains(&"README.md"), "{jako_text:?}");
    }

    #[tokio::test]
    async fn vypis_preskoci_zavislosti_a_buildy() {
        // Projít node_modules znamená statisíce souborů, ve kterých
        // uživatelův problém není.
        let p = prostredi();
        let jako_text: Vec<String> =
            p.fs.list(None)
                .await
                .unwrap()
                .iter()
                .map(|r| r.to_string())
                .collect();

        for zakazane in ["node_modules", "target", ".git"] {
            assert!(
                !jako_text.iter().any(|s| s.contains(zakazane)),
                "{zakazane} se dostalo do výpisu: {jako_text:?}"
            );
        }
    }

    #[tokio::test]
    async fn vypis_preskoci_binarni_soubory() {
        let p = prostredi();
        let jako_text: Vec<String> =
            p.fs.list(None)
                .await
                .unwrap()
                .iter()
                .map(|r| r.to_string())
                .collect();
        assert!(
            !jako_text.iter().any(|s| s.ends_with(".png")),
            "{jako_text:?}"
        );
    }

    #[tokio::test]
    async fn vypis_filtruje_priponou() {
        let p = prostredi();
        let rs = p.fs.list(Some("*.rs")).await.unwrap();
        assert_eq!(rs.len(), 2);
        assert!(rs.iter().all(|r| r.extension().as_deref() == Some("rs")));
    }

    #[tokio::test]
    async fn neznamy_vzor_radsi_propusti_vse() {
        // Tiše nevrátit nic je horší než vrátit víc — model by si myslel,
        // že projekt je prázdný.
        let p = prostredi();
        let vse = p.fs.list(None).await.unwrap().len();
        assert_eq!(p.fs.list(Some("**")).await.unwrap().len(), vse);
        assert_eq!(p.fs.list(Some("*")).await.unwrap().len(), vse);
    }

    // --- čtení ---

    #[tokio::test]
    async fn cteni_vrati_obsah_a_pocet_radku() {
        let p = prostredi();
        let s = p.fs.read(&cesta("src/main.rs"), None, None).await.unwrap();
        assert_eq!(s.start_line, 1);
        assert_eq!(s.total_lines, 4);
        assert!(s.text.starts_with("fn main()"));
        assert!(!s.truncated());
    }

    #[tokio::test]
    async fn cteni_od_radku_a_po_kusech() {
        let p = prostredi();
        let s =
            p.fs.read(&cesta("src/main.rs"), Some(2), Some(2))
                .await
                .unwrap();
        assert_eq!(s.start_line, 2);
        assert_eq!(s.end_line(), 3);
        assert!(s.truncated(), "za koncem výřezu ještě něco je");
        assert!(s.text.contains("unwrap"), "{}", s.text);
        assert!(!s.text.contains("fn main"), "{}", s.text);
    }

    #[tokio::test]
    async fn cteni_respektuje_limit_radku() {
        let dir = tempfile::tempdir().unwrap();
        let obsah: String = (1..=100).map(|i| format!("radek {i}\n")).collect();
        std::fs::write(dir.path().join("velky.txt"), obsah).unwrap();

        let ws = Workspace::new(std::fs::canonicalize(dir.path()).unwrap()).unwrap();
        let fs = LocalWorkspaceFs::new(ws).unwrap().with_limits(FsLimits {
            max_lines_per_read: 10,
            ..FsLimits::default()
        });

        // Model si vyžádá 1000 řádků — limit ho musí přebít.
        let s = fs
            .read(&cesta("velky.txt"), None, Some(1000))
            .await
            .unwrap();
        assert_eq!(s.text.lines().count(), 10);
        assert!(s.truncated());
    }

    #[tokio::test]
    async fn dlouhy_radek_se_orizne() {
        // Minifikovaný JavaScript má klidně 50 kB na řádek.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("min.js"), "x".repeat(5000)).unwrap();

        let ws = Workspace::new(std::fs::canonicalize(dir.path()).unwrap()).unwrap();
        let fs = LocalWorkspaceFs::new(ws).unwrap();

        let s = fs.read(&cesta("min.js"), None, None).await.unwrap();
        assert!(
            s.text.contains("řádek zkrácen"),
            "{}",
            &s.text[..80.min(s.text.len())]
        );
        assert!(s.text.chars().count() < 500);
    }

    #[tokio::test]
    async fn binarni_soubor_se_necte() {
        let p = prostredi();
        let e =
            p.fs.read(&cesta("assets/logo.png"), None, None)
                .await
                .unwrap_err();
        assert!(e.to_string().contains("binární"), "{e}");
    }

    #[tokio::test]
    async fn neexistujici_soubor_je_srozumitelna_chyba() {
        let p = prostredi();
        let e =
            p.fs.read(&cesta("neni/tady.rs"), None, None)
                .await
                .unwrap_err();
        assert!(e.to_string().contains("není"), "{e}");
    }

    // --- hledání ---

    #[tokio::test]
    async fn hledani_najde_s_umistenim() {
        let p = prostredi();
        let hits = p.fs.grep("unwrap", None).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file.as_str(), "src/main.rs");
        assert_eq!(hits[0].line, 2);
    }

    #[tokio::test]
    async fn hledani_nehleda_v_zavislostech() {
        let p = prostredi();
        assert!(p.fs.grep("module.exports", None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn prazdny_vzor_neprojde() {
        let p = prostredi();
        assert!(p.fs.grep("   ", None).await.is_err());
    }

    // --- sandbox ---

    #[tokio::test]
    async fn cesta_mimo_workspace_neprojde_uz_v_domene() {
        // První obrana: lexikální kontrola.
        assert!(RelativePath::parse("../tajne.txt").is_err());
        assert!(RelativePath::parse("/etc/passwd").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_ven_z_workspace_neprojde() {
        // Druhá obrana. Lexikální kontrola symlink nevidí — `odkaz` je
        // naprosto nevinná relativní cesta.
        let p = prostredi();
        let mimo = tempfile::tempdir().unwrap();
        std::fs::write(mimo.path().join("tajne.txt"), "heslo").unwrap();
        std::os::unix::fs::symlink(mimo.path().join("tajne.txt"), p.root.join("odkaz")).unwrap();

        let e = p.fs.read(&cesta("odkaz"), None, None).await.unwrap_err();
        assert!(e.to_string().contains("mimo složku projektu"), "{e}");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn junction_ven_z_workspace_neprojde() {
        // Totéž na Windows. Symlinky vyžadují oprávnění, které běžný účet
        // nemá, takže když se vytvořit nedá, test se přeskočí — jinak by
        // hlásil chybu tam, kde žádná není.
        let p = prostredi();
        let mimo = tempfile::tempdir().unwrap();
        let cil = mimo.path().join("tajne.txt");
        std::fs::write(&cil, "heslo").unwrap();

        if std::os::windows::fs::symlink_file(&cil, p.root.join("odkaz")).is_err() {
            eprintln!("symlink nejde vytvořit (chybí oprávnění) — test přeskočen");
            return;
        }

        let e = p.fs.read(&cesta("odkaz"), None, None).await.unwrap_err();
        assert!(e.to_string().contains("mimo složku projektu"), "{e}");
    }

    // --- zápis ---

    #[tokio::test]
    async fn zapis_a_zpetne_precteni() {
        let p = prostredi();
        p.fs.write(&cesta("src/novy.rs"), "obsah\n").await.unwrap();

        assert_eq!(
            p.fs.read_whole(&cesta("src/novy.rs")).await.unwrap(),
            Some("obsah\n".to_string())
        );
        assert_eq!(
            std::fs::read_to_string(p.root.join("src/novy.rs")).unwrap(),
            "obsah\n"
        );
    }

    #[tokio::test]
    async fn zapis_zalozi_chybejici_slozky() {
        let p = prostredi();
        p.fs.write(&cesta("a/b/c/hluboko.txt"), "x").await.unwrap();
        assert!(p.root.join("a/b/c/hluboko.txt").is_file());
    }

    #[tokio::test]
    async fn cely_obsah_se_neorezava() {
        // `read` ořezává na limit řádků; z toho by se úprava spočítat nedala,
        // protože by se zahodil zbytek souboru.
        let p = prostredi();
        let dlouhy: String = (0..1000).map(|i| format!("radek {i}\n")).collect();
        p.fs.write(&cesta("dlouhy.txt"), &dlouhy).await.unwrap();

        let cely =
            p.fs.read_whole(&cesta("dlouhy.txt"))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(cely.lines().count(), 1000);
    }

    #[tokio::test]
    async fn neexistujici_soubor_neni_chyba_ale_none() {
        let p = prostredi();
        assert_eq!(p.fs.read_whole(&cesta("neni.txt")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn zapis_mimo_workspace_neprojde_uz_v_domene() {
        // Lexikální obrana platí pro zápis stejně jako pro čtení.
        assert!(RelativePath::parse("../mimo.txt").is_err());
        assert!(RelativePath::parse("/etc/passwd").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn zapis_pres_odkaz_ven_neprojde() {
        // Zápis je nebezpečnější než čtení: přes odkaz by se přepsal cizí
        // soubor. Kontroluje se složka, protože cíl ještě nemusí existovat.
        let p = prostredi();
        let mimo = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(mimo.path(), p.root.join("odkaz")).unwrap();

        let e =
            p.fs.write(&cesta("odkaz/podvrzeny.txt"), "x")
                .await
                .unwrap_err();
        assert!(e.to_string().contains("mimo složku projektu"), "{e}");
        assert!(
            !mimo.path().join("podvrzeny.txt").exists(),
            "soubor se přesto zapsal"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn zapis_pres_odkaz_ven_neprojde() {
        let p = prostredi();
        let mimo = tempfile::tempdir().unwrap();

        if std::os::windows::fs::symlink_dir(mimo.path(), p.root.join("odkaz")).is_err() {
            eprintln!("symlink nejde vytvořit (chybí oprávnění) — test přeskočen");
            return;
        }

        let e =
            p.fs.write(&cesta("odkaz/podvrzeny.txt"), "x")
                .await
                .unwrap_err();
        assert!(e.to_string().contains("mimo složku projektu"), "{e}");
        assert!(
            !mimo.path().join("podvrzeny.txt").exists(),
            "soubor se přesto zapsal"
        );
    }

    #[tokio::test]
    async fn vsechny_vypsane_soubory_jdou_precist() {
        // Kdyby se do výpisu dostalo něco, co `resolve` odmítne, model by
        // dostal seznam a pak chybu u každého pokusu.
        let p = prostredi();
        for soubor in p.fs.list(None).await.unwrap() {
            assert!(
                p.fs.read(&soubor, None, Some(1)).await.is_ok(),
                "{soubor} je ve výpisu, ale nejde přečíst"
            );
        }
    }
}
