//! Automatické sloučení kontextu.
//!
//! Kontextové okno je konečné a u lokálního modelu se naráží rychle — kód se
//! čte po celých souborech. Když se okno začne plnit, nechá se model
//! sesumarizovat nejstarší část konverzace, souhrn se uloží ke konverzaci
//! a ty zprávy se do promptu už neposílají. Souhrny se řetězí: další sloučení
//! dostane předchozí souhrn jako vstup.
//!
//! Rozhodovací část je čistá funkce nad konverzací ([`plan_compaction`]),
//! takže jde otestovat bez modelu i bez disku. Samotné sumarizování je až
//! [`CompactionService`], protože k němu je potřeba engine.

use std::sync::Arc;

use anvil_domain::{
    conversation::{Conversation, Message, Role},
    error::DomainResult,
    id::MessageId,
    model::Sampling,
    ports::{ChatEngine, CompletionRequest},
};
use tokio_util::sync::CancellationToken;

/// Při jakém zaplnění okna se začne slučovat.
///
/// 75 % je kompromis: dřív by se zbytečně zahazoval kontext, který se ještě
/// vejde, později by na odpověď nezbylo místo a model by se uřízl v půlce věty.
pub const DEFAULT_THRESHOLD_PERCENT: u32 = 75;

/// Kolik nejnovějších zpráv se nikdy neslučuje.
///
/// Poslední tah je aktuální dotaz uživatele a odpověď na něj — shrnout je
/// do třetí osoby by znamenalo odpovídat na parafrázi místo na otázku.
const KEEP_RECENT_MESSAGES: usize = 4;

/// Jaký podíl slučitelných zpráv se shrne najednou.
///
/// Slučovat po jedné by znamenalo drahé volání modelu skoro při každém tahu;
/// slučovat všechno by po jednom sloučení nechalo prázdnou historii.
const SUMMARIZE_FRACTION: f64 = 2.0 / 3.0;

/// Hrubý odhad tokenů pro zprávu, u které ještě neproběhlo měření.
///
/// Tři znaky na token je střed mezi českým textem (~3,5) a kódem (~2,8).
/// Používá se jen do doby, než zprávu změří skutečný tokenizer — na
/// rozhodnutí „už je čas slučovat" to stačí.
pub fn estimate_tokens(message: &Message) -> u32 {
    message
        .token_count
        .unwrap_or_else(|| (message.content.chars().count() / 3).max(1) as u32)
}

/// Co se má sloučit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionPlan {
    /// Poslední zpráva, kterou má souhrn pokrýt (včetně).
    pub through: MessageId,
    /// Kolik zpráv se shrnuje.
    pub message_count: usize,
    /// Kolik tokenů zabíraly — pro log a pro UI.
    pub reclaimed_tokens: u32,
}

/// Rozhodne, jestli je čas slučovat, a co přesně.
///
/// `budget_tokens` je to, co v okně zbývá na konverzaci — tedy velikost
/// kontextu bez místa rezervovaného na odpověď a systémovou instrukci.
/// Vrací `None`, když slučovat netřeba nebo není co.
pub fn plan_compaction(
    conversation: &Conversation,
    budget_tokens: u32,
    threshold_percent: u32,
) -> Option<CompactionPlan> {
    if budget_tokens == 0 {
        return None;
    }

    let visible = conversation.visible_messages();
    let pouzito: u32 = visible.iter().map(estimate_tokens).sum();
    let hranice = (budget_tokens as u64 * threshold_percent as u64 / 100) as u32;
    if pouzito <= hranice {
        return None;
    }

    // Nejnovější tahy zůstávají vždycky.
    let slucitelnych = visible.len().saturating_sub(KEEP_RECENT_MESSAGES);
    if slucitelnych == 0 {
        // Okno přeteklo, ale všechno viditelné je čerstvé. Slučovat nemá co —
        // ořezání řeší až engine, který prompt nacpe do okna.
        return None;
    }

    let pocet = ((slucitelnych as f64) * SUMMARIZE_FRACTION).ceil() as usize;
    let pocet = pocet.clamp(1, slucitelnych);

    let shrnovane = &visible[..pocet];
    Some(CompactionPlan {
        through: shrnovane[pocet - 1].id,
        message_count: pocet,
        reclaimed_tokens: shrnovane.iter().map(estimate_tokens).sum(),
    })
}

const SUMMARY_SYSTEM: &str = "Jsi nástroj na shrnutí konverzace mezi vývojářem a programovacím \
     asistentem. Napiš věcné shrnutí v češtině, které zachová: co se řešilo, \
     ke kterým souborům a funkcím se došlo, jaká rozhodnutí padla a co zbývá \
     udělat. Piš v odrážkách, bez úvodu a bez závěru. Nepřidávej nic, co \
     v konverzaci nezaznělo.";

