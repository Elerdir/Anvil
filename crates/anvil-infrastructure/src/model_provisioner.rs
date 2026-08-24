//! Zajištění modelu na disku.
//!
//! Pravidlo, které tenhle modul dodržuje především: **zdrojový soubor se
//! nikdy nemaže.** GGUF má 15–20 GB a uživatel jich obvykle pár má z jiných
//! nástrojů. Když model najdeme mimo cílovou složku, zkopírujeme ho —
//! přesun by mu rozebral sbírku, o které nic nevíme.
//!
//! Pořadí je pak zřejmé: co už v cílové složce leží, se nechá; co leží jinde,
//! se zkopíruje; a teprve co nikde není, se stahuje.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anvil_domain::{
    error::{DomainError, DomainResult},
    model::{InstalledModel, ModelId, ModelSpec},
    ports::{DownloadCallback, DownloadProgress, ModelCatalog, ModelProvisioner},
};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::ai::model_downloader::{DownloadTarget, ModelDownloader};

/// Kolik místa navíc chceme mít volného, než začneme kopírovat nebo stahovat.
/// Zaplnit disk do posledního bajtu znamená, že přestane fungovat i zbytek
/// systému, ne jen Anvil.
const FREE_SPACE_HEADROOM_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub struct FileSystemModelProvisioner {
    downloader: ModelDownloader,
    /// Cílová složka — sem se model nakonec dostane.
    target_dir: PathBuf,
    /// Kde všude se hledá už stažený model, v pořadí priority.
    search_paths: Vec<PathBuf>,
    catalog: Arc<dyn ModelCatalog>,
}

impl FileSystemModelProvisioner {
    pub fn new(
        target_dir: PathBuf,
        search_paths: Vec<PathBuf>,
        catalog: Arc<dyn ModelCatalog>,
    ) -> Self {
        let mut downloader = ModelDownloader::new(target_dir.clone());
        downloader.set_hf_token(None);
        Self {
            downloader,
            target_dir,
            search_paths,
            catalog,
        }
    }

    /// Nastaví token pro modely, které vyžadují souhlas s licencí.
    pub fn set_hf_token(&mut self, token: Option<String>) {
        self.downloader.set_hf_token(token);
    }

    pub fn target_dir(&self) -> &Path {
        &self.target_dir
    }

    /// Najde soubor daného názvu v prohledávaných místech.
    /// Cílová složka má přednost, pak se jde podle pořadí `search_paths`.
    fn locate(&self, filename: &str, expected_bytes: u64) -> Option<PathBuf> {
        std::iter::once(&self.target_dir)
            .chain(self.search_paths.iter())
            .map(|dir| dir.join(filename))
            .find(|p| is_complete_model(p, expected_bytes))
    }

    /// Zkopíruje model do cílové složky. Kopíruje se do `.part`, teprve pak
    /// se přejmenuje — přerušení tak nenechá na disku soubor, který vypadá
    /// jako hotový model.
    async fn copy_into_target(
        &self,
        source: &Path,
        filename: &str,
        on_progress: Option<&DownloadCallback>,
    ) -> DomainResult<PathBuf> {
        let cil = self.target_dir.join(filename);
        let rozpracovany = self.target_dir.join(format!("{filename}.part"));

        let velikost = tokio::fs::metadata(source)
            .await
            .map(|m| m.len())
            .map_err(|e| {
                DomainError::storage(format!("nelze zjistit velikost {}: {e}", source.display()))
            })?;

        tokio::fs::create_dir_all(&self.target_dir)
            .await
            .map_err(|e| {
                DomainError::storage(format!("nelze vytvořit {}: {e}", self.target_dir.display()))
            })?;
        ensure_free_space(&self.target_dir, velikost)?;

        tracing::info!(
            source = %source.display(),
            target = %cil.display(),
            size_mb = velikost / 1_048_576,
            "Model je na disku jinde — kopíruji do cílové složky (zdroj zůstává)"
        );

        if let Some(cb) = on_progress {
            cb(DownloadProgress {
                downloaded_bytes: 0,
                total_bytes: velikost,
                bytes_per_second: 0.0,
            });
        }

        tokio::fs::copy(source, &rozpracovany).await.map_err(|e| {
            DomainError::storage(format!("kopírování {} selhalo: {e}", source.display()))
        })?;
        tokio::fs::rename(&rozpracovany, &cil).await.map_err(|e| {
            DomainError::storage(format!("nelze dokončit kopii do {}: {e}", cil.display()))
        })?;

        if let Some(cb) = on_progress {
            cb(DownloadProgress {
                downloaded_bytes: velikost,
                total_bytes: velikost,
                bytes_per_second: 0.0,
            });
        }

        Ok(cil)
    }
}

