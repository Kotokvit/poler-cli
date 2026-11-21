//! poler — Sovereign CLI Suite: Harvester, Grep, Zero-Copy Piece-Table Editor & AI-Agent MCP Engine
//!
//! Информационный стек движка poler-engine (порт 2026-11: archive,
//! retrieval, parser, intent, tokenizer, resonance, compression,
//! streaming, binfmt, psi, poler) собран здесь без опасных модулей
//! (calc / crypto / exec / shell остались в закрытом движке).

pub mod aidde;
pub mod archive;
pub mod asm;
pub mod asm_compat;
pub mod binfmt;
pub mod calc;
pub mod compression;
pub mod core;
pub mod engine;
pub mod graph;
pub mod output;
pub mod harvest;
pub mod intent;
pub mod mcp;
pub mod parser;
pub mod poler;
pub mod psi;
pub mod resonance;
pub mod retrieval;
pub mod streaming;
pub mod tokenizer;

pub use crate::core::{
    agent_ops, buffer, crystal, engine_gate, event_plane, extractor, fastcdc, fsst, license,
    pdf_edit, protocol, scratch, search, trit_silicon, trit_syntax, universal_letters, vfs,
    vfs_edit,
};
pub use output::{render_markdown, render_simple, ContextAnchor, SearchResult};
pub use tokenizer::{InvertedIndex, PiiCleaner, PiiMode};

/// Мусорные каталоги, пропускаемые обходом даже без .gitignore
/// (как SKIP_DIRS в корне poler-engine).
pub const SKIP_DIRS: &[&str] =
    &[".git", "target", "node_modules", ".svn", ".hg", "__pycache__"];

/// Расширения, сканируемые по умолчанию (как DEFAULT_EXTENSIONS в
/// poler-engine v0.84.0: +zig/s/asm/lua — низкоуровневые ядра poler-os,
/// +hh/hxx/cxx/inc/inl — современный C++). Матчинг регистронезависим.
pub const DEFAULT_EXTENSIONS: &str =
    "md,markdown,txt,rst,rs,py,c,h,cpp,hpp,cc,hh,hxx,cxx,inc,inl,js,jsx,ts,tsx,java,go,kt,swift,cs,scala,dart,zig,s,asm,lua,json,toml,yaml,yml,sql,sh";

/// Режим накопления резонанса (порт из poler-engine).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResonanceMode {
    /// IIR по последовательности совпадений (ε окна каждого хита).
    Hits,
    /// Поле резонанса по всему документу, строго O(N), семплы на хитах.
    Field,
    /// POLER[Ψ]: уравнение внимания из POLER_Psi_v3.py (Kotokvit).
    Psi,
    /// Канонический POLER-цикл из P3_Engine (p3_poler.zig, Kotokvit):
    /// `p_new = p − η·Π_Λ(D·p + γ·J·p + ∇F)`.
    Poler,
}

