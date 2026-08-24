//! Infrastrukturní vrstva Anvilu.
//!
//! Konkrétní implementace portů z `anvil-domain`: lokální model přes
//! llama.cpp, stahování z HuggingFace, systémové úložiště tajemství
//! a nastavení na disku.
//!
//! Všechno, co potřebuje llama.cpp, je za feature `engine` — bez ní se
//! zbytek přeloží a otestuje kdekoli.

pub mod ai;
pub mod huggingface;
pub mod model_provisioner;
pub mod paths;
pub mod secrets;
pub mod settings_store;