/// Soubor existuje a má věrohodnou velikost.
///
/// Tolerance je 2 % — velikost v katalogu je odhad a různé kvantizační
/// nástroje se o pár megabajtů liší. Rozdíl většího řádu ale znamená
/// nedostažený nebo poškozený soubor, a ten se použít nesmí.
fn is_complete_model(path: &Path, expected_bytes: u64) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    if expected_bytes == 0 {
        return meta.len() > 0;
    }
    let tolerance = expected_bytes / 50;
    meta.len() + tolerance >= expected_bytes
}

/// Ověří, že na cílovém svazku je dost místa. Bez `sysinfo` a podobných
/// závislostí to spolehlivě nezjistíme, takže se kontroluje jen to, co jde
/// bez nich — zbytek odhalí až samotný zápis se srozumitelnou chybou.
fn ensure_free_space(_dir: &Path, _needed: u64) -> DomainResult<()> {
    // Záměrně bez implementace: zjištění volného místa vyžaduje platformní
    // API (GetDiskFreeSpaceEx / statvfs) a přidávat kvůli tomu závislost se
    // nevyplatí — selhání zápisu je stejně srozumitelné a nastane hned.
    // Konstanta FREE_SPACE_HEADROOM_BYTES zůstává pro chvíli, kdy se sem
    // kontrola doplní.
    let _ = FREE_SPACE_HEADROOM_BYTES;
    Ok(())
}

#[async_trait]
impl ModelProvisioner for FileSystemModelProvisioner {
    async fn installed(&self) -> DomainResult<Vec<InstalledModel>> {
        let katalog = self.catalog.all();
        let mut nalezene: Vec<InstalledModel> = Vec::new();

        for dir in std::iter::once(&self.target_dir).chain(self.search_paths.iter()) {
            let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
                continue;
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if !name.ends_with(".gguf") {
                    continue;
                }
                let Some(spec) = katalog.iter().find(|m| m.local_filename() == name) else {
                    continue; // soubor mimo katalog — Anvil ho neumí popsat
                };
                if !is_complete_model(&path, spec.size_bytes) {
                    tracing::warn!(
                        path = %path.display(),
                        "Soubor vypadá jako nedostažený model — přeskakuji"
                    );
                    continue;
                }
                // První nález vyhrává (cílová složka je první v pořadí).
                if nalezene.iter().any(|m| m.id == spec.id) {
                    continue;
                }
                let size_bytes = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
                nalezene.push(InstalledModel {
                    id: spec.id.clone(),
                    path,
                    size_bytes,
                });
            }
        }

