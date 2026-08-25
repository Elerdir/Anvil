//! Ukládání nahraných souborů na disk.

use std::{
    io,
    path::{Path, PathBuf},
};

pub struct Uloziste {
    koren: PathBuf,
}

impl Uloziste {
    pub fn new(koren: impl Into<PathBuf>) -> Self {
        Self {
            koren: koren.into(),
        }
    }

    /// Cesta k souboru pod kořenem úložiště.
    fn cesta(&self, jmeno: &str) -> PathBuf {
        self.koren.join(jmeno)
    }

    /// Uloží nahraný soubor pod zadaným jménem.
    ///
    /// `jmeno` chodí z HTTP requestu, tak jak ho poslal klient.
    pub fn uloz(&self, jmeno: &str, data: &[u8]) -> io::Result<PathBuf> {
        let cil = self.cesta(jmeno);
        if let Some(rodic) = cil.parent() {
            std::fs::create_dir_all(rodic)?;
        }
        std::fs::write(&cil, data)?;
        Ok(cil)
    }

    pub fn nacti(&self, jmeno: &str) -> io::Result<Vec<u8>> {
        std::fs::read(self.cesta(jmeno))
    }

    pub fn smaz(&self, jmeno: &str) -> io::Result<()> {
        std::fs::remove_file(self.cesta(jmeno))
    }

    pub fn koren(&self) -> &Path {
        &self.koren
    }
}
