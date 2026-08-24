//! Znovupoužití KV cache mezi tahy.
//!
//! Prompt se u každého tahu skládá celý znovu, ale jeho **začátek** se od
//! minule nezměnil — systémová instrukce a starší tahy jsou tytéž. Tokeny,
//! které už v KV cache jsou, se proto nemusí počítat podruhé; stačí zahodit
//! cache od prvního místa, kde se prompty rozcházejí, a dopočítat jen zbytek.
//!
//! Proč to stojí za to: na hybridním MoE běhu (experti v RAM) jede zpracování
//! promptu jen **~26 tokenů za sekundu** — o málo rychleji než samotné
//! generování. Naměřeno na gemma-4-26B-A4B, RTX 4070 Laptop 8 GB:
//!
//! | tokenů promptu | zpracování |
//! |---|---|
//! | 1 069 | 39 s |
//! | 2 109 | 78 s |
//! | 4 124 | 154 s |
//!
//! Bez znovupoužití by uživatel u konverzace s obsahem jednoho zdrojáku čekal
//! na začátek **každé** odpovědi minuty. S ním se dopočítá jen nová zpráva.
//!
//! Naměřený dopad na druhém tahu konverzace (`examples/smoke`, tentýž model
//! a tentýž dotaz):
//!
//! | | 1. tah | 2. tah (277 tokenů promptu) |
//! |---|---|---|
//! | bez znovupoužití | 2,9 s | **30,8 s** |
//! | se znovupoužitím | 2,6 s | **1,6 s** |
//!
//! Druhý tah je se znovupoužitím rychlejší než první, protože se dopočítává
//! jen nová zpráva místo celé konverzace.
//!
//! Modul je záměrně mimo feature `engine` — je to čistá logika nad seznamem
//! čísel a testy k ní mají běžet i bez llama.cpp.

/// Kolik tokenů od začátku má `predchozi` společných s `novy`.
///
/// Výsledek je omezený na `novy.len() - 1`: aby model mohl vygenerovat další
/// token, musí se aspoň jeden token promptu skutečně zpracovat a vydat logity.
/// Kdyby se shodl celý prompt, neměl by z čeho vzorkovat.
pub fn reusable_prefix(predchozi: &[i32], novy: &[i32]) -> usize {
    if novy.is_empty() {
        return 0;
    }
    let strop = novy.len() - 1;
    predchozi
        .iter()
        .zip(novy.iter())
        .take(strop)
        .take_while(|(a, b)| a == b)
        .count()
}

/// Vyplatí se prefix znovu použít?
///
/// Zahození a dopočítání pár tokenů je levné, ale samotné `seq_rm` a přeskládání
/// dávky taky něco stojí. Pod touhle hranicí se prostě začne od nuly — kód je
/// tím pádem v jedné větvi a nespoléhá na to, že drobné znovupoužití vyjde.
pub const MIN_WORTHWHILE_PREFIX: usize = 32;

/// Rozhodnutí, co se má před zpracováním promptu udělat s KV cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePlan {
    /// Zahodit celou cache a zpracovat prompt od začátku.
    Rebuild,
    /// Zahodit cache od pozice `keep` dál a dopočítat jen `novy[keep..]`.
    Reuse { keep: usize },
}

impl CachePlan {
    /// Od které pozice se bude dekódovat.
    pub fn start_position(self) -> usize {
        match self {
            CachePlan::Rebuild => 0,
            CachePlan::Reuse { keep } => keep,
        }
    }
}