/// Конфигурация движка (порт из poler-engine).
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Радиус окна в токенах (полуокно).
    pub window_radius: usize,
    /// Коэффициент затухания IIR-резонанса φ ∈ [0.75, 0.90].
    pub phi_decay: f64,
    /// Масштабный коэффициент калибровки ε.
    pub kappa: f64,
    /// Число возвращаемых якорей.
    pub top_n: usize,
    /// Глубина K-hop обхода графа.
    pub k_hop_depth: usize,
    /// Режим очистки PII.
    pub pii_mode: PiiMode,
    /// Режим резонанса.
    pub resonance_mode: ResonanceMode,
    /// Temporal-фильтр (например, `Т-23`).
    pub temporal_filter: Option<String>,
    /// Считать ε по статистикам файла, а не корпуса.
    pub local_stats: bool,
    /// Сканируемые расширения (lowercase, без точек).
    pub extensions: Vec<String>,
    /// Максимальный размер файла в байтах.
    pub max_file_bytes: u64,
    /// Максимальная длина enclosing_scope в байтах.
    pub max_scope_bytes: usize,
    /// Максимум K-hop отношений на якорь.
    pub max_relations: usize,
    /// Показывать скрытые файлы (rg --hidden).
    pub include_hidden: bool,
    /// Дамп графа сущностей в SQL-файл (схема super-z memory_graph).
    pub graph_export: Option<std::path::PathBuf>,
    /// Бюджет рёбер графа сущностей (защита от неограниченного роста).
    pub max_graph_triples: usize,
    /// Гиперпараметры POLER[Ψ] (η, γ, ρ, K) — режим резонанса Psi.
    pub psi_params: crate::psi::PsiParams,
    /// Гиперпараметры канонического POLER-цикла (P3_Engine) — режим Poler.
    pub poler_params: crate::poler::PolerParams,
    /// Режим intent-детекта запроса: код ↔ проза (v0.82.0).
    pub intent_mode: crate::intent::IntentMode,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            window_radius: 40,
            phi_decay: 0.85,
            kappa: 1.0,
            top_n: 10,
            k_hop_depth: 2,
            pii_mode: PiiMode::Mask,
            resonance_mode: ResonanceMode::Hits,
            temporal_filter: None,
            local_stats: false,
            extensions: DEFAULT_EXTENSIONS
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .collect(),
            max_file_bytes: 64 * 1024 * 1024,
            max_scope_bytes: 16 * 1024,
            max_relations: 64,
            include_hidden: false,
            graph_export: None,
            max_graph_triples: 200_000,
            psi_params: crate::psi::PsiParams::default(),
            poler_params: crate::poler::PolerParams::default(),
            intent_mode: crate::intent::IntentMode::Auto,
        }
    }
}

/// Статистика прогона (для `--verbose`; порт из poler-engine).
#[derive(Debug, Clone, Default)]
pub struct ScanStats {
    pub files_scanned: usize,
    pub files_with_hits: usize,
    pub total_tokens: u64,
    pub total_hits: usize,
    pub graph_nodes: usize,
    pub graph_edges: usize,
    pub elapsed_ms: u128,
}

/// Открывает файл через mmap и вызывает `f(text)` (порт из poler-engine).
/// Невалидный UTF-8 конвертируется lossy; пустые файлы дают `""`.
/// Подсказка ядру `madvise(SEQUENTIAL)` — чтение предсказуемо
/// последовательное, ядро включает readahead.
pub(crate) fn with_text<T>(path: &std::path::Path, max_bytes: u64, f: impl FnOnce(&str) -> T) -> Option<T> {
    use std::fs::File;
    let file = File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > max_bytes {
        return None;
    }
    if meta.len() == 0 {
        return Some(f(""));
    }
    let mmap = unsafe { memmap2::Mmap::map(&file) }.ok()?;
    #[cfg(unix)]
    let _ = mmap.advise(memmap2::Advice::Sequential);
    let bytes = &mmap[..];
    let lossy;
    let text = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => {
            lossy = String::from_utf8_lossy(bytes).into_owned();
            lossy.as_str()
        }
    };
    Some(f(text))
}

/// Сбор файлов: WalkBuilder из крейта `ignore` (ripgrep) — .gitignore,
/// скрытые файлы, фильтр нежелательных директорий (порт из poler-engine).
pub fn collect_files(target: &std::path::Path, config: &EngineConfig) -> Vec<std::path::PathBuf> {
    use ignore::WalkBuilder;
    if target.is_file() {
        return vec![target.to_path_buf()];
    }
    let exts: Vec<String> = config.extensions.clone();
    WalkBuilder::new(target)
        .hidden(!config.include_hidden)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .filter_entry(move |e| {
            e.file_type().map_or(true, |t| !t.is_dir())
                || !SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref())
        })
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| exts.iter().any(|x| x.eq_ignore_ascii_case(s)))
                .unwrap_or(false)
        })
        .map(|e| e.into_path())
        .collect()
}
