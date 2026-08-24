//! Konverzace a zprávy.
//!
//! Konverzace si vedle zpráv drží **souhrn** starších tahů. Když se okno
//! zaplní, aplikační vrstva nechá model sesumarizovat nejstarší část, souhrn
//! uloží sem a zprávy pod hranicí `compacted_through` se do promptu už
//! neposílají. Doména jen popisuje, co je viditelné — samotné sloučení dělá
//! `anvil-application`.

use std::collections::HashMap;

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

/// Odkud se konverzace odvětvila.
///
/// Ukazuje do **rodiče**, ne do sebe. Zprávy se při větvení kopírují a
/// dostávají nová ID, takže odkaz na původní zprávu je jediné, co po větvení
/// zbyde jako spojnice mezi vlákny.
///
/// Kopírují se schválně, i když je to na první pohled plýtvání: kdyby větev
/// sdílela zprávy s rodičem, smazání rodiče by vykuchalo všechny jeho větve
/// a slučování starých tahů v jednom vlákně by tiše měnilo obsah druhého.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchPoint {
    pub parent: ConversationId,
    /// Zpráva rodiče, u které se větvilo.
    pub at_message: MessageId,
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
    /// Odkud vlákno vzniklo. `None` u konverzace založené napřímo.
    #[serde(default)]
    pub branched_from: Option<BranchPoint>,
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
            branched_from: None,
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

    /// Nová konverzace s historií **včetně** zadané zprávy.
    ///
    /// „Odsud jinudy" — původní vlákno zůstane nedotčené a dál se pokračuje
    /// v kopii. Vrácená konverzace má prázdný název; pojmenovat ji je na
    /// volajícím, protože jen ten vidí ostatní názvy v seznamu.
    pub fn branch_through(&self, at: MessageId) -> DomainResult<Conversation> {
        let idx = self.index_of(at)?;
        Ok(self.branch(idx + 1, at))
    }

    /// Nová konverzace s historií **před** zadanou zprávou.
    ///
    /// „Zeptat se znovu jinak" — zpráva, na kterou se ukazuje, se nezkopíruje,
    /// takže větev končí tam, odkud má jít nové zadání.
    pub fn branch_before(&self, at: MessageId) -> DomainResult<Conversation> {
        let idx = self.index_of(at)?;
        Ok(self.branch(idx, at))
    }

    fn index_of(&self, id: MessageId) -> DomainResult<usize> {
        self.messages
            .iter()
            .position(|m| m.id == id)
            .ok_or_else(|| DomainError::not_found(format!("zpráva {id} v téhle konverzaci není")))
    }

    /// Společné jádro obou způsobů větvení. `keep` je počet zpráv od začátku,
    /// `at` zpráva rodiče, u které uživatel větvil.
    fn branch(&self, keep: usize, at: MessageId) -> Conversation {
        let now = OffsetDateTime::now_utc();

        // Zprávy dostávají nová ID. Obě vlákna žijou vedle sebe ve stejné
        // tabulce, kde je ID primární klíč — se sdílenými ID by uložení
        // větve přepsalo zprávy rodiče.
        let mut preklad: HashMap<MessageId, MessageId> = HashMap::new();
        let messages: Vec<Message> = self.messages[..keep]
            .iter()
            .map(|m| {
                let kopie = Message {
                    id: MessageId::new(),
                    ..m.clone()
                };
                preklad.insert(m.id, kopie.id);
                kopie
            })
            .collect();

        // Souhrn se přenese jen tehdy, když hranice padla do zkopírovaného
        // úseku. Jinak by větev dostala shrnutí tahů, které v ní nejsou —
        // model by pak odpovídal s ohledem na rozhovor, který se v tomhle
        // vlákně nikdy neodehrál.
        let (summary, compacted_through) = match self.compacted_through {
            Some(hranice) => match preklad.get(&hranice) {
                Some(nova) => (self.summary.clone(), Some(*nova)),
                None => (None, None),
            },
            None => (None, None),
        };

        Conversation {
            id: ConversationId::new(),
            title: String::new(),
            model_id: self.model_id.clone(),
            summary,
            compacted_through,
            // Připnutí je volba uživatele o konkrétním vlákně, ne vlastnost
            // obsahu — do větve se nedědí.
            pinned: false,
            sort_order: 0,
            branched_from: Some(BranchPoint {
                parent: self.id,
                at_message: at,
            }),
            messages,
            created_at: now,
            updated_at: now,
        }
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

    // --- větvení ---------------------------------------------------------

    #[test]
    fn vetev_si_odnese_historii_vcetne_zvolene_zpravy() {
        let c = konverzace_se_zpravami(3);
        let vetev = c.branch_through(c.messages[3].id).unwrap();

        assert_eq!(vetev.messages.len(), 4);
        assert_eq!(vetev.messages[3].content, "odpověď 1");
    }

    #[test]
    fn vetev_pred_zpravou_ji_uz_neobsahuje() {
        let c = konverzace_se_zpravami(3);
        let vetev = c.branch_before(c.messages[2].id).unwrap();

        assert_eq!(vetev.messages.len(), 2);
        assert_eq!(vetev.messages[1].content, "odpověď 0");
    }

    #[test]
    fn vetev_pred_prvni_zpravou_zacina_prazdna() {
        let c = konverzace_se_zpravami(2);
        let vetev = c.branch_before(c.messages[0].id).unwrap();

        assert!(vetev.messages.is_empty());
        assert!(vetev.branched_from.is_some());
    }

    /// Nejdůležitější test z celého větvení: obě vlákna se ukládají do jedné
    /// tabulky, kde je ID zprávy primární klíč. Kdyby se ID zkopírovala,
    /// uložení větve by přepsalo zprávy rodiče.
    #[test]
    fn zpravy_ve_vetvi_maji_nova_id() {
        let c = konverzace_se_zpravami(2);
        let vetev = c.branch_through(c.messages[3].id).unwrap();

        let puvodni: Vec<_> = c.messages.iter().map(|m| m.id).collect();
        for m in &vetev.messages {
            assert!(
                !puvodni.contains(&m.id),
                "zpráva {} si nesla ID z rodiče",
                m.content
            );
        }
        assert_ne!(vetev.id, c.id);
    }

    #[test]
    fn vetveni_nemeni_puvodni_konverzaci() {
        let c = konverzace_se_zpravami(3);
        let pred = c.clone();
        let _ = c.branch_before(c.messages[2].id).unwrap();

        assert_eq!(c, pred);
    }

    #[test]
    fn vetev_na_neznamou_zpravu_neprojde() {
        let c = konverzace_se_zpravami(1);
        let err = c.branch_through(MessageId::new()).unwrap_err();
        assert!(matches!(err, DomainError::NotFound(_)));
    }

    #[test]
    fn vetev_si_pamatuje_odkud_vznikla() {
        let c = konverzace_se_zpravami(2);
        let misto = c.messages[1].id;
        let vetev = c.branch_through(misto).unwrap();

        let odkud = vetev.branched_from.expect("větev zná rodiče");
        assert_eq!(odkud.parent, c.id);
        // Ukazuje na zprávu **rodiče**, ne na kopii ve větvi.
        assert_eq!(odkud.at_message, misto);
    }

    #[test]
    fn souhrn_se_prenese_kdyz_hranice_padla_do_vetve() {
        let mut c = konverzace_se_zpravami(3);
        let hranice = c.messages[1].id;
        c.compact("shrnutí prvního kola", hranice).unwrap();

        let vetev = c.branch_through(c.messages[3].id).unwrap();

        assert_eq!(vetev.summary.as_deref(), Some("shrnutí prvního kola"));
        // Hranice musí ukazovat na kopii, ne na zprávu rodiče — jinak by ji
        // `visible_messages` nenašlo a souhrn by se tiše přestal používat.
        assert_eq!(vetev.compacted_through, Some(vetev.messages[1].id));
        assert_eq!(vetev.visible_messages().len(), 2);
    }

    #[test]
    fn souhrn_se_zahodi_kdyz_hranice_zustala_mimo_vetev() {
        let mut c = konverzace_se_zpravami(3);
        let hranice = c.messages[3].id;
        c.compact("shrnutí dvou kol", hranice).unwrap();

        // Větev končí dřív, než kam souhrn sahá.
        let vetev = c.branch_through(c.messages[1].id).unwrap();

        assert!(vetev.summary.is_none(), "souhrn shrnoval cizí tahy");
        assert!(vetev.compacted_through.is_none());
        assert_eq!(vetev.visible_messages().len(), 2);
    }

    #[test]
    fn vetev_zdedi_model_ale_ne_pripnuti() {
        let mut c = konverzace_se_zpravami(1);
        c.model_id = Some(ModelId::parse("gemma").unwrap());
        c.pinned = true;

        let vetev = c.branch_through(c.messages[0].id).unwrap();

        assert_eq!(vetev.model_id, c.model_id);
        assert!(!vetev.pinned);
    }

    #[test]
    fn vetev_nema_nazev_a_odvodi_si_ho_z_prvniho_dotazu() {
        let c = konverzace_se_zpravami(2);
        let mut vetev = c.branch_before(c.messages[2].id).unwrap();
        assert!(vetev.title.is_empty());

        vetev.derive_title();
        assert_eq!(vetev.title, "dotaz 0");
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