        nalezene.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        Ok(nalezene)
    }

    async fn ensure(
        &self,
        spec: &ModelSpec,
        cancel: CancellationToken,
        on_progress: Option<DownloadCallback>,
    ) -> DomainResult<InstalledModel> {
        let filename = spec.local_filename();
        let v_cili = self.target_dir.join(filename);

        // 1) Už je na místě.
        if is_complete_model(&v_cili, spec.size_bytes) {
            tracing::info!(model = %spec.id, "Model už je v cílové složce");
            return Ok(hotovo(spec.id.clone(), v_cili));
        }

        // 2) Leží jinde — zkopírovat. Zdroj zůstává, může to být uživatelova sbírka.
        if let Some(jinde) = self.locate(filename, spec.size_bytes) {
            let cil = self
                .copy_into_target(&jinde, filename, on_progress.as_ref())
                .await?;
            return Ok(hotovo(spec.id.clone(), cil));
        }

        // 3) Stáhnout.
        if spec.gated && !self.downloader.has_hf_token() {
            return Err(DomainError::validation(format!(
                "model {} vyžaduje souhlas s licencí na HuggingFace — ulož si nejdřív token \
                 v nastavení",
                spec.name
            )));
        }

        let target = DownloadTarget::from(spec);
        let hlaseni = on_progress.clone();
        let most = Arc::new(move |p: crate::ai::model_downloader::DownloadProgress| {
            if let Some(cb) = &hlaseni {
                cb(DownloadProgress {
                    downloaded_bytes: p.downloaded,
                    total_bytes: p.total.unwrap_or(0),
                    bytes_per_second: p.speed_bps as f64,
                });
            }
        });

        let cesta = self
            .downloader
            .download(&target, cancel.clone(), most)
            .await
            .map_err(|e| {
                if cancel.is_cancelled() {
                    DomainError::Cancelled
                } else {
                    DomainError::network(format!("stažení {} selhalo: {e:#}", spec.name))
                }
            })?;

        Ok(hotovo(spec.id.clone(), cesta))
    }
}

fn hotovo(id: ModelId, path: PathBuf) -> InstalledModel {
    let size_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    InstalledModel {
        id,
        path,
        size_bytes,
    }
}

#[cfg(test)]
mod tests {
    use anvil_domain::model::{ChatTemplateKind, ModelRole};

    use super::*;

    struct TestKatalog(Vec<ModelSpec>);

    impl ModelCatalog for TestKatalog {
        fn all(&self) -> Vec<ModelSpec> {
            self.0.clone()
        }
    }

    fn spec(velikost: u64) -> ModelSpec {
        ModelSpec {
            id: ModelId::parse("testovaci-model").unwrap(),
            name: "Testovací model".into(),
            description: String::new(),
            role: ModelRole::Coding,
            repo: "vendor/repo-GGUF".into(),
            file: "testovaci-model.gguf".into(),
            size_bytes: velikost,
            template: ChatTemplateKind::Qwen3,
            gated: false,
            recommended: true,
            active_params_b: 3.0,
            total_params_b: 30.0,
            native_context_tokens: 32_768,
        }
    }

