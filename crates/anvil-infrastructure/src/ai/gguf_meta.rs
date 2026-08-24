//! Minimální čtečka GGUF hlavičky — bez načítání vah zjistí
//! `general.architecture` (llama / mistral3 / qwen3moe / gemma4 / …)
//! a pár číselných parametrů potřebných pro plánování offloadu
//! (počet expertů = MoE ano/ne, počet vrstev, rozměry pro odhad KV cache).
//!
//! Používá se pro plánování offloadu a pro srozumitelnou chybovou hlášku
//! u poškozených nebo cizích souborů dřív,
//! než by se soubor předal enginu (který na nekompatibilním souboru
//! umí spadnout bez chyby, viz zaseknuté načítání Gemmy 4).
//!
//! Formát (little-endian): magic "GGUF", u32 verze (2/3), u64 tensor_count,
//! u64 kv_count, pak KV páry: klíč (u64 délka + UTF-8), u32 typ hodnoty,
//! hodnota. Čteme jen metadata sekci, váhy se nikdy nedotkneme.

use std::{
    fs::File,
    io::{BufReader, Read},
    path::Path,
};

/// Sanity strop pro délky stringů/polí v metadatech — chrání před absurdní
/// alokací u poškozeného souboru (běžné klíče mají desítky bajtů).
const MAX_META_STRING: u64 = 16 * 1024 * 1024;

/// Hodnotové typy GGUF metadat (spec ggml/gguf.md).
const T_U8: u32 = 0;
const T_I8: u32 = 1;
const T_U16: u32 = 2;
const T_I16: u32 = 3;
const T_U32: u32 = 4;
const T_I32: u32 = 5;
const T_F32: u32 = 6;
const T_BOOL: u32 = 7;
const T_STRING: u32 = 8;
const T_ARRAY: u32 = 9;
const T_U64: u32 = 10;
const T_I64: u32 = 11;
const T_F64: u32 = 12;

/// Číselné klíče, které nás zajímají. Prefix je název architektury
/// (`gemma4.block_count`, `qwen3moe.expert_count`, …), takže je hledáme
/// podle přípony — pořadí klíčů v hlavičce není specifikací dané.
const NUMERIC_SUFFIXES: &[&str] = &[
    ".block_count",
    ".expert_count",
    ".embedding_length",
    ".attention.head_count",
    ".attention.head_count_kv",
    ".attention.key_length",
    ".attention.value_length",
];

/// Co z hlavičky potřebujeme k routingu a plánování offloadu.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GgufInfo {
    pub architecture: String,
    /// Počet expertů. `Some(n)` s `n > 1` = MoE model — u něj se vyplatí
    /// nechat experty v RAM a na GPU dát jen attention (viz `offload_plan`).
    pub expert_count: Option<u32>,
    pub block_count: Option<u32>,
    pub embedding_length: Option<u32>,
    pub head_count: Option<u32>,
    pub head_count_kv: Option<u32>,
    /// Rozměr K/V hlavy, pokud ho model uvádí explicitně (Gemma ano —
    /// není to `embedding_length / head_count`).
    pub key_length: Option<u32>,
    pub value_length: Option<u32>,
}

impl GgufInfo {
    /// MoE = víc než jeden expert. Modely s `expert_count = 0/1` jsou husté.
    pub fn is_moe(&self) -> bool {
        self.expert_count.is_some_and(|n| n > 1)
    }
}

/// Přečte `general.architecture` z GGUF souboru. Chyby vrací jako český
/// popis vhodný k zobrazení uživateli.
pub fn read_gguf_architecture(path: &Path) -> Result<String, String> {
    Ok(read_gguf_info(path)?.architecture)
}

