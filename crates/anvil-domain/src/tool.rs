//! Nástroje, které si model může vyžádat.
//!
//! Tady je jádro spolehlivosti celé fáze 2. Lokální model s ~3 miliardami
//! aktivních parametrů nástroje volat **umí**, ale plete se: vynechá povinný
//! parametr, pošle číslo jako text, vymyslí si název nástroje, který
//! neexistuje. Kdyby se takové volání provedlo, dopadne to buď pádem, nebo
//! tím horším — nesmyslným výsledkem, který model vezme za fakt.
//!
//! [`ToolSpec::validate`] proto každé volání prověří **dřív**, než se cokoli
//! stane, a při chybě vrátí hlášku napsanou pro model, ne pro člověka:
//! konkrétní, krátkou a s návodem, co opravit. Agentní smyčka ji pošle zpátky
//! a nechá model zkusit to znovu.
//!
//! Celé je to čistá logika nad `serde_json::Value` — testuje se bez modelu.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{DomainError, DomainResult};

/// Typ parametru. Schválně jen tři — složitější schéma malý model
/// spolehlivě nevyplní a každý typ navíc je další způsob, jak se splést.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamKind {
    Text,
    Integer,
    Boolean,
}

impl ParamKind {
    /// Název typu tak, jak ho uvidí model v popisu nástroje.
    pub fn label(self) -> &'static str {
        match self {
            ParamKind::Text => "text",
            ParamKind::Integer => "celé číslo",
            ParamKind::Boolean => "true/false",
        }
    }

    /// Odpovídá hodnota tomuhle typu?
    ///
    /// U čísel je kontrola **záměrně tolerantní k textu**: modely posílají
    /// `"12"` místo `12` natolik často, že odmítnout to by znamenalo pálit
    /// kola na něčem, co jde jednoznačně opravit. Text, který číslo
    /// nepředstavuje, se ale odmítne.
    fn accepts(self, value: &Value) -> bool {
        match self {
            ParamKind::Text => value.is_string(),
            ParamKind::Integer => {
                value.as_i64().is_some()
                    || value
                        .as_str()
                        .is_some_and(|s| s.trim().parse::<i64>().is_ok())
            }
            ParamKind::Boolean => {
                value.is_boolean()
                    || value
                        .as_str()
                        .is_some_and(|s| matches!(s.trim(), "true" | "false"))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolParam {
    pub name: String,
    pub kind: ParamKind,
    pub required: bool,
    /// Popis pro model. Krátká věta, co do parametru patří.
    pub description: String,
}

impl ToolParam {
    pub fn required(name: &str, kind: ParamKind, description: &str) -> Self {
        Self {
            name: name.into(),
            kind,
            required: true,
            description: description.into(),
        }
    }

    pub fn optional(name: &str, kind: ParamKind, description: &str) -> Self {
        Self {
            name: name.into(),
            kind,
            required: false,
            description: description.into(),
        }
    }
}

/// Popis nástroje — co umí a co potřebuje.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    /// K čemu nástroj je. Jde do promptu, takže stručně a konkrétně.
    pub description: String,
    pub params: Vec<ToolParam>,
}

impl ToolSpec {
    pub fn new(name: &str, description: &str, params: Vec<ToolParam>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            params,
        }
    }

    /// Ověří volání proti popisu a vrátí **normalizované** argumenty.
    ///
    /// Normalizace znamená, že `"12"` u celočíselného parametru vyjde jako
    /// `12` — nástroj se pak nemusí zabývat tím, v jakém tvaru to model
    /// poslal.
    ///
    /// Chybová hláška je psaná pro model: říká přesně, co je špatně a co
    /// s tím. Smyčka ji pošle zpátky jako výsledek nástroje.
    pub fn validate(&self, call: &ToolCall) -> Result<Value, String> {
        if call.name != self.name {
            return Err(format!(
                "Nástroj se jmenuje '{}', ne '{}'.",
                self.name, call.name
            ));
        }

        let Some(obj) = call.arguments.as_object() else {
            return Err(format!(
                "Argumenty nástroje '{}' musí být objekt, například {{\"{}\": …}}.",
                self.name,
                self.params
                    .first()
                    .map(|p| p.name.as_str())
                    .unwrap_or("parametr")
            ));
        };

        let mut out = serde_json::Map::new();

        for param in &self.params {
            match obj.get(&param.name) {
                None => {
                    if param.required {
                        return Err(format!(
                            "Chybí povinný parametr '{}' ({}). {}",
                            param.name,
                            param.kind.label(),
                            param.description
                        ));
                    }
                }
                Some(value) if value.is_null() && !param.required => {}
                Some(value) => {
                    if !param.kind.accepts(value) {
                        return Err(format!(
                            "Parametr '{}' má být {}, ale přišlo {}.",
                            param.name,
                            param.kind.label(),
                            describe(value)
                        ));
                    }
                    out.insert(param.name.clone(), normalize(param.kind, value));
                }
            }
        }

        // Neznámý parametr je signál, že si model nástroj domýšlí. Tiše ho
        // zahodit by znamenalo provést něco jiného, než co si model myslí,
        // že provádí.
        let znamé: Vec<&str> = self.params.iter().map(|p| p.name.as_str()).collect();
        if let Some(cizí) = obj.keys().find(|k| !znamé.contains(&k.as_str())) {
            return Err(format!(
                "Nástroj '{}' nezná parametr '{}'. Zná jen: {}.",
                self.name,
                cizí,
                if znamé.is_empty() {
                    "žádný".to_string()
                } else {
                    znamé.join(", ")
                }
            ));
        }

        Ok(Value::Object(out))
    }

    /// Řádek do promptu, kterým se model dozví, že nástroj existuje.
    pub fn prompt_line(&self) -> String {
        let params = self
            .params
            .iter()
            .map(|p| {
                let znacka = if p.required { "" } else { "?" };
                format!("{}{znacka}: {}", p.name, p.kind.label())
            })
            .collect::<Vec<_>>()
            .join(", ");

        format!("- {}({params}) — {}", self.name, self.description)
    }
}

fn describe(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(_) => "true/false".into(),
        Value::Number(_) => "číslo".into(),
        Value::String(s) => format!("text \"{}\"", zkrat(s, 30)),
        Value::Array(_) => "seznam".into(),
        Value::Object(_) => "objekt".into(),
    }
}