/// Provede sloučení pomocí modelu.
pub struct CompactionService {
    /// Kolik tokenů má souhrn nejvýš zabrat.
    max_summary_tokens: u32,
    threshold_percent: u32,
}

impl CompactionService {
    pub fn new() -> Self {
        Self {
            max_summary_tokens: 800,
            threshold_percent: DEFAULT_THRESHOLD_PERCENT,
        }
    }

    pub fn with_threshold(mut self, percent: u32) -> Self {
        self.threshold_percent = percent.clamp(10, 95);
        self
    }

    pub fn threshold_percent(&self) -> u32 {
        self.threshold_percent
    }

    /// Sloučí kontext, pokud je potřeba. Vrací `Ok(Some(plán))`, když
    /// ke sloučení došlo.
    ///
    /// Selhání sumarizace **není** chyba, která by měla shodit odeslání
    /// zprávy — konverzace jede dál s plným kontextem a nejhorší dopad je,
    /// že prompt bude delší. Proto se chyba jen zaloguje.
    pub async fn compact_if_needed(
        &self,
        conversation: &mut Conversation,
        engine: &Arc<dyn ChatEngine>,
        budget_tokens: u32,
        cancel: CancellationToken,
    ) -> DomainResult<Option<CompactionPlan>> {
        let Some(plan) = plan_compaction(conversation, budget_tokens, self.threshold_percent)
        else {
            return Ok(None);
        };

        let k_shrnuti: Vec<Message> = conversation
            .visible_messages()
            .iter()
            .take(plan.message_count)
            .cloned()
            .collect();

        tracing::info!(
            zprav = plan.message_count,
            tokenu = plan.reclaimed_tokens,
            "Kontext se plní — slučuji nejstarší část konverzace"
        );

        let request = CompletionRequest::new(vec![Message::new(
            Role::User,
            format_for_summary(conversation.summary.as_deref(), &k_shrnuti),
        )])
        .with_system(SUMMARY_SYSTEM)
        .with_max_tokens(self.max_summary_tokens)
        // Shrnutí má být věcné a reprodukovatelné, ne kreativní.
        .with_sampling(Sampling::PRECISE);

        match engine.complete(&request, cancel, None).await {
            Ok(outcome) if !outcome.text.trim().is_empty() => {
                conversation.compact(outcome.text.trim(), plan.through)?;
                Ok(Some(plan))
            }
            Ok(_) => {
                tracing::warn!("Model vrátil prázdné shrnutí — kontext zůstává celý");
                Ok(None)
            }
            Err(e) if e.is_cancelled() => Err(e),
            Err(e) => {
                tracing::warn!(error = %e, "Shrnutí selhalo — konverzace pokračuje s plným kontextem");
                Ok(None)
            }
        }
    }
}

impl Default for CompactionService {
    fn default() -> Self {
        Self::new()
    }
}