/// Přečte architekturu + číselné parametry modelu z GGUF hlavičky.
/// Chybějící číselné klíče nejsou chyba (starší/exotické konvertory je
/// nemusí uvádět) — volající si poradí odhadem.
pub fn read_gguf_info(path: &Path) -> Result<GgufInfo, String> {
    let file =
        File::open(path).map_err(|e| format!("Soubor {} nejde otevřít: {e}", path.display()))?;
    let mut r = BufReader::new(file);

    let mut magic = [0u8; 4];
    read_exact(&mut r, &mut magic)?;
    if &magic != b"GGUF" {
        return Err("Soubor není GGUF (chybí magic hlavička).".into());
    }

    let version = read_u32(&mut r)?;
    if !(2..=3).contains(&version) {
        return Err(format!(
            "Nepodporovaná verze GGUF hlavičky: {version} (podporované: 2, 3)."
        ));
    }

    let _tensor_count = read_u64(&mut r)?;
    let kv_count = read_u64(&mut r)?;

    let mut architecture: Option<String> = None;
    let mut numbers: Vec<(String, u32)> = Vec::new();

    for _ in 0..kv_count {
        let key = read_string(&mut r)?;
        let value_type = read_u32(&mut r)?;

        if key == "general.architecture" {
            if value_type != T_STRING {
                return Err(format!(
                    "general.architecture má nečekaný typ {value_type} (čekán string)."
                ));
            }
            architecture = Some(read_string(&mut r)?);
            continue;
        }

        if NUMERIC_SUFFIXES.iter().any(|s| key.ends_with(s)) {
            // Některé modely mají hodnoty per-vrstvu jako pole — ty
            // přeskočíme, odhad si poradí i bez nich.
            if let Some(value) = read_scalar_u32(&mut r, value_type)? {
                numbers.push((key, value));
            }
            continue;
        }

        skip_value(&mut r, value_type)?;
    }

    let architecture =
        architecture.ok_or("GGUF metadata neobsahují general.architecture.".to_string())?;

    let pick = |suffix: &str| -> Option<u32> {
        numbers
            .iter()
            .find(|(k, _)| k.ends_with(suffix))
            .map(|(_, v)| *v)
    };

    Ok(GgufInfo {
        expert_count: pick(".expert_count"),
        block_count: pick(".block_count"),
        embedding_length: pick(".embedding_length"),
        head_count: pick(".attention.head_count"),
        head_count_kv: pick(".attention.head_count_kv"),
        key_length: pick(".attention.key_length"),
        value_length: pick(".attention.value_length"),
        architecture,
    })
}

/// Přečte celočíselnou hodnotu, pokud je skalární; jinak ji přeskočí
/// a vrátí `None`.
fn read_scalar_u32(r: &mut impl Read, value_type: u32) -> Result<Option<u32>, String> {
    let value = match value_type {
        T_U8 | T_I8 => {
            let mut b = [0u8; 1];
            read_exact(r, &mut b)?;
            b[0] as u32
        }
        T_U16 | T_I16 => {
            let mut b = [0u8; 2];
            read_exact(r, &mut b)?;
            u16::from_le_bytes(b) as u32
        }
        T_U32 | T_I32 => read_u32(r)?,
        T_U64 | T_I64 => {
            let v = read_u64(r)?;
            u32::try_from(v).unwrap_or(u32::MAX)
        }
        other => {
            skip_value(r, other)?;
            return Ok(None);
        }
    };
    Ok(Some(value))
}

// ---------- primitivní čtení (little-endian) --------------------------------

fn read_exact(r: &mut impl Read, buf: &mut [u8]) -> Result<(), String> {
    r.read_exact(buf)
        .map_err(|e| format!("GGUF hlavička je useknutá: {e}"))
}

