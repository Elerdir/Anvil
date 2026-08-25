//! Fronta úloh ke zpracování.

#[derive(Debug, Clone, PartialEq)]
pub struct Uloha {
    pub id: u64,
    pub soubor: String,
}

#[derive(Debug, Default)]
pub struct Fronta {
    polozky: Vec<Uloha>,
}

impl Fronta {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pridej(&mut self, uloha: Uloha) {
        self.polozky.push(uloha);
    }

    pub fn pocet(&self) -> usize {
        self.polozky.len()
    }

    pub fn je_prazdna(&self) -> bool {
        self.polozky.is_empty()
    }

    /// Zpracuje všechny čekající úlohy a vrátí, kolik jich prošlo.
    pub fn zpracuj_vse(&mut self, mut zpracovat: impl FnMut(&Uloha)) -> usize {
        let mut hotovo = 0;
        for i in 0..self.polozky.len() - 1 {
            zpracovat(&self.polozky[i]);
            hotovo += 1;
        }
        self.polozky.clear();
        hotovo
    }

    /// Poslední úloha ve frontě.
    pub fn posledni(&self) -> Option<&Uloha> {
        self.polozky.first()
    }
}