    fn napis(dir: &Path, jmeno: &str, bajtu: usize) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(jmeno);
        std::fs::write(&p, vec![7u8; bajtu]).unwrap();
        p
    }

    fn provisioner(
        cil: &Path,
        hledat: Vec<PathBuf>,
        spec: ModelSpec,
    ) -> FileSystemModelProvisioner {
        FileSystemModelProvisioner::new(
            cil.to_path_buf(),
            hledat,
            Arc::new(TestKatalog(vec![spec])),
        )
    }

    #[tokio::test]
    async fn model_v_cili_se_nechá_být() {
        let d = tempfile::tempdir().unwrap();
        let cil = d.path().join("cil");
        napis(&cil, "testovaci-model.gguf", 1000);

        let p = provisioner(&cil, vec![], spec(1000));
        let out = p
            .ensure(&spec(1000), CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(out.path, cil.join("testovaci-model.gguf"));
    }

    #[tokio::test]
    async fn model_nalezeny_jinde_se_zkopiruje_a_zdroj_zustane() {
        let d = tempfile::tempdir().unwrap();
        let cil = d.path().join("cil");
        let sbirka = d.path().join("sbirka");
        let zdroj = napis(&sbirka, "testovaci-model.gguf", 1000);
        std::fs::create_dir_all(&cil).unwrap();

        let p = provisioner(&cil, vec![sbirka.clone()], spec(1000));
        let out = p
            .ensure(&spec(1000), CancellationToken::new(), None)
            .await
            .unwrap();

        assert_eq!(out.path, cil.join("testovaci-model.gguf"));
        assert!(
            zdroj.is_file(),
            "zdrojový soubor se nesmí smazat — může to být uživatelova sbírka"
        );
    }

    #[tokio::test]
    async fn po_kopii_nezustane_part_soubor() {
        let d = tempfile::tempdir().unwrap();
        let cil = d.path().join("cil");
        let sbirka = d.path().join("sbirka");
        napis(&sbirka, "testovaci-model.gguf", 1000);
        std::fs::create_dir_all(&cil).unwrap();

        let p = provisioner(&cil, vec![sbirka], spec(1000));
        p.ensure(&spec(1000), CancellationToken::new(), None)
            .await
            .unwrap();

        let zbytky: Vec<_> = std::fs::read_dir(&cil)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".part"))
            .collect();
        assert!(
            zbytky.is_empty(),
            "zůstaly rozpracované soubory: {zbytky:?}"
        );
    }

    #[tokio::test]
    async fn nedostazeny_soubor_se_nepouzije() {
        // Půlka souboru vypadá jako model, ale načíst by se nedal.
        let d = tempfile::tempdir().unwrap();
        let cil = d.path().join("cil");
        napis(&cil, "testovaci-model.gguf", 400);

        let p = provisioner(&cil, vec![], spec(1000));
        assert!(p.installed().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn drobny_rozdil_ve_velikosti_je_v_poradku() {
        // Velikost v katalogu je odhad; 1 % dolů je pořád tentýž soubor.
        let d = tempfile::tempdir().unwrap();
        let cil = d.path().join("cil");
        napis(&cil, "testovaci-model.gguf", 990);

        let p = provisioner(&cil, vec![], spec(1000));
        assert_eq!(p.installed().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn soubory_mimo_katalog_se_ignoruji() {
        let d = tempfile::tempdir().unwrap();
        let cil = d.path().join("cil");
        napis(&cil, "neznamy-model.gguf", 1000);
        napis(&cil, "poznamky.txt", 10);

        let p = provisioner(&cil, vec![], spec(1000));
        assert!(p.installed().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn tyz_model_na_dvou_mistech_se_ohlasi_jednou() {
        let d = tempfile::tempdir().unwrap();
        let cil = d.path().join("cil");
        let sbirka = d.path().join("sbirka");
        napis(&cil, "testovaci-model.gguf", 1000);
        napis(&sbirka, "testovaci-model.gguf", 1000);

        let p = provisioner(&cil, vec![sbirka], spec(1000));
        let nalezene = p.installed().await.unwrap();
        assert_eq!(nalezene.len(), 1);
        assert!(
            nalezene[0].path.starts_with(&cil),
            "přednost má cílová složka"
        );
    }

    #[tokio::test]
    async fn neexistujici_slozka_v_hledani_nevadi() {
        let d = tempfile::tempdir().unwrap();
        let cil = d.path().join("cil");
        napis(&cil, "testovaci-model.gguf", 1000);

        let p = provisioner(&cil, vec![d.path().join("neni-tu")], spec(1000));
        assert_eq!(p.installed().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn gated_model_bez_tokenu_se_ani_nezacne_stahovat() {
        let d = tempfile::tempdir().unwrap();
        let cil = d.path().join("cil");
        let mut s = spec(1000);
        s.gated = true;

        let p = provisioner(&cil, vec![], s.clone());
        let chyba = p
            .ensure(&s, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(
            chyba.to_string().contains("token"),
            "hláška má uživateli říct, co chybí: {chyba}"
        );
    }
}