/// Poskládá vstup pro sumarizaci. Předchozí souhrn jde na začátek, aby se
/// souhrny řetězily a nezmizelo, co bylo shrnuté minule.
fn format_for_summary(previous: Option<&str>, messages: &[Message]) -> String {
    let mut out = String::new();
    if let Some(p) = previous {
        out.push_str("Shrnutí starší části konverzace:\n");
        out.push_str(p);
        out.push_str("\n\n");
    }
    out.push_str("Konverzace ke shrnutí:\n\n");
    for m in messages {
        let kdo = match m.role {
            Role::User => "Vývojář",
            Role::Assistant => "Asistent",
            Role::Tool => "Nástroj",
            Role::System => continue,
        };
        out.push_str(kdo);
        out.push_str(": ");
        out.push_str(&m.content);
        out.push_str("\n\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zprava(role: Role, tokenu: u32) -> Message {
        Message::new(role, "x".repeat(tokenu as usize * 3)).with_token_count(tokenu)
    }

    fn konverzace(tahu: usize, tokenu_na_zpravu: u32) -> Conversation {
        let mut c = Conversation::new("test");
        for _ in 0..tahu {
            c.push(zprava(Role::User, tokenu_na_zpravu));
            c.push(zprava(Role::Assistant, tokenu_na_zpravu));
        }
        c
    }

    #[test]
    fn pod_hranici_se_neslucuje() {
        let c = konverzace(2, 100); // 400 tokenů
        assert_eq!(plan_compaction(&c, 10_000, 75), None);
    }

    #[test]
    fn nad_hranici_se_slucuje() {
        let c = konverzace(10, 100); // 2000 tokenů
        let plan = plan_compaction(&c, 2_000, 75).expect("mělo se sloučit");
        assert!(plan.message_count > 0);
        assert!(plan.reclaimed_tokens > 0);
    }

    #[test]
    fn nejnovejsi_tahy_zustavaji() {
        let c = konverzace(10, 100);
        let plan = plan_compaction(&c, 2_000, 75).unwrap();
        assert!(
            plan.message_count <= c.messages.len() - KEEP_RECENT_MESSAGES,
            "poslední tahy se slučovat nesmí, jinak se odpovídá na parafrázi"
        );
    }

    #[test]
    fn hranice_je_posledni_slucovana_zprava() {
        let c = konverzace(10, 100);
        let plan = plan_compaction(&c, 2_000, 75).unwrap();
        assert_eq!(plan.through, c.messages[plan.message_count - 1].id);
    }

    #[test]
    fn kratka_konverzace_se_neslucuje_ani_kdyz_pretece() {
        // Dva tahy, ale obří — slučovat není co, všechno je čerstvé.
        let c = konverzace(2, 5_000);
        assert_eq!(plan_compaction(&c, 1_000, 75), None);
    }

    #[test]
    fn nulovy_rozpocet_nedeli_nulou() {
        let c = konverzace(10, 100);
        assert_eq!(plan_compaction(&c, 0, 75), None);
    }

    #[test]
    fn prazdna_konverzace_se_neslucuje() {
        let c = Conversation::new("prázdná");
        assert_eq!(plan_compaction(&c, 1_000, 75), None);
    }

    #[test]
    fn slouceni_se_pocita_jen_z_viditelnych_zprav() {
        // Po prvním sloučení se nesmí do rozpočtu počítat to, co už je shrnuté.
        let mut c = konverzace(10, 100);
        let prvni = plan_compaction(&c, 2_000, 75).unwrap();
        c.compact("shrnutí", prvni.through).unwrap();

        let po = plan_compaction(&c, 2_000, 75);
        assert!(
            po.is_none(),
            "po sloučení se má vejít do rozpočtu, plán byl {po:?}"
        );
    }

    #[test]
    fn odhad_tokenu_se_pouzije_jen_bez_mereni() {
        let zmereny = Message::user("krátká").with_token_count(999);
        assert_eq!(estimate_tokens(&zmereny), 999);

        let nezmereny = Message::user("x".repeat(300));
        assert_eq!(estimate_tokens(&nezmereny), 100);
    }

    #[test]
    fn odhad_nikdy_nevrati_nulu() {
        // Nula by v součtu schovala zprávu, která v okně místo zabírá.
        assert_eq!(estimate_tokens(&Message::user("a")), 1);
        assert_eq!(estimate_tokens(&Message::user("")), 1);
    }

    #[test]
    fn nizsi_hranice_slucuje_driv() {
        let c = konverzace(10, 100); // 2000 tokenů
        assert_eq!(plan_compaction(&c, 4_000, 75), None, "2000 z 3000 se vejde");
        assert!(
            plan_compaction(&c, 4_000, 40).is_some(),
            "při hranici 40 % (1600 tokenů) už se slučovat má"
        );
    }

    #[test]
    fn vstup_pro_shrnuti_retezi_predchozi_souhrn() {
        let zpravy = vec![Message::user("dotaz"), Message::assistant("odpověď")];
        let text = format_for_summary(Some("dřívější shrnutí"), &zpravy);

        assert!(text.contains("dřívější shrnutí"));
        assert!(text.contains("Vývojář: dotaz"));
        assert!(text.contains("Asistent: odpověď"));
        assert!(
            text.find("dřívější shrnutí").unwrap() < text.find("Vývojář").unwrap(),
            "starší souhrn patří před shrnovanou konverzaci"
        );
    }

    #[test]
    fn vstup_pro_shrnuti_vynecha_systemove_zpravy() {
        let zpravy = vec![
            Message::new(Role::System, "instrukce"),
            Message::user("dotaz"),
        ];
        let text = format_for_summary(None, &zpravy);
        assert!(!text.contains("instrukce"));
    }

    #[test]
    fn hranice_sluzby_se_orizne_do_rozumu() {
        assert_eq!(
            CompactionService::new()
                .with_threshold(0)
                .threshold_percent(),
            10
        );
        assert_eq!(
            CompactionService::new()
                .with_threshold(200)
                .threshold_percent(),
            95
        );
        assert_eq!(
            CompactionService::new()
                .with_threshold(60)
                .threshold_percent(),
            60
        );
    }
}
