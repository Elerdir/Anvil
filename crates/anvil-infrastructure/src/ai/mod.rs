//! Práce s lokálním jazykovým modelem.
//!
//! Modul je rozdělený na dvě části podle toho, co k překladu potřebují:
//!
//! * **Čistá logika** — plán offloadu, parser GGUF hlavičky, rozvrh úseků
//!   při stahování, skládání promptů. Nepotřebuje llama.cpp, přeloží se
//!   kdekoli a je celá pokrytá testy, které jdou pustit i v CI bez CMake.
//! * **Engine** za feature `engine` — vlastní llama.cpp. Jeho build vyžaduje
//!   CMake a (pro GPU) Vulkan SDK nebo Metal, takže je záměrně volitelný.
//!   Bez něj se přeloží a otestuje všechno ostatní.
//!
//! Tohle dělení není kosmetika: kdyby na llama.cpp viselo všechno, nešlo by
//! pustit `cargo test` bez pětiminutového buildu a bez připraveného
//! prostředí — a testy, které se nepouštějí, nikoho nechrání.

pub mod chat_template;
pub mod chunk_plan;
pub mod gemma;
pub mod gguf_meta;
pub mod kv_reuse;
pub mod model_catalog;
pub mod model_downloader;
pub mod offload_plan;

#[cfg(feature = "engine")]
pub mod device_catalog;
#[cfg(feature = "engine")]
pub mod llama_engine;
