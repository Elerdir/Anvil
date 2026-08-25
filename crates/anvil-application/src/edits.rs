//! Plán úprav — co model navrhl a co se z toho po schválení zapíše.
//!
//! Návrh a zápis jsou schválně dvě různé věci. Nástroj umí jenom navrhnout;
//! na disk sahá až [`EditPlan::apply`], které spouští uživatel poté, co viděl
//! náhled. Model tak nemá jak něco přepsat sám od sebe, ať se splete jakkoli.
//!
//! Úpravy v jednom tahu se **řetězí**: druhá úprava téhož souboru se počítá
//! proti výsledku první, ne proti obsahu na disku. Bez toho by dvě úpravy
//! vedle sebe tiše zahodily jednu z nich.
//!
//! Schvaluje se **po souborech**, ne po jednotlivých úpravách. Přijmout druhou
//! úpravu a první ne by dalo nesmysl, protože druhá stojí na výsledku první.

use std::sync::Arc;

use anvil_domain::{
    edit::{EditError, EditKind, EditPreview},
    error::{DomainError, DomainResult},
    ports::WorkspaceFs,
    workspace::RelativePath,
};

/// Chystaná změna jednoho souboru.
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: RelativePath,
    /// Obsah před první úpravou. `None` = soubor se teprve zakládá.
    original: Option<String>,
    /// Obsah po všech úpravách v tomhle tahu.
    result: String,
    /// Kolik úprav se na tomhle souboru sešlo.
    pub edits: u32,
}

impl FileChange {
    /// Náhled celé změny — z původního obsahu na výsledný.
    ///
    /// Počítá se ze stejných dvou textů, které se použijí při zápisu, takže
    /// nemůže ukázat něco jiného, než co se provede.
    pub fn preview(&self) -> EditPreview {
        EditPreview::new(self.path.clone(), self.original.as_deref(), &self.result)
    }

    pub fn result(&self) -> &str {
        &self.result
    }
}

#[derive(Debug, Default)]
pub struct EditPlan {
    changes: Vec<FileChange>,
}

impl EditPlan {
    /// Kolik souborů smí jeden plán obsahovat.
    ///
    /// Při zakládání projektu model klidně navrhne strukturu na dvacet
    /// souborů; třicet je strop, za kterým už to nikdo neprojde očima
    /// a potvrzovací krok ztrácí smysl. Odmítnutí je hlasité, ne tiché —
    /// model se dozví, že má napřed dodat jádro.
    pub const MAX_FILES: usize = 30;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn changes(&self) -> &[FileChange] {
        &self.changes
    }

    pub fn clear(&mut self) {
        self.changes.clear();
    }

    /// Zahodí návrhy pro zadané soubory. Neznámé cesty se ignorují —
    /// zahodit něco, co tam není, je splněné přání, ne chyba.
    pub fn discard(&mut self, paths: &[RelativePath]) {
        self.changes.retain(|c| !paths.contains(&c.path));
    }

    /// Zapsané soubory po schválení. Prázdné, dokud se nezavolá `apply`.
    pub fn find(&self, path: &RelativePath) -> Option<&FileChange> {
        self.changes.iter().find(|c| &c.path == path)
    }

    /// Přidá návrh úpravy. Na disk nesahá.
    ///
    /// Vrací náhled, aby ho nástroj mohl rovnou popsat modelu — ten se
    /// z počtu přidaných a odebraných řádků dozví, jestli se trefil.
    pub async fn propose(
        &mut self,
        fs: &Arc<dyn WorkspaceFs>,
        path: RelativePath,
        kind: EditKind,
    ) -> Result<EditPreview, EditError> {
        // Rozpracovaný obsah má přednost před diskem — jinak by se druhá
        // úprava téhož souboru počítala proti stavu, který už neplatí.
        let (soucasny, puvodni) = match self.changes.iter().find(|c| c.path == path) {
            Some(c) => (Some(c.result.clone()), c.original.clone()),
            None => {
                let z_disku = fs.read_whole(&path).await.unwrap_or(None);
                (z_disku.clone(), z_disku)
            }
        };

        let novy_soubor = !self.changes.iter().any(|c| c.path == path);
        if novy_soubor && self.changes.len() >= Self::MAX_FILES {
            return Err(EditError::TooManyFiles {
                limit: Self::MAX_FILES,
            });
        }

        let vysledek = kind.apply(soucasny.as_deref()).map_err(|e| {
            // „Úsek se v souboru nevyskytuje" je matoucí, když ho model
            // opsal z disku správně a jen mezitím sám soubor změnil. Ať
            // z hlášky pozná, že si má přečíst rozpracované znění.
            match e {
                EditError::NotFound if !novy_soubor => EditError::NotFoundAfterEdit,
                jina => jina,
            }
        })?;
        let nahled = EditPreview::new(path.clone(), puvodni.as_deref(), &vysledek);

        match self.changes.iter_mut().find(|c| c.path == path) {
            Some(c) => {
                c.result = vysledek;
                c.edits += 1;
            }
            None => self.changes.push(FileChange {
                path,
                original: puvodni,
                result: vysledek,
                edits: 1,
            }),
        }
        Ok(nahled)
    }

