//! Načtení konfigurace ze souboru `sklad.conf`.
//!
//! Formát je záměrně primitivní: jeden `klíč = hodnota` na řádek, prázdné
//! řádky a řádky začínající `#` se přeskakují.

use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct Config {
    hodnoty: HashMap<String, String>,
}

impl Config {
    pub fn nacti(obsah: &str) -> Self {
        let mut hodnoty = HashMap::new();

        for radek in obsah.lines() {
            let radek = radek.trim();
            if radek.is_empty() || radek.starts_with('#') {
                continue;
            }

            let casti: Vec<&str> = radek.splitn(2, '=').collect();
            let klic = casti[0].trim().to_string();
            let hodnota = casti[1].trim().to_string();
            hodnoty.insert(klic, hodnota);
        }

        Self { hodnoty }
    }

    pub fn text(&self, klic: &str) -> Option<&str> {
        self.hodnoty.get(klic).map(String::as_str)
    }

    pub fn cislo(&self, klic: &str) -> Option<u32> {
        self.text(klic).and_then(|v| v.parse().ok())
    }

    /// Port, na kterém sklad poslouchá. Bez uvedení se bere 8080.
    pub fn port(&self) -> u32 {
        self.cislo("port").unwrap_or(8080)
    }
}
