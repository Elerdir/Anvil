//! Testovací dvojníci portů.
//!
//! Klíčový kus celé testovací strategie. Lokální model je pomalý,
//! nedeterministický a k jeho načtení je potřeba 16 GB souboru — testovat
//! proti němu logiku aplikace nejde. [`ScriptedEngine`] místo toho vrací
//! předem danou posloupnost odpovědí a zapisuje si, co dostal, takže se dá
//! přesně ověřit: co bylo v promptu, kolikrát se model volal, jak se aplikace
//! zachovala při chybě, při prázdné odpovědi i při zrušení.
//!
//! Od fáze 2 na tomhle stojí testy agentní smyčky — skript vrátí posloupnost
//! volání nástrojů a test ověří, že smyčka reaguje správně, včetně nevalidního
//! vstupu a limitu kol.
//!
//! Dostupné v testech a pod feature `testing` (aby si dvojníky mohly půjčit
//! i testy v jiných crate).

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use anvil_domain::{
    error::{DomainError, DomainResult},
    model::ModelId,
    ports::{
        ChatEngine, CompletionOutcome, CompletionRequest, GenerationProgress, ProgressCallback,
    },
};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

/// Jedna naskriptovaná reakce modelu.
#[derive(Debug, Clone)]
pub enum ScriptedResponse {
    /// Model odpoví tímhle textem.
    Text(String),
    /// Model selže.
    Failure(String),
    /// Model se nechá zrušit — vrátí, co „stihl", s příznakem zrušení.
    CancelledAfter(String),
}

impl ScriptedResponse {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }
}

/// Engine, který místo počítání vrací předem daný scénář.
pub struct ScriptedEngine {
    model: ModelId,
    context_tokens: u32,
    responses: Mutex<VecDeque<ScriptedResponse>>,
    /// Všechny požadavky v pořadí, jak přišly.
    calls: Mutex<Vec<CompletionRequest>>,
    /// Co vrátit, až scénář dojde. `None` = chyba (test na to upozorní).
    fallback: Option<String>,
}

impl ScriptedEngine {
    pub fn new(responses: impl IntoIterator<Item = ScriptedResponse>) -> Self {
        Self {
            model: ModelId::parse("scripted").expect("platné ID"),
            context_tokens: 8_192,
            responses: Mutex::new(responses.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
            fallback: None,
        }
    }

    /// Zkratka pro scénář složený jen z textových odpovědí.
    pub fn with_texts<S: Into<String>>(texts: impl IntoIterator<Item = S>) -> Self {
        Self::new(texts.into_iter().map(ScriptedResponse::text))
    }

    /// Odpovídá pořád dokola týmž textem. Pro testy, kde na obsahu nezáleží.
    pub fn always(text: impl Into<String>) -> Self {
        Self {
            fallback: Some(text.into()),
            ..Self::new([])
        }
    }

    pub fn with_context_tokens(mut self, tokens: u32) -> Self {
        self.context_tokens = tokens;
        self
    }

    pub fn with_model_id(mut self, id: ModelId) -> Self {
        self.model = id;
        self
    }

    /// Kolikrát se model volal.
    pub fn call_count(&self) -> usize {
        self.calls.lock().expect("zámek").len()
    }

    /// Požadavky v pořadí volání.
    pub fn calls(&self) -> Vec<CompletionRequest> {
        self.calls.lock().expect("zámek").clone()
    }

    /// Poslední požadavek. Panikaří, když model ještě nikdo nezavolal —
    /// v testu je to vždycky chyba testu.
    pub fn last_call(&self) -> CompletionRequest {
        self.calls
            .lock()
            .expect("zámek")
            .last()
            .cloned()
            .expect("model ještě nebyl zavolán")
    }

    /// Zbývá ve scénáři něco nevyužitého? Test to má na konci ověřit —
    /// nevyčerpaný scénář obvykle znamená, že se něco nestalo.
    pub fn remaining(&self) -> usize {
        self.responses.lock().expect("zámek").len()
    }
}

#[async_trait]
impl ChatEngine for ScriptedEngine {
    async fn complete(
        &self,
        request: &CompletionRequest,
        cancel: CancellationToken,
        on_progress: Option<ProgressCallback>,
    ) -> DomainResult<CompletionOutcome> {
        self.calls.lock().expect("zámek").push(request.clone());

        if cancel.is_cancelled() {
            return Err(DomainError::Cancelled);
        }

        let reakce = self.responses.lock().expect("zámek").pop_front();
        let (text, cancelled) = match reakce {
            Some(ScriptedResponse::Text(t)) => (t, false),
            Some(ScriptedResponse::CancelledAfter(t)) => (t, true),
            Some(ScriptedResponse::Failure(msg)) => return Err(DomainError::model(msg)),
            None => match &self.fallback {
                Some(t) => (t.clone(), false),
                None => {
                    return Err(DomainError::model(
                        "scénář ScriptedEngine je vyčerpaný — model byl volán víckrát, \
                         než test čekal",
                    ))
                }
            },
        };

        // Streamování po slovech, ať se dá otestovat i průběžné hlášení.
        if let Some(cb) = &on_progress {
            let mut nasbirano = String::new();
            for (i, kus) in text.split_inclusive(' ').enumerate() {
                nasbirano.push_str(kus);
                cb(GenerationProgress {
                    delta: kus.to_string(),
                    accumulated: nasbirano.clone(),
                    token_count: (i + 1) as u32,
                });
            }
        }

        let generated = self.count_tokens(&text)?;
        Ok(CompletionOutcome {
            text,
            prompt_tokens: request
                .messages
                .iter()
                .map(|m| self.count_tokens(&m.content).unwrap_or(0))
                .sum(),
            generated_tokens: generated,
            time_to_first_token_ms: 1,
            total_ms: 2,
            cancelled,
        })
    }