    /// Zapíše schválené soubory a vrátí, které to byly.
    ///
    /// Před zápisem se ověří, že se soubor od návrhu **nezměnil**. Mezi
    /// návrhem a schválením může uplynout libovolně dlouho a uživatel mohl
    /// mezitím sáhnout do souboru v editoru; přepsat mu jeho práci beze slova
    /// je horší než úpravu neprovést.
    pub async fn apply(
        &mut self,
        fs: &Arc<dyn WorkspaceFs>,
        paths: &[RelativePath],
    ) -> DomainResult<Vec<RelativePath>> {
        let mut zapsane = Vec::new();

        for path in paths {
            let Some(zmena) = self.changes.iter().find(|c| &c.path == path) else {
                return Err(DomainError::not_found(format!(
                    "úprava souboru {path} v plánu není"
                )));
            };

            let na_disku = fs.read_whole(path).await?;
            if na_disku != zmena.original {
                return Err(DomainError::validation(format!(
                    "Soubor {path} se od návrhu změnil, takže se úprava nezapsala. \
                     Nech si ho projít znovu."
                )));
            }

            fs.write(path, &zmena.result).await?;
            zapsane.push(path.clone());
        }

        self.changes.retain(|c| !zapsane.contains(&c.path));
        Ok(zapsane)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::fake_fs::FakeFs;

    fn fs() -> Arc<dyn WorkspaceFs> {
        Arc::new(FakeFs::new(&[(
            "src/main.rs",
            "fn main() {\n    let x = f().unwrap();\n}",
        )]))
    }

    fn cesta(s: &str) -> RelativePath {
        RelativePath::parse(s).unwrap()
    }

    fn nahrad(old: &str, new: &str) -> EditKind {
        EditKind::Replace {
            old_text: old.into(),
            new_text: new.into(),
        }
    }

    #[tokio::test]
    async fn navrh_nesaha_na_disk() {
        // Tohle je celé jádro fáze 4: model navrhuje, nezapisuje.
        let fs = fs();
        let mut plan = EditPlan::new();
        plan.propose(
            &fs,
            cesta("src/main.rs"),
            nahrad("unwrap()", "unwrap_or(0)"),
        )
        .await
        .unwrap();

        let na_disku = fs.read_whole(&cesta("src/main.rs")).await.unwrap().unwrap();
        assert!(
            na_disku.contains("unwrap()"),
            "soubor se změnil bez schválení"
        );
        assert!(!na_disku.contains("unwrap_or"));
    }

    #[tokio::test]
    async fn schvaleni_zapise() {
        let fs = fs();
        let mut plan = EditPlan::new();
        plan.propose(
            &fs,
            cesta("src/main.rs"),
            nahrad("unwrap()", "unwrap_or(0)"),
        )
        .await
        .unwrap();

        let zapsane = plan.apply(&fs, &[cesta("src/main.rs")]).await.unwrap();

        assert_eq!(zapsane, vec![cesta("src/main.rs")]);
        let na_disku = fs.read_whole(&cesta("src/main.rs")).await.unwrap().unwrap();
        assert!(na_disku.contains("unwrap_or(0)"), "{na_disku}");
        // Zapsané se z plánu odeberou, ať nejdou zapsat podruhé.
        assert!(plan.is_empty());
    }

    #[tokio::test]
    async fn dve_upravy_tehoz_souboru_se_retezi() {
        // Kdyby se druhá počítala proti disku, první by se tiše ztratila.
        let fs = fs();
        let mut plan = EditPlan::new();
        plan.propose(&fs, cesta("src/main.rs"), nahrad("fn main", "fn hlavni"))
            .await
            .unwrap();
        plan.propose(
            &fs,
            cesta("src/main.rs"),
            nahrad("unwrap()", "unwrap_or(0)"),
        )
        .await
        .unwrap();

        plan.apply(&fs, &[cesta("src/main.rs")]).await.unwrap();

        let na_disku = fs.read_whole(&cesta("src/main.rs")).await.unwrap().unwrap();
        assert!(na_disku.contains("fn hlavni"), "{na_disku}");
        assert!(na_disku.contains("unwrap_or(0)"), "{na_disku}");
    }

    #[tokio::test]
    async fn nahled_ukazuje_celou_zmenu_ne_posledni_krok() {
        let fs = fs();
        let mut plan = EditPlan::new();
        plan.propose(&fs, cesta("src/main.rs"), nahrad("fn main", "fn hlavni"))
            .await
            .unwrap();
        plan.propose(
            &fs,
            cesta("src/main.rs"),
            nahrad("unwrap()", "unwrap_or(0)"),
        )
        .await
        .unwrap();

        let zmena = &plan.changes()[0];
        assert_eq!(zmena.edits, 2);
        let n = zmena.preview();
        // Obě změny jsou na stejném řádku? Ne — `fn main` je na prvním,
        // `unwrap` na druhém, takže se odeberou i přidají dva řádky.
        assert_eq!(n.removed, 2, "{:?}", n.lines);
        assert_eq!(n.added, 2);
    }

    #[tokio::test]
    async fn zmena_souboru_pod_rukama_zabrani_zapisu() {
        let fs = fs();
        let mut plan = EditPlan::new();
        plan.propose(
            &fs,
            cesta("src/main.rs"),
            nahrad("unwrap()", "unwrap_or(0)"),
        )
        .await
        .unwrap();

        // Uživatel mezitím sáhl do souboru v editoru.
        fs.write(&cesta("src/main.rs"), "úplně jiný obsah")
            .await
            .unwrap();

        let err = plan.apply(&fs, &[cesta("src/main.rs")]).await.unwrap_err();
        assert!(err.to_string().contains("se od návrhu změnil"), "{err}");

        // A hlavně: jeho práce tam pořád je.
        let na_disku = fs.read_whole(&cesta("src/main.rs")).await.unwrap().unwrap();
        assert_eq!(na_disku, "úplně jiný obsah");
    }

    #[tokio::test]
    async fn zalozeni_noveho_souboru() {
        let fs = fs();
        let mut plan = EditPlan::new();
        let nahled = plan
            .propose(
                &fs,
                cesta("src/novy.rs"),
                EditKind::Create {
                    content: "pub fn f() {}\n".into(),
                },
            )
            .await
            .unwrap();

        assert!(nahled.creates_file);
        plan.apply(&fs, &[cesta("src/novy.rs")]).await.unwrap();
        assert_eq!(
            fs.read_whole(&cesta("src/novy.rs")).await.unwrap(),
            Some("pub fn f() {}\n".to_string())
        );
    }

    #[tokio::test]
    async fn neschvalene_soubory_zustanou_nedotcene() {
        let fs = Arc::new(FakeFs::new(&[
            ("a.txt", "puvodni A"),
            ("b.txt", "puvodni B"),
        ])) as Arc<dyn WorkspaceFs>;
        let mut plan = EditPlan::new();
        plan.propose(&fs, cesta("a.txt"), nahrad("puvodni A", "novy A"))
            .await
            .unwrap();
        plan.propose(&fs, cesta("b.txt"), nahrad("puvodni B", "novy B"))
            .await
            .unwrap();

        // Uživatel schválil jen jeden.
        plan.apply(&fs, &[cesta("a.txt")]).await.unwrap();

        assert_eq!(
            fs.read_whole(&cesta("b.txt")).await.unwrap(),
            Some("puvodni B".to_string())
        );
        // A ten druhý v plánu zůstává, ať se dá schválit později.
        assert_eq!(plan.changes().len(), 1);
        assert_eq!(plan.changes()[0].path, cesta("b.txt"));
    }

    #[tokio::test]
    async fn nejednoznacna_uprava_se_do_planu_nedostane() {
        let fs = Arc::new(FakeFs::new(&[("a.txt", "x\nx\nx")])) as Arc<dyn WorkspaceFs>;
        let mut plan = EditPlan::new();

        let err = plan
            .propose(&fs, cesta("a.txt"), nahrad("x", "y"))
            .await
            .unwrap_err();

        assert_eq!(err, EditError::Ambiguous { count: 3 });
        assert!(plan.is_empty(), "odmítnutá úprava se nesmí zapamatovat");
    }

    /// Přesně tohle se stalo skutečné Gemmě: úspěšně opravila `cesta()`,
    /// pak poslala tutéž opravu znovu a dostala „úsek se nevyskytuje“.
    /// Ona ho přitom z disku opsala správně — jen mezitím sama soubor
    /// změnila a rozpracované znění nevidí.
    #[tokio::test]
    async fn opakovana_uprava_rekne_ze_soubor_uz_je_zmeneny() {
        let fs = fs();
        let mut plan = EditPlan::new();
        plan.propose(
            &fs,
            cesta("src/main.rs"),
            nahrad("f().unwrap()", "f().unwrap_or(0)"),
        )
        .await
        .unwrap();

        let err = plan
            .propose(
                &fs,
                cesta("src/main.rs"),
                nahrad("f().unwrap()", "f().unwrap_or_default()"),
            )
            .await
            .unwrap_err();

        assert_eq!(err, EditError::NotFoundAfterEdit);
        assert!(err.to_string().contains("předchozí úprava"), "{err}");
        assert!(err.to_string().contains("Přečti si soubor znovu"), "{err}");
    }

    #[tokio::test]
    async fn u_netknuteho_souboru_zustava_bezna_hlaska() {
        // Rozlišení má smysl jen tam, kde soubor opravdu čeká s úpravou.
        let fs = fs();
        let mut plan = EditPlan::new();
        let err = plan
            .propose(&fs, cesta("src/main.rs"), nahrad("neexistuje", "x"))
            .await
            .unwrap_err();

        assert_eq!(err, EditError::NotFound);
        assert!(err.to_string().contains("odsazení"), "{err}");
    }

    #[tokio::test]
    async fn strop_poctu_souboru_se_ozve() {
        // Při zakládání projektu model klidně navrhne strukturu na desítky
        // souborů. Nad strop už to nikdo neprojde očima a potvrzování
        // ztrácí smysl — tak se to odmítne nahlas.
        let fs = Arc::new(FakeFs::new(&[])) as Arc<dyn WorkspaceFs>;
        let mut plan = EditPlan::new();

        for i in 0..EditPlan::MAX_FILES {
            plan.propose(
                &fs,
                cesta(&format!("src/m{i}.rs")),
                EditKind::Create {
                    content: "x\n".into(),
                },
            )
            .await
            .unwrap();
        }

        let err = plan
            .propose(
                &fs,
                cesta("src/jeste_jeden.rs"),
                EditKind::Create {
                    content: "x\n".into(),
                },
            )
            .await
            .unwrap_err();

        assert!(matches!(err, EditError::TooManyFiles { .. }), "{err:?}");
        assert!(err.to_string().contains("jádro"), "{err}");
        assert_eq!(plan.changes().len(), EditPlan::MAX_FILES);
    }

    #[tokio::test]
    async fn strop_nebrani_dalsi_uprave_uz_zapocateho_souboru() {
        // Strop je na počet souborů, ne na počet úprav. Kdyby bránil i
        // opravě rozdělaného souboru, model by se zasekl na plném plánu.
        let fs = Arc::new(FakeFs::new(&[])) as Arc<dyn WorkspaceFs>;
        let mut plan = EditPlan::new();
        for i in 0..EditPlan::MAX_FILES {
            plan.propose(
                &fs,
                cesta(&format!("src/m{i}.rs")),
                EditKind::Create {
                    content: "puvodni\n".into(),
                },
            )
            .await
            .unwrap();
        }

        plan.propose(&fs, cesta("src/m0.rs"), nahrad("puvodni", "opravene"))
            .await
            .expect("úprava už započatého souboru musí projít");
    }

    #[tokio::test]
    async fn zalozeni_celeho_projektu_do_prazdne_slozky() {
        // Fáze 5 v malém: několik souborů naráz, nic na disku, dokud se
        // neschválí, a pak všechny.
        let fs = Arc::new(FakeFs::new(&[])) as Arc<dyn WorkspaceFs>;
        let mut plan = EditPlan::new();

        for (path, obsah) in [
            ("Cargo.toml", "[package]\nname = \"novy\"\n"),
            ("src/main.rs", "fn main() {\n    println!(\"ahoj\");\n}\n"),
            ("README.md", "# Nový projekt\n"),
        ] {
            plan.propose(
                &fs,
                cesta(path),
                EditKind::Create {
                    content: obsah.into(),
                },
            )
            .await
            .unwrap();
        }

        assert_eq!(plan.changes().len(), 3);
        assert!(plan.changes().iter().all(|c| c.preview().creates_file));
        // Dokud se neschválí, složka zůstává prázdná.
        assert!(fs.list(None).await.unwrap().is_empty());

        let cesty: Vec<RelativePath> = plan.changes().iter().map(|c| c.path.clone()).collect();
        plan.apply(&fs, &cesty).await.unwrap();

        assert_eq!(fs.list(None).await.unwrap().len(), 3);
        assert!(plan.is_empty());
    }

    #[tokio::test]
    async fn schvaleni_neznameho_souboru_je_chyba() {
        let fs = fs();
        let mut plan = EditPlan::new();
        let err = plan.apply(&fs, &[cesta("neni.txt")]).await.unwrap_err();
        assert!(matches!(err, DomainError::NotFound(_)), "{err}");
    }
}
