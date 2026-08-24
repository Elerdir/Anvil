//! Systémové instrukce.
//!
//! Skládají se při každém tahu znovu a do historie se neukládají — díky tomu
//! jde instrukci změnit (jiná role, jiný workspace) bez přepisování konverzace.

use anvil_domain::{model::ModelRole, workspace::Workspace};

/// Jazyk odpovědí. U modelu laděného na kód se čeština musí vyžádat výslovně,
/// jinak sklouzne do angličtiny hned u prvního delšího vysvětlení.
const CESKY: &str = "Odpovídej česky. Názvy souborů, funkcí, proměnných a útržky kódu \
     nepřekládej — ty zůstávají tak, jak jsou v projektu.";

const KODOVACI: &str = "Jsi zkušený programátor a pomáháš vývojáři s jeho projektem. \
     Odpovídej věcně a stručně. Když si nejsi jistý, řekni to rovnou místo dohadu. \
     U kódu uváděj, ve kterém souboru a na kterém řádku se věc nachází.";

const KONVERZACNI: &str = "Jsi zkušený programátor. Vysvětluješ, radíš a diskutuješ nad \
     návrhem řešení. Piš srozumitelně a bez zbytečné omáčky.";

/// Systémová instrukce pro daný režim a případně otevřenou složku projektu.
pub fn system_prompt(role: ModelRole, workspace: Option<&Workspace>) -> String {
    let zaklad = match role {
        ModelRole::Coding => KODOVACI,
        ModelRole::Conversational => KONVERZACNI,
    };

    let mut out = String::with_capacity(512);
    out.push_str(zaklad);
    out.push_str("\n\n");
    out.push_str(CESKY);

    if let Some(ws) = workspace {
        out.push_str("\n\n");
        out.push_str(&format!(
            "Pracuješ nad projektem ve složce „{}\". Cesty k souborům uváděj vždy relativně \
             k této složce, nikdy absolutně.",
            ws.name()
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn ws() -> Workspace {
        let root = if cfg!(windows) {
            PathBuf::from(r"E:\Projects\Anvil")
        } else {
            PathBuf::from("/home/dev/anvil")
        };
        Workspace::new(root).unwrap()
    }

    #[test]
    fn obe_role_vyzaduji_cestinu() {
        // Qwen3-Coder bez tohohle sklouzne do angličtiny.
        for role in ModelRole::ALL {
            assert!(
                system_prompt(role, None).contains("Odpovídej česky"),
                "{role:?}"
            );
        }
    }

    #[test]
    fn role_maji_ruzne_instrukce() {
        assert_ne!(
            system_prompt(ModelRole::Coding, None),
            system_prompt(ModelRole::Conversational, None)
        );
    }

    #[test]
    fn otevrena_slozka_se_zmini_v_instrukci() {
        let s = system_prompt(ModelRole::Coding, Some(&ws()));
        assert!(s.contains("Anvil") || s.contains("anvil"));
        assert!(s.contains("relativně"));
    }

    #[test]
    fn bez_slozky_se_o_projektu_nemluvi() {
        let s = system_prompt(ModelRole::Coding, None);
        assert!(!s.contains("Pracuješ nad projektem"));
    }

    #[test]
    fn instrukce_neobsahuje_absolutni_cestu() {
        // Absolutní cesta v promptu by modelu naznačila, že s ní smí pracovat.
        let ws = ws();
        let s = system_prompt(ModelRole::Coding, Some(&ws));
        assert!(
            !s.contains(&ws.root().display().to_string()),
            "kořen workspace do promptu nepatří: {s}"
        );
    }
}
