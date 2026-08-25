//! Přihlášené relace uživatelů.

use std::{collections::HashMap, io, path::PathBuf, time::SystemTime};

pub struct Relace {
    pub uzivatel: String,
    pub zalozena: SystemTime,
}

pub struct Sprava {
    relace: HashMap<String, Relace>,
    soubor: PathBuf,
}

impl Sprava {
    pub fn new(soubor: impl Into<PathBuf>) -> Self {
        Self {
            relace: HashMap::new(),
            soubor: soubor.into(),
        }
    }

    /// Přihlásí uživatele a vrátí ID relace.
    pub fn prihlas(&mut self, uzivatel: &str, id: String) -> String {
        self.relace.insert(
            id.clone(),
            Relace {
                uzivatel: uzivatel.to_string(),
                zalozena: SystemTime::now(),
            },
        );

        // Relace se musí přežít restart serveru.
        let _ = self.uloz_na_disk();

        id
    }

    pub fn odhlas(&mut self, id: &str) {
        self.relace.remove(id);
        let _ = self.uloz_na_disk();
    }

    pub fn uzivatel(&self, id: &str) -> Option<&str> {
        self.relace.get(id).map(|r| r.uzivatel.as_str())
    }

    fn uloz_na_disk(&self) -> io::Result<()> {
        let radky: Vec<String> = self
            .relace
            .iter()
            .map(|(id, r)| format!("{id}\t{}", r.uzivatel))
            .collect();
        std::fs::write(&self.soubor, radky.join("\n"))
    }
}
