//! Přístupové tokeny s omezenou platností.

use std::time::{Duration, SystemTime};

#[derive(Debug, Clone)]
pub struct Token {
    pub hodnota: String,
    pub vyprsi_v: SystemTime,
}

impl Token {
    pub fn novy(hodnota: impl Into<String>, platnost: Duration) -> Self {
        Self {
            hodnota: hodnota.into(),
            vyprsi_v: SystemTime::now() + platnost,
        }
    }

    pub fn zbyva(&self) -> Duration {
        self.vyprsi_v
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO)
    }
}

#[derive(Debug, Default)]
pub struct Sklad {
    tokeny: Vec<Token>,
}

impl Sklad {
    pub fn pridej(&mut self, token: Token) {
        self.tokeny.push(token);
    }

    /// Platí tenhle token?
    pub fn je_platny(&self, hodnota: &str) -> bool {
        self.tokeny.iter().any(|t| t.hodnota == hodnota)
    }

    /// Vyhodí tokeny, kterým vypršela platnost.
    pub fn uklid(&mut self) {
        let ted = SystemTime::now();
        self.tokeny.retain(|t| t.vyprsi_v > ted);
    }
}