fn read_u32(r: &mut impl Read) -> Result<u32, String> {
    let mut b = [0u8; 4];
    read_exact(r, &mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut impl Read) -> Result<u64, String> {
    let mut b = [0u8; 8];
    read_exact(r, &mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_string(r: &mut impl Read) -> Result<String, String> {
    let len = read_u64(r)?;
    if len > MAX_META_STRING {
        return Err(format!("GGUF metadata: string délky {len} B je podezřelý."));
    }
    let mut buf = vec![0u8; len as usize];
    read_exact(r, &mut buf)?;
    String::from_utf8(buf).map_err(|e| format!("GGUF metadata: neplatné UTF-8: {e}"))
}

fn skip_bytes(r: &mut impl Read, n: u64) -> Result<(), String> {
    let copied = std::io::copy(&mut r.take(n), &mut std::io::sink())
        .map_err(|e| format!("GGUF hlavička je useknutá: {e}"))?;
    if copied != n {
        return Err("GGUF hlavička je useknutá (nečekaný konec souboru).".into());
    }
    Ok(())
}

/// Přeskočí hodnotu daného typu (včetně vnořených polí).
fn skip_value(r: &mut impl Read, value_type: u32) -> Result<(), String> {
    match value_type {
        T_U8 | T_I8 | T_BOOL => skip_bytes(r, 1),
        T_U16 | T_I16 => skip_bytes(r, 2),
        T_U32 | T_I32 | T_F32 => skip_bytes(r, 4),
        T_U64 | T_I64 | T_F64 => skip_bytes(r, 8),
        T_STRING => {
            let len = read_u64(r)?;
            if len > MAX_META_STRING {
                return Err(format!("GGUF metadata: string délky {len} B je podezřelý."));
            }
            skip_bytes(r, len)
        }
        T_ARRAY => {
            let elem_type = read_u32(r)?;
            let count = read_u64(r)?;
            // Fixní typy přeskočíme jedním seekem, stringy/pole po prvcích.
            match elem_type {
                T_U8 | T_I8 | T_BOOL => skip_bytes(r, count),
                T_U16 | T_I16 => skip_bytes(r, count.saturating_mul(2)),
                T_U32 | T_I32 | T_F32 => skip_bytes(r, count.saturating_mul(4)),
                T_U64 | T_I64 | T_F64 => skip_bytes(r, count.saturating_mul(8)),
                _ => {
                    for _ in 0..count {
                        skip_value(r, elem_type)?;
                    }
                    Ok(())
                }
            }
        }
        other => Err(format!("GGUF metadata: neznámý typ hodnoty {other}.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_string(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u64).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }

    /// Sestaví minimální syntetický GGUF: pár KV před general.architecture,
    /// aby se procvičilo přeskakování typů (u32, string, pole stringů).
    fn synthetic_gguf(architecture: Option<&str>) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"GGUF");
        out.extend_from_slice(&3u32.to_le_bytes()); // verze
        out.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
        let kv_count: u64 = if architecture.is_some() { 4 } else { 3 };
        out.extend_from_slice(&kv_count.to_le_bytes());

        // 1) u32 hodnota
        write_string(&mut out, "general.quantization_version");
        out.extend_from_slice(&T_U32.to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());

        // 2) string hodnota
        write_string(&mut out, "general.name");
        out.extend_from_slice(&T_STRING.to_le_bytes());
        write_string(&mut out, "Test Model");

        // 3) pole stringů
        write_string(&mut out, "tokenizer.ggml.tokens");
        out.extend_from_slice(&T_ARRAY.to_le_bytes());
        out.extend_from_slice(&T_STRING.to_le_bytes());
        out.extend_from_slice(&2u64.to_le_bytes());
        write_string(&mut out, "<s>");
        write_string(&mut out, "</s>");

        // 4) hledaný klíč
        if let Some(arch) = architecture {
            write_string(&mut out, "general.architecture");
            out.extend_from_slice(&T_STRING.to_le_bytes());
            write_string(&mut out, arch);
        }
        out
    }

    fn write_temp(bytes: &[u8]) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("anvil-gguf-test-{}.gguf", uuid::Uuid::new_v4()));
        let mut f = File::create(&path).expect("temp file");
        f.write_all(bytes).expect("write");
        path
    }

    #[test]
    fn reads_architecture_after_skipping_other_kv_types() {
        let path = write_temp(&synthetic_gguf(Some("gemma4")));
        let arch = read_gguf_architecture(&path).expect("architektura");
        assert_eq!(arch, "gemma4");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn rejects_non_gguf_magic() {
        let path = write_temp(b"NOPE1234");
        let err = read_gguf_architecture(&path).unwrap_err();
        assert!(err.contains("magic"), "{err}");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn missing_architecture_key_is_error() {
        let path = write_temp(&synthetic_gguf(None));
        let err = read_gguf_architecture(&path).unwrap_err();
        assert!(err.contains("general.architecture"), "{err}");
        std::fs::remove_file(path).ok();
    }

    /// GGUF s číselnými klíči modelu — mezi nimi jeden jako pole
    /// (per-vrstvu), aby se ověřilo, že ho čtečka jen přeskočí.
    fn moe_gguf() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"GGUF");
        out.extend_from_slice(&3u32.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&5u64.to_le_bytes());

        write_string(&mut out, "general.architecture");
        out.extend_from_slice(&T_STRING.to_le_bytes());
        write_string(&mut out, "gemma4");

        write_string(&mut out, "gemma4.expert_count");
        out.extend_from_slice(&T_U32.to_le_bytes());
        out.extend_from_slice(&128u32.to_le_bytes());

        write_string(&mut out, "gemma4.block_count");
        out.extend_from_slice(&T_U32.to_le_bytes());
        out.extend_from_slice(&48u32.to_le_bytes());

        write_string(&mut out, "gemma4.attention.head_count_kv");
        out.extend_from_slice(&T_U32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());

        // Pole místo skaláru — musí se přeskočit bez chyby.
        write_string(&mut out, "gemma4.attention.head_count");
        out.extend_from_slice(&T_ARRAY.to_le_bytes());
        out.extend_from_slice(&T_U32.to_le_bytes());
        out.extend_from_slice(&2u64.to_le_bytes());
        out.extend_from_slice(&8u32.to_le_bytes());
        out.extend_from_slice(&8u32.to_le_bytes());

        out
    }

    #[test]
    fn reads_moe_parameters() {
        let path = write_temp(&moe_gguf());
        let info = read_gguf_info(&path).expect("info");
        assert_eq!(info.architecture, "gemma4");
        assert_eq!(info.expert_count, Some(128));
        assert_eq!(info.block_count, Some(48));
        assert_eq!(info.head_count_kv, Some(4));
        assert_eq!(info.head_count, None, "pole se má přeskočit");
        assert!(info.is_moe());
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn dense_model_is_not_moe() {
        let path = write_temp(&synthetic_gguf(Some("llama")));
        let info = read_gguf_info(&path).expect("info");
        assert_eq!(info.expert_count, None);
        assert!(!info.is_moe());
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn truncated_header_is_error_not_panic() {
        let full = synthetic_gguf(Some("llama"));
        let path = write_temp(&full[..40]);
        assert!(read_gguf_architecture(&path).is_err());
        std::fs::remove_file(path).ok();
    }
}