fn zkrat(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

fn normalize(kind: ParamKind, value: &Value) -> Value {
    match (kind, value) {
        (ParamKind::Integer, Value::String(s)) => s
            .trim()
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| value.clone()),
        (ParamKind::Boolean, Value::String(s)) => match s.trim() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => value.clone(),
        },
        _ => value.clone(),
    }
}

/// Volání nástroje tak, jak ho model vyslovil.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

impl ToolCall {
    pub fn new(name: impl Into<String>, arguments: Value) -> Self {
        Self {
            name: name.into(),
            arguments,
        }
    }

    /// Rozparsuje JSON tělo volání.
    pub fn parse(raw: &str) -> DomainResult<Self> {
        serde_json::from_str(raw)
            .map_err(|e| DomainError::validation(format!("volání nástroje není platný JSON: {e}")))
    }
}

impl fmt::Display for ToolCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.name, self.arguments)
    }
}

/// Výsledek nástroje, který jde zpátky modelu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub content: String,
    /// Nástroj selhal nebo bylo volání neplatné. Model to má vidět a zkusit
    /// to jinak, ne to považovat za data.
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn read_file() -> ToolSpec {
        ToolSpec::new(
            "read_file",
            "Přečte soubor z projektu.",
            vec![
                ToolParam::required("path", ParamKind::Text, "Cesta relativně ke složce."),
                ToolParam::optional("start_line", ParamKind::Integer, "První řádek."),
            ],
        )
    }

    // --- co má projít ---

    #[test]
    fn platne_volani_projde() {
        let out = read_file()
            .validate(&ToolCall::new("read_file", json!({"path": "src/main.rs"})))
            .unwrap();
        assert_eq!(out["path"], json!("src/main.rs"));
    }

    #[test]
    fn nepovinny_parametr_muze_chybet() {
        assert!(read_file()
            .validate(&ToolCall::new("read_file", json!({"path": "a.rs"})))
            .is_ok());
    }

    #[test]
    fn nepovinny_parametr_smi_byt_null() {
        assert!(read_file()
            .validate(&ToolCall::new(
                "read_file",
                json!({"path": "a.rs", "start_line": null})
            ))
            .is_ok());
    }

    #[test]
    fn cislo_poslane_jako_text_se_srovna() {
        // Modely tohle dělají pořád. Odmítnout to znamená pálit kolo za nic.
        let out = read_file()
            .validate(&ToolCall::new(
                "read_file",
                json!({"path": "a.rs", "start_line": "12"}),
            ))
            .unwrap();
        assert_eq!(out["start_line"], json!(12));
    }

    #[test]
    fn boolean_poslany_jako_text_se_srovna() {
        let spec = ToolSpec::new(
            "t",
            "",
            vec![ToolParam::required("flag", ParamKind::Boolean, "")],
        );
        let out = spec
            .validate(&ToolCall::new("t", json!({"flag": "true"})))
            .unwrap();
        assert_eq!(out["flag"], json!(true));
    }

    // --- co musí spadnout ---

    #[test]
    fn chybejici_povinny_parametr_neprojde() {
        let chyba = read_file()
            .validate(&ToolCall::new("read_file", json!({})))
            .unwrap_err();
        assert!(chyba.contains("path"), "{chyba}");
        // Hláška musí modelu říct, co s tím — ne jen že je něco špatně.
        assert!(chyba.contains("Cesta relativně"), "{chyba}");
    }

    #[test]
    fn spatny_typ_neprojde() {
        let chyba = read_file()
            .validate(&ToolCall::new(
                "read_file",
                json!({"path": "a.rs", "start_line": "úplně začátek"}),
            ))
            .unwrap_err();
        assert!(chyba.contains("start_line"), "{chyba}");
        assert!(chyba.contains("celé číslo"), "{chyba}");
    }

    #[test]
    fn cesta_jako_cislo_neprojde() {
        let chyba = read_file()
            .validate(&ToolCall::new("read_file", json!({"path": 42})))
            .unwrap_err();
        assert!(chyba.contains("path"), "{chyba}");
    }

    #[test]
    fn neznamy_parametr_neprojde() {
        // Tiše ho zahodit by znamenalo provést něco jiného, než co si model
        // myslí, že provádí.
        let chyba = read_file()
            .validate(&ToolCall::new(
                "read_file",
                json!({"path": "a.rs", "encoding": "utf-8"}),
            ))
            .unwrap_err();
        assert!(chyba.contains("encoding"), "{chyba}");
        assert!(
            chyba.contains("path"),
            "hláška má vyjmenovat, co nástroj zná: {chyba}"
        );
    }

    #[test]
    fn jiny_nazev_nastroje_neprojde() {
        let chyba = read_file()
            .validate(&ToolCall::new("read_files", json!({"path": "a.rs"})))
            .unwrap_err();
        assert!(chyba.contains("read_file"), "{chyba}");
    }

    #[test]
    fn argumenty_nejsou_objekt() {
        for spatne in [json!("src/main.rs"), json!(["a"]), json!(42), json!(null)] {
            let chyba = read_file()
                .validate(&ToolCall::new("read_file", spatne.clone()))
                .unwrap_err();
            assert!(chyba.contains("objekt"), "{spatne}: {chyba}");
        }
    }

    #[test]
    fn dlouhy_text_v_hlasce_se_zkrati() {
        // Hláška jde do promptu; nesmí do něj nasypat celý soubor.
        let chyba = read_file()
            .validate(&ToolCall::new(
                "read_file",
                json!({"path": "a.rs", "start_line": "x".repeat(500)}),
            ))
            .unwrap_err();
        assert!(
            chyba.chars().count() < 150,
            "hláška má {} znaků",
            chyba.chars().count()
        );
    }

    // --- parsování a prompt ---

    #[test]
    fn volani_se_rozparsuje_z_json() {
        let c = ToolCall::parse(r#"{"name":"grep","arguments":{"pattern":"unwrap"}}"#).unwrap();
        assert_eq!(c.name, "grep");
        assert_eq!(c.arguments["pattern"], json!("unwrap"));
    }

    #[test]
    fn rozbity_json_da_srozumitelnou_chybu() {
        let chyba = ToolCall::parse(r#"{"name":"grep",}"#)
            .unwrap_err()
            .to_string();
        assert!(chyba.contains("není platný JSON"), "{chyba}");
    }

    #[test]
    fn volani_bez_argumentu_je_v_poradku() {
        // Nástroj bez parametrů se volá jako {"name":"..."} — chybějící
        // `arguments` nesmí být chyba parsování.
        let c = ToolCall::parse(r#"{"name":"list_files"}"#).unwrap();
        assert_eq!(c.arguments, Value::Null);
    }

    #[test]
    fn radek_do_promptu_obsahuje_vse_podstatne() {
        let radek = read_file().prompt_line();
        assert!(radek.contains("read_file"), "{radek}");
        assert!(radek.contains("path"), "{radek}");
        // Nepovinný parametr je označený, ať si ho model nevynucuje.
        assert!(radek.contains("start_line?"), "{radek}");
        assert!(radek.contains("Přečte soubor"), "{radek}");
    }

    #[test]
    fn vysledek_nese_priznak_chyby() {
        assert!(!ToolResult::ok("obsah").is_error);
        assert!(ToolResult::error("nenalezeno").is_error);
    }
}
