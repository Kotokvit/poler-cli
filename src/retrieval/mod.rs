//! Слой Native Retrieval: точный поиск (grep), резидентный RAM-кэш,
//! чанкинг документов и SIMD-движок Teddy.
//!
//! Порт из poler-engine (src/retrieval) без web-стека:
//! `semantic_bridge` остался в движке — он тянет web-индекс.

pub mod chunk;
pub mod filecache;
pub mod grep;
pub mod teddy;

pub use grep::{grep_run, grep_run_cached, render_text, GrepConfig, GrepMode, GrepOutput, GrepReport};
