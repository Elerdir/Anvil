//! Klient HuggingFace pro to, co není stahování souboru.

use anvil_domain::{
    error::{DomainError, DomainResult},
    ports::TokenValidator,
};
use async_trait::async_trait;
use serde::Deserialize;

const WHOAMI_URL: &str = "https://huggingface.co/api/whoami-v2";

#[derive(Deserialize)]
struct WhoAmI {
    name: String,
}

pub struct HuggingFaceClient {
    client: reqwest::Client,
}

impl HuggingFaceClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
        }
    }
}

impl Default for HuggingFaceClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TokenValidator for HuggingFaceClient {
    async fn validate_huggingface(&self, token: &str) -> DomainResult<String> {
        let token = token.trim();
        if token.is_empty() {
            return Err(DomainError::validation("token nesmí být prázdný"));
        }

        let response = self
            .client
            .get(WHOAMI_URL)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| DomainError::network(format!("HuggingFace neodpověděl: {e}")))?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            // Nejčastější případ a jediný, na který uživatel umí reagovat —
            // proto vlastní hláška místo obecného „HTTP 401".
            return Err(DomainError::validation(
                "token HuggingFace není platný. Vytvoř si nový na \
                 huggingface.co/settings/tokens (stačí oprávnění „read\").",
            ));
        }
        if !status.is_success() {
            return Err(DomainError::network(format!("HuggingFace vrátil {status}")));
        }

        let who: WhoAmI = response
            .json()
            .await
            .map_err(|e| DomainError::network(format!("odpověď HuggingFace nejde přečíst: {e}")))?;
        Ok(who.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prazdny_token_se_nezkousi_posilat() {
        // Nesmí odejít žádný požadavek — chyba je zřejmá už z tvaru vstupu.
        let klient = HuggingFaceClient::new();
        assert!(klient.validate_huggingface("").await.is_err());
        assert!(klient.validate_huggingface("   ").await.is_err());
    }

    /// Sahá na síť, proto `ignore`. Pustit ručně:
    /// `cargo test -p anvil-infrastructure -- --ignored neplatny_token`
    #[tokio::test]
    #[ignore = "sahá na síť"]
    async fn neplatny_token_da_srozumitelnou_hlasku() {
        let klient = HuggingFaceClient::new();
        let chyba = klient
            .validate_huggingface("hf_rozhodne_neplatny_token")
            .await
            .unwrap_err();
        assert!(
            chyba.to_string().contains("huggingface.co/settings/tokens"),
            "hláška má uživateli říct, kde token vzít: {chyba}"
        );
    }
}
