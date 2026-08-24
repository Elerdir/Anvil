//! Doménové chyby.
//!
//! Doména nezná HTTP kódy ani chyby konkrétních knihoven — infrastruktura
//! své chyby na tyhle varianty převádí. Díky tomu jde aplikační vrstva
//! testovat proti fake portům bez toho, aby znala sqlx nebo reqwest.

use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    /// Vstup nesplňuje pravidlo domény (prázdný název, cesta mimo workspace…).
    #[error("neplatný vstup: {0}")]
    Validation(String),

    /// Entita nebo zdroj neexistuje.
    #[error("nenalezeno: {0}")]
    NotFound(String),

    /// Selhala práce s modelem — načtení, tokenizace, generování.
    #[error("model: {0}")]
    Model(String),

    /// Selhal přístup k úložišti (disk, databáze, keychain).
    #[error("úložiště: {0}")]
    Storage(String),

    /// Selhala síť — stahování modelu, dotaz na HuggingFace.
    #[error("síť: {0}")]
    Network(String),

    /// Operaci zrušil uživatel. Není to chyba v pravém slova smyslu, ale
    /// prochází stejnou cestou, takže volající pozná, že se nemá hlásit.
    #[error("zrušeno")]
    Cancelled,

    /// Cokoli, co se nedalo zařadit. Používat střídmě.
    #[error("{0}")]
    Other(String),
}

impl DomainError {
    pub fn validation(msg: impl fmt::Display) -> Self {
        Self::Validation(msg.to_string())
    }

    pub fn not_found(msg: impl fmt::Display) -> Self {
        Self::NotFound(msg.to_string())
    }

    pub fn model(msg: impl fmt::Display) -> Self {
        Self::Model(msg.to_string())
    }

    pub fn storage(msg: impl fmt::Display) -> Self {
        Self::Storage(msg.to_string())
    }

    pub fn network(msg: impl fmt::Display) -> Self {
        Self::Network(msg.to_string())
    }

    pub fn other(msg: impl fmt::Display) -> Self {
        Self::Other(msg.to_string())
    }

    /// `true` pro zrušení uživatelem — volající to nemá hlásit jako chybu.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

pub type DomainResult<T> = Result<T, DomainError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zruseni_se_pozna() {
        assert!(DomainError::Cancelled.is_cancelled());
        assert!(!DomainError::validation("x").is_cancelled());
    }

    #[test]
    fn chyby_maji_ceskou_hlasku() {
        assert_eq!(
            DomainError::not_found("model qwen").to_string(),
            "nenalezeno: model qwen"
        );
    }
}
