//! Tajemství v systémovém úložišti.
//!
//! Credential Manager na Windows, Keychain na macOS — obojí přes `keyring`.
//! Nahrazuje DPAPI, které je jen windowsové a na Macu by nešlo použít vůbec.
//!
//! Token se ukládá **až po ověření**. Uložit ho hned znamená, že se uživatel
//! o překlepu dozví až v okamžiku, kdy mu po hodině selže stahování.

use anvil_domain::{
    error::{DomainError, DomainResult},
    ports::{SecretKey, SecretStore},
};

use crate::paths::APP_NAME;

/// Uživatelské jméno u položky. Keychain i Credential Manager chtějí dvojici
/// (služba, uživatel); Anvil je jednouživatelský, takže je konstantní.
const ACCOUNT: &str = "anvil";

pub struct KeyringSecretStore {
    service: String,
}

impl KeyringSecretStore {
    pub fn new() -> Self {
        Self {
            service: APP_NAME.to_string(),
        }
    }

    /// Vlastní název služby — používají testy, aby si nesahaly do skutečného
    /// keychainu uživatele.
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn entry(&self, key: SecretKey) -> DomainResult<keyring::Entry> {
        keyring::Entry::new(&format!("{}:{}", self.service, key.entry_name()), ACCOUNT)
            .map_err(|e| DomainError::storage(format!("nelze otevřít systémové úložiště: {e}")))
    }
}

impl Default for KeyringSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretStore for KeyringSecretStore {
    fn get(&self, key: SecretKey) -> DomainResult<Option<String>> {
        match self.entry(key)?.get_password() {
            Ok(value) => Ok(Some(value)),
            // Chybějící položka není chyba — jen ještě nic uloženého není.
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(DomainError::storage(format!(
                "nelze přečíst {}: {e}",
                key.entry_name()
            ))),
        }
    }

    fn set(&self, key: SecretKey, value: &str) -> DomainResult<()> {
        let value = value.trim();
        if value.is_empty() {
            return Err(DomainError::validation("tajemství nesmí být prázdné"));
        }
        self.entry(key)?
            .set_password(value)
            .map_err(|e| DomainError::storage(format!("nelze uložit {}: {e}", key.entry_name())))
    }

    fn delete(&self, key: SecretKey) -> DomainResult<()> {
        match self.entry(key)?.delete_credential() {
            // Mazání je idempotentní — smazat neexistující položku je v pořádku.
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(DomainError::storage(format!(
                "nelze smazat {}: {e}",
                key.entry_name()
            ))),
        }
    }
}

/// Úložiště v paměti — pro testy a pro běh, kde systémový keychain není
/// k dispozici (headless CI). Nic nepřetrvá restart, což je záměr.
#[derive(Debug, Default)]
pub struct InMemorySecretStore {
    values: std::sync::Mutex<std::collections::HashMap<SecretKey, String>>,
}

impl SecretStore for InMemorySecretStore {
    fn get(&self, key: SecretKey) -> DomainResult<Option<String>> {
        Ok(self.values.lock().expect("zámek").get(&key).cloned())
    }

    fn set(&self, key: SecretKey, value: &str) -> DomainResult<()> {
        let value = value.trim();
        if value.is_empty() {
            return Err(DomainError::validation("tajemství nesmí být prázdné"));
        }
        self.values
            .lock()
            .expect("zámek")
            .insert(key, value.to_string());
        Ok(())
    }

    fn delete(&self, key: SecretKey) -> DomainResult<()> {
        self.values.lock().expect("zámek").remove(&key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v_pameti_ulozi_precte_smaze() {
        let s = InMemorySecretStore::default();
        assert_eq!(s.get(SecretKey::HuggingFace).unwrap(), None);

        s.set(SecretKey::HuggingFace, "hf_abc").unwrap();
        assert_eq!(
            s.get(SecretKey::HuggingFace).unwrap().as_deref(),
            Some("hf_abc")
        );

        s.delete(SecretKey::HuggingFace).unwrap();
        assert_eq!(s.get(SecretKey::HuggingFace).unwrap(), None);
    }

    #[test]
    fn prazdne_tajemstvi_neprojde() {
        let s = InMemorySecretStore::default();
        assert!(s.set(SecretKey::HuggingFace, "").is_err());
        assert!(s.set(SecretKey::HuggingFace, "   ").is_err());
    }

    #[test]
    fn obalujici_mezery_se_orizmou() {
        // Token zkopírovaný z webu s sebou nese mezeru nebo nový řádek.
        let s = InMemorySecretStore::default();
        s.set(SecretKey::HuggingFace, "  hf_abc\n").unwrap();
        assert_eq!(
            s.get(SecretKey::HuggingFace).unwrap().as_deref(),
            Some("hf_abc")
        );
    }

    #[test]
    fn smazani_neexistujiciho_neni_chyba() {
        let s = InMemorySecretStore::default();
        assert!(s.delete(SecretKey::HuggingFace).is_ok());
    }

    /// Sahá na skutečný systémový keychain, proto `ignore` — na CI ani
    /// v headless prostředí by neprošel. Pustit ručně:
    /// `cargo test -p anvil-infrastructure -- --ignored keyring`
    #[test]
    #[ignore = "sahá na systémový keychain"]
    fn keyring_ulozi_precte_smaze() {
        let s = KeyringSecretStore::with_service("Anvil-test");
        let _ = s.delete(SecretKey::HuggingFace);

        assert_eq!(s.get(SecretKey::HuggingFace).unwrap(), None);
        s.set(SecretKey::HuggingFace, "hf_testovaci").unwrap();
        assert_eq!(
            s.get(SecretKey::HuggingFace).unwrap().as_deref(),
            Some("hf_testovaci")
        );
        s.delete(SecretKey::HuggingFace).unwrap();
        assert_eq!(s.get(SecretKey::HuggingFace).unwrap(), None);
    }
}