/// Naplánuje práci s cache pro nový prompt.
///
/// `context_tokens` je velikost okna — když se nový prompt nevejde, nemá cenu
/// nic zachovávat, protože se stejně nepodaří dokončit; volající to ohlásí
/// jako chybu, ale plán zůstane konzistentní.
pub fn plan_cache(predchozi: &[i32], novy: &[i32]) -> CachePlan {
    let prefix = reusable_prefix(predchozi, novy);
    if prefix >= MIN_WORTHWHILE_PREFIX {
        CachePlan::Reuse { keep: prefix }
    } else {
        CachePlan::Rebuild
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shodny_zacatek_se_pozna() {
        let a = [1, 2, 3, 4, 5];
        let b = [1, 2, 3, 9, 9];
        assert_eq!(reusable_prefix(&a, &b), 3);
    }

    #[test]
    fn zadna_shoda_da_nulu() {
        assert_eq!(reusable_prefix(&[1, 2, 3], &[9, 8, 7]), 0);
    }

    #[test]
    fn prazdna_historie_da_nulu() {
        assert_eq!(reusable_prefix(&[], &[1, 2, 3]), 0);
    }

    #[test]
    fn prazdny_novy_prompt_da_nulu() {
        // Nesmí to podtéct na `novy.len() - 1`.
        assert_eq!(reusable_prefix(&[1, 2, 3], &[]), 0);
    }

    #[test]
    fn cely_shodny_prompt_necha_jeden_token_k_dopoctu() {
        // Bez toho by se nedekódovalo nic a nebylo by z čeho vzorkovat.
        let a = [1, 2, 3, 4];
        assert_eq!(reusable_prefix(&a, &a), 3);
    }

    #[test]
    fn jednotokenovy_prompt_se_vzdy_pocita_cely() {
        assert_eq!(reusable_prefix(&[7], &[7]), 0);
    }

    #[test]
    fn delsi_historie_nez_prompt_nevadi() {
        // Po sloučení kontextu je nový prompt kratší než to, co je v cache.
        let predchozi = [1, 2, 3, 4, 5, 6, 7, 8];
        let novy = [1, 2, 3];
        assert_eq!(reusable_prefix(&predchozi, &novy), 2);
    }

    #[test]
    fn kratky_prefix_se_nevyplati() {
        let predchozi: Vec<i32> = (0..10).collect();
        let mut novy = predchozi.clone();
        novy.extend(100..200);
        // Shoda 10 tokenů je pod hranicí — přestavět je jednodušší.
        assert_eq!(plan_cache(&predchozi, &novy), CachePlan::Rebuild);
    }

    #[test]
    fn dostatecny_prefix_se_znovu_pouzije() {
        let predchozi: Vec<i32> = (0..500).collect();
        let mut novy = predchozi.clone();
        novy.extend(1000..1050);
        assert_eq!(
            plan_cache(&predchozi, &novy),
            CachePlan::Reuse { keep: 500 }
        );
    }

    #[test]
    fn zmena_na_zacatku_zahodi_vsechno() {
        // Po sloučení kontextu se změní systémový blok — prefix padá celý.
        let predchozi: Vec<i32> = (0..500).collect();
        let mut novy = vec![999];
        novy.extend(1..500);
        assert_eq!(plan_cache(&predchozi, &novy), CachePlan::Rebuild);
    }

    #[test]
    fn plan_urcuje_od_ktere_pozice_se_dekoduje() {
        assert_eq!(CachePlan::Rebuild.start_position(), 0);
        assert_eq!(CachePlan::Reuse { keep: 120 }.start_position(), 120);
    }

    #[test]
    fn typicky_dalsi_tah_znovu_pouzije_skoro_vsechno() {
        // Reálný tvar: v cache je prompt i vygenerovaná odpověď, nový prompt
        // je totéž plus další uživatelský tah.
        let v_cache: Vec<i32> = (0..1200).collect();
        let mut novy = v_cache.clone();
        novy.extend(5000..5060); // nová zpráva a značky šablony

        match plan_cache(&v_cache, &novy) {
            CachePlan::Reuse { keep } => {
                assert_eq!(keep, 1200);
                // Dopočítá se jen 60 tokenů místo 1260.
                assert_eq!(novy.len() - keep, 60);
            }
            CachePlan::Rebuild => panic!("mělo se znovu použít"),
        }
    }
}