    fn count_tokens(&self, text: &str) -> DomainResult<u32> {
        // Deterministicky: tři znaky na token. Nesmí vrátit nula pro
        // neprázdný text, jinak by se v součtech ztratil.
        Ok(if text.is_empty() {
            0
        } else {
            ((text.chars().count() / 3).max(1)) as u32
        })
    }

    fn context_tokens(&self) -> u32 {
        self.context_tokens
    }

    fn model_id(&self) -> &ModelId {
        &self.model
    }
}

/// Vyrobí `Arc<dyn ChatEngine>` — zkratka, aby testy nemusely psát přetypování.
pub fn scripted(engine: ScriptedEngine) -> Arc<dyn ChatEngine> {
    Arc::new(engine)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anvil_domain::conversation::Message;

    fn pozadavek() -> CompletionRequest {
        CompletionRequest::new(vec![Message::user("dotaz")])
    }

    #[tokio::test]
    async fn vraci_odpovedi_v_poradi() {
        let e = ScriptedEngine::with_texts(["první", "druhá"]);
        let c = CancellationToken::new();

        assert_eq!(
            e.complete(&pozadavek(), c.clone(), None)
                .await
                .unwrap()
                .text,
            "první"
        );
        assert_eq!(
            e.complete(&pozadavek(), c, None).await.unwrap().text,
            "druhá"
        );
        assert_eq!(e.call_count(), 2);
        assert_eq!(e.remaining(), 0);
    }

    #[tokio::test]
    async fn vycerpany_scenar_je_chyba_a_ne_ticho() {
        // Kdyby vracel prázdno, test by prošel i při chybném počtu volání.
        let e = ScriptedEngine::with_texts(["jediná"]);
        let c = CancellationToken::new();
        e.complete(&pozadavek(), c.clone(), None).await.unwrap();
        assert!(e.complete(&pozadavek(), c, None).await.is_err());
    }

    #[tokio::test]
    async fn always_odpovida_porad_dokola() {
        let e = ScriptedEngine::always("pořád totéž");
        let c = CancellationToken::new();
        for _ in 0..5 {
            assert_eq!(
                e.complete(&pozadavek(), c.clone(), None)
                    .await
                    .unwrap()
                    .text,
                "pořád totéž"
            );
        }
    }

    #[tokio::test]
    async fn zaznamenava_pozadavky() {
        let e = ScriptedEngine::with_texts(["ok"]);
        let req = CompletionRequest::new(vec![Message::user("konkrétní dotaz")])
            .with_system("systémová instrukce");
        e.complete(&req, CancellationToken::new(), None)
            .await
            .unwrap();

        let zaznam = e.last_call();
        assert_eq!(zaznam.system.as_deref(), Some("systémová instrukce"));
        assert_eq!(zaznam.messages[0].content, "konkrétní dotaz");
    }

    #[tokio::test]
    async fn selhani_se_da_naskriptovat() {
        let e = ScriptedEngine::new([ScriptedResponse::Failure("model spadl".into())]);
        let chyba = e
            .complete(&pozadavek(), CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(chyba.to_string().contains("model spadl"));
    }

    #[tokio::test]
    async fn zruseni_pred_volanim_se_ohlasi() {
        let e = ScriptedEngine::with_texts(["nikdy"]);
        let c = CancellationToken::new();
        c.cancel();
        assert!(e
            .complete(&pozadavek(), c, None)
            .await
            .unwrap_err()
            .is_cancelled());
    }

    #[tokio::test]
    async fn zrusena_odpoved_vrati_cast_textu() {
        let e = ScriptedEngine::new([ScriptedResponse::CancelledAfter("půlka věty".into())]);
        let out = e
            .complete(&pozadavek(), CancellationToken::new(), None)
            .await
            .unwrap();
        assert!(out.cancelled);
        assert_eq!(out.text, "půlka věty");
    }

    #[tokio::test]
    async fn prubeh_se_hlasi_po_kusech() {
        let e = ScriptedEngine::with_texts(["jedna dvě tři"]);
        let sebrane = Arc::new(Mutex::new(Vec::new()));
        let sber = sebrane.clone();

        e.complete(
            &pozadavek(),
            CancellationToken::new(),
            Some(Arc::new(move |p: GenerationProgress| {
                sber.lock().unwrap().push(p.delta);
            })),
        )
        .await
        .unwrap();

        let kusy = sebrane.lock().unwrap().clone();
        assert!(kusy.len() > 1, "mělo přijít víc dávek: {kusy:?}");
        assert_eq!(kusy.concat(), "jedna dvě tři");
    }

    #[test]
    fn pocitani_tokenu_je_deterministicke() {
        let e = ScriptedEngine::with_texts([""; 0]);
        assert_eq!(e.count_tokens("").unwrap(), 0);
        assert_eq!(e.count_tokens("abc").unwrap(), 1);
        assert_eq!(
            e.count_tokens("a").unwrap(),
            1,
            "neprázdný text nesmí dát nulu"
        );
        assert_eq!(e.count_tokens(&"x".repeat(300)).unwrap(), 100);
    }
}
