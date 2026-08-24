//! Konverzace a zprávy.
//!
//! Konverzace si vedle zpráv drží **souhrn** starších tahů. Když se okno
//! zaplní, aplikační vrstva nechá model sesumarizovat nejstarší část, souhrn
//! uloží sem a zprávy pod hranicí `compacted_through` se do promptu už
//! neposílají. Doména jen popisuje, co je viditelné — samotné sloučení dělá
//! `anvil-application`.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{
    error::{DomainError, DomainResult},
    id::{ConversationId, MessageId},
    model::ModelId,
};

/// Kdo zprávu poslal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Systémová instrukce. V konverzaci se neukládá jako zpráva — skládá se
    /// při každém tahu znovu, aby šla změnit bez přepisování historie.
    System,
    User,
    Assistant,
    /// Výstup nástroje, který si vyžádal model. Používá se od fáze 2.
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub role: Role,
    pub content: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Počet tokenů obsahu podle tokenizeru modelu. `None` dokud se nezměří.
    #[serde(default)]
    pub token_count: Option<u32>,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            id: MessageId::new(),
            role,
            content: content.into(),
            created_at: OffsetDateTime::now_utc(),
            token_count: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, content)
    }

    pub fn with_token_count(mut self, tokens: u32) -> Self {
        self.token_count = Some(tokens);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: ConversationId,
    pub title: String,
    /// Model, kterým konverzace běží. `None` u čerstvě založené.
    #[serde(default)]
    pub model_id: Option<ModelId>,
    /// Souhrn zpráv, které už se do promptu neposílají.
    #[serde(default)]
    pub summary: Option<String>,
    /// Poslední zpráva, kterou souhrn pokrývá (včetně).
    #[serde(default)]
    pub compacted_through: Option<MessageId>,
    /// Připnutá konverzace zůstává v seznamu nahoře.
    #[serde(default)]
    pub pinned: bool,
    /// Pozice v seznamu. Menší číslo = výš. Viz [`crate::history`].
    #[serde(default)]
    pub sort_order: i64,
    pub messages: Vec<Message>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl Conversation {
    /// Maximální délka automaticky odvozeného názvu.
    pub const TITLE_MAX_CHARS: usize = 60;

    pub fn new(title: impl Into<String>) -> Self {
        let now = OffsetDateTime::now_utc();
        Self {
            id: ConversationId::new(),
            title: title.into(),
            model_id: None,
            summary: None,
            compacted_through: None,
            pinned: false,
            sort_order: 0,
            messages: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    pub fn push(&mut self, message: Message) {
        self.updated_at = message.created_at;
        self.messages.push(message);
    }

    /// Zprávy, které se mají poslat do promptu — tedy ty, které souhrn
    /// nepokrývá. Když `compacted_through` ukazuje na zprávu, která už
    /// v konverzaci není, vrací se všechno; ztráta souhrnu je menší zlo
    /// než tichá ztráta zpráv.
    pub fn visible_messages(&self) -> &[Message] {
        let Some(boundary) = self.compacted_through else {
            return &self.messages;
        };
        match self.messages.iter().position(|m| m.id == boundary) {
            Some(idx) => &self.messages[idx + 1..],
            None => &self.messages,
        }
    }

    /// Označí zprávy po `through` (včetně) za pokryté souhrnem.
    pub fn compact(&mut self, summary: impl Into<String>, through: MessageId) -> DomainResult<()> {
        if !self.messages.iter().any(|m| m.id == through) {
            return Err(DomainError::not_found(format!(
                "zpráva {through} v konverzaci není, souhrn by odřízl špatný úsek"
            )));
        }
        self.summary = Some(summary.into());
        self.compacted_through = Some(through);
        self.updated_at = OffsetDateTime::now_utc();
        Ok(())
    }

    /// Odvodí název z prvního dotazu uživatele. Volá se jen dokud je název
    /// prázdný nebo výchozí — pojmenovanou konverzaci nikdy nepřepíše.
    pub fn derive_title(&mut self) {
        if !self.title.trim().is_empty() {
            return;
        }
        let Some(first) = self.messages.iter().find(|m| m.role == Role::User) else {
            return;
        };
        self.title = summarize_to_title(&first.content);
    }

    /// Součet naměřených tokenů viditelných zpráv. Zprávy bez změřeného
    /// počtu se ignorují, takže jde o dolní odhad.
    pub fn visible_token_estimate(&self) -> u32 {
        self.visible_messages()
            .iter()
            .filter_map(|m| m.token_count)
            .sum()
    }
}

/// Zkrátí text na jednořádkový název.
fn summarize_to_title(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return "Nová konverzace".to_string();
    }
    if flat.chars().count() <= Conversation::TITLE_MAX_CHARS {
        return flat;
    }
    // Řezat po znacích, ne po bajtech — jinak by to rozseklo diakritiku.
    let cut: String = flat
        .chars()
        .take(Conversation::TITLE_MAX_CHARS - 1)
        .collect();
    // Useknout na poslední mezeře, ať název nekončí půlkou slova.
    let cut = match cut.rfind(' ') {
        Some(i) if i > Conversation::TITLE_MAX_CHARS / 3 => cut[..i].to_string(),
        _ => cut,
    };
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn konverzace_se_zpravami(n: usize) -> Conversation {
        let mut c = Conversation::new("Test");
        for i in 0..n {
            c.push(Message::user(format!("dotaz {i}")));
            c.push(Message::assistant(format!("odpověď {i}")));
        }
        c
    }

    #[test]
    fn bez_souhrnu_jsou_videt_vsechny_zpravy() {
        let c = konverzace_se_zpravami(3);
        assert_eq!(c.visible_messages().len(), 6);
    }

    #[test]
    fn souhrn_odrizne_starsi_zpravy() {
        let mut c = konverzace_se_zpravami(3);
        let hranice = c.messages[3].id;
        c.compact("shrnutí prvních dvou kol", hranice).unwrap();

        let videt = c.visible_messages();
        assert_eq!(videt.len(), 2);
        assert_eq!(videt[0].content, "dotaz 2");
    }

    #[test]
    fn souhrn_na_neznamou_zpravu_neprojde() {
        let mut c = konverzace_se_zpravami(1);
        let cizi = MessageId::new();
        let err = c.compact("x", cizi).unwrap_err();
        assert!(matches!(err, DomainError::NotFound(_)));
        // Konverzace musí zůstat nedotčená.
        assert!(c.summary.is_none());
        assert_eq!(c.visible_messages().len(), 2);
    }

    #[test]
    fn zmizela_hranice_vrati_vsechno_misto_prazdna() {
        let mut c = konverzace_se_zpravami(2);
        let hranice = c.messages[1].id;
        c.compact("shrnutí", hranice).unwrap();
        // Zpráva se z historie vytratí (např. po smazání) — nesmí to způsobit,
        // že se do promptu nepošle nic.
        c.messages.retain(|m| m.id != hranice);
        assert_eq!(c.visible_messages().len(), 3);
    }

    #[test]
    fn nazev_se_odvodi_z_prvniho_dotazu() {
        let mut c = Conversation::new("");
        c.push(Message::assistant("pozdrav"));
        c.push(Message::user("Zkontroluj mi prosím tenhle repozitář"));
        c.derive_title();
        assert_eq!(c.title, "Zkontroluj mi prosím tenhle repozitář");
    }

    #[test]
    fn nazev_pojmenovane_konverzace_zustane() {
        let mut c = Conversation::new("Moje review");
        c.push(Message::user("něco jiného"));
        c.derive_title();
        assert_eq!(c.title, "Moje review");
    }

    #[test]
    fn dlouhy_nazev_se_zkrati_na_hranici_slova() {
        let mut c = Conversation::new("");
        c.push(Message::user(
            "Projdi mi prosím celý ten backend a napiš mi ke každému souboru, \
             co bys na něm zlepšil a proč",
        ));
        c.derive_title();

        assert!(c.title.ends_with('…'));
        assert!(c.title.chars().count() <= Conversation::TITLE_MAX_CHARS);
        assert!(!c.title.contains("  "));
    }

    #[test]
    fn nazev_z_viceradkoveho_dotazu_je_jednoradkovy() {
        let mut c = Conversation::new("");
        c.push(Message::user("první řádek\n\ndruhý řádek"));
        c.derive_title();
        assert_eq!(c.title, "první řádek druhý řádek");
    }

    #[test]
    fn odhad_tokenu_scita_jen_viditelne() {
        let mut c = Conversation::new("t");
        c.push(Message::user("a").with_token_count(10));
        let hranice = c.messages[0].id;
        c.push(Message::assistant("b").with_token_count(20));
        c.compact("s", hranice).unwrap();
        assert_eq!(c.visible_token_estimate(), 20);
    }

    #[test]
    fn konverzace_prezije_kolecko_pres_json() {
        let c = konverzace_se_zpravami(2);
        let json = serde_json::to_string(&c).unwrap();
        let zpet: Conversation = serde_json::from_str(&json).unwrap();
        assert_eq!(zpet, c);
    }
}
