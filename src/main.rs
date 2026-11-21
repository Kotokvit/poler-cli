//! poler — Sovereign Ultra-Fast Harvester, Grep, Zero-Copy Piece-Table Editor & AI MCP Engine

use clap::{Parser, ValueEnum};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use serde_json::json;

use poler_cli::harvest::{
    run_harvest, ColorChoice, HarvestConfig, HarvestFormat, HarvestMode,
};
use poler_cli::retrieval::grep::{
    grep_run, render_text, GrepConfig, GrepMode, GrepOutput,
};

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatArg {
    Address,
    Json,
    Markdown,
    Corpus,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeArg {
    Sections,
    Files,
}

#[derive(Parser, Debug)]
#[command(
    name = "poler harvest",
    about = "Sovereign Ultra-Fast Filesystem Harvester & L1-L3 Address Crystal Scanner"
)]
struct HarvestCli {
    #[arg(value_name = "ROOT")]
    root: Option<PathBuf>,

    #[arg(value_name = "QUERY")]
    query: Option<String>,

    #[arg(value_name = "FORMAT")]
    format_pos: Option<String>,

    #[arg(short = 'q', long = "query")]
    query_flag: Option<String>,

    #[arg(short = 'd', long = "disk")]
    roots_flag: Vec<PathBuf>,

    #[arg(short = 'f', long = "format", value_enum)]
    format_flag: Option<FormatArg>,

    #[arg(short = 'm', long = "mode", value_enum, default_value = "sections")]
    mode: ModeArg,

    #[arg(short = 'r', long = "regex")]
    regex: bool,

    #[arg(short = 'c', long = "context", default_value_t = 0)]
    context: usize,

    #[arg(short = 'o', long = "out")]
    out: Option<PathBuf>,

    #[arg(short = 'H', long = "hidden")]
    hidden: bool,

    #[arg(long = "no-ignore")]
    no_ignore: bool,

    #[arg(long = "no-cache")]
    no_cache: bool,

    #[arg(short = 'j', long = "threads", default_value_t = 0)]
    threads: usize,

    #[arg(long = "max-files", default_value_t = 20_000)]
    max_files: usize,

    #[arg(long = "first-n")]
    first_n: bool,

    #[arg(long = "min-matches", default_value_t = 1)]
    min_matches: usize,

    #[arg(long = "max-file-bytes", default_value_t = 500 * 1024 * 1024)]
    max_file_bytes: u64,

    #[arg(long = "max-out-bytes", default_value_t = 256 * 1024 * 1024)]
    max_out_bytes: u64,

    #[arg(long = "color", value_enum, default_value = "auto")]
    color: ColorChoice,
}

#[derive(Parser, Debug)]
#[command(
    name = "poler grep",
    about = "Fast streaming project grep with .gitignore awareness and parallel Rayon scan"
)]
struct GrepCli {
    #[arg(value_name = "PATTERN")]
    pattern: String,

    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,

    #[arg(short = 'i', long = "ignore-case")]
    ignore_case: bool,

    #[arg(short = 'r', long = "regex")]
    regex: bool,

    #[arg(short = 'A', long = "after-context", default_value_t = 0)]
    after: usize,

    #[arg(short = 'B', long = "before-context", default_value_t = 0)]
    before: usize,

    #[arg(short = 'C', long = "context", default_value_t = 0)]
    context: usize,

    #[arg(short = 'c', long = "count")]
    count: bool,

    #[arg(short = 'l', long = "files-with-matches")]
    list_matching: bool,

    #[arg(short = 'L', long = "files-without-match")]
    list_non_matching: bool,

    #[arg(short = 'H', long = "hidden")]
    hidden: bool,

    #[arg(long = "no-ignore")]
    no_ignore: bool,

    /// Скан архивов без распаковки: zip/tar/tar.gz/tar.zst/gz/zst
    /// раскрываются виртуальными файлами «архив::запись».
    #[arg(long = "archives")]
    archives: bool,

    /// Пароль зашифрованных zip-записей (ZipCrypto/AES).
    /// Резолв: --archive-password → POLER_ARCHIVE_KEY.
    #[arg(long = "archive-password")]
    archive_password: Option<String>,

    /// Останов после N совпавших строк на файл (grep -m).
    /// Жук №1 полевого заезда: прежний дефолт 50 000 молча обрезал
    /// большие логи (375k совпадений → 50k, rc=0). Дефолт поднят до
    /// 1 000 000; при фактической обрезке печатается предупреждение
    /// в stderr и выставляется stats.truncated в --json.
    #[arg(long = "max-results", default_value_t = 1_000_000)]
    max_results: usize,

    #[arg(long = "json")]
    json: bool,
}

fn print_help() {
    println!(
        "poler {} — Sovereign Ultra-Fast Harvester, Grep, Zero-Copy Piece-Table & AI MCP Engine\n\n\
         USAGE:\n  \
           poler <COMMAND> [ARGS...]\n\n\
         CORE COMMANDS:\n  \
           poler harvest <ROOT> \"<QUERY>\" [flags]   Full-disk harvest with L1-L3 cache (.t5a) & ANSI highlighting\n  \
           poler grep \"<PATTERN>\" [PATH] [flags]    Streaming project grep with .gitignore awareness\n  \
           poler scan <PATH> -q \"<QUERY>\" [flags]   Topographic resonance search: full scenes, eps-density, R(t), K-hop graph (ai-json|md|simple)\n  \
           poler edit [PATH]                       Launch stdio JSON-RPC editor core for IDEs and agents\n  \
           poler view <PATH> [--lines A..B] [--json] Instant viewer for text, 40GB logs, DOCX, EPUB, PDF\n  \
           poler patch <PATH> <OLD> <NEW> [flags]  Atomic surgical replacement (--dry-run, --diff, --json, --entry)\n  \
           poler diff <PATH>                       Show diff of unsaved Piece-Table modifications\n  \
           poler mcp                               Unified stdio MCP server for Claude/Cursor/Antigravity\n  \
           poler schema                            Self-documenting JSON manifest of all tools\n\n\
         ALIASES:\n  \
           poler-harvest ...                       Direct alias for 'harvest' subcommand\n  \
           poler-edit-core ...                     Direct alias for 'edit' subcommand\n",
        env!("CARGO_PKG_VERSION")
    );
}

fn print_editor_schema() {
    let schema = json!({
        "engine": "poler-edit-core",
        "version": env!("CARGO_PKG_VERSION"),
        "flag": "--schema",
        "purpose": "Самодокументирующийся манифест Zero-Copy Piece-Table & VFS редактора для LLM-агентов",
        "auto_doc_resolution": "Команды search, read_span, insert_at, delete, replace_match, piece_diff, saccade, scroll, seek_energy, save автоматически разрешают doc=1, если открыт один документ или передан 'path'. В viewport и close указывайте 'doc': 1 явно.",
        "command_groups": {
            "lifecycle": ["open", "list_docs", "close", "stats", "reload", "save", "save_as", "tx_save_all", "undo", "redo"],
            "navigation_and_ai_eyes": ["viewport", "read_span", "goto", "linecol", "outline", "breadcrumbs", "saccade", "scroll", "seek_energy", "search", "search_live", "semantic_search", "project_impact", "index"],
            "piece_table_surgery": ["replace_match", "insert_at", "delete", "append", "stream", "apply_batch", "apply_diff", "piece_diff", "fork_doc", "merge_fork", "verify_syntax"],
            "vfs_archive_surgery_docx_epub_odt_zip": ["vfs_list", "vfs_read", "vfs_open", "vfs_save", "vfs_write", "vfs_delete", "vfs_dirty", "vfs_flush", "vfs_discard"],
            "pdf_surgery": ["pdf_replace", "pdf_save", "extract_doc"],
            "scratchpad_and_crystal": ["scratch_put", "scratch_get", "scratch_list", "scratch_discard", "scratch_apply", "crystal_note", "crystal_assoc", "crystal_stats", "crystal_load", "crystal_save"],
            "engine_integration": ["silicon_info", "symbol_bench", "engine_status", "engine_search", "engine_grep", "engine_pack", "engine_squeeze", "engine_archive_ls", "engine_impact", "engine_crystal_ingest"]
        },
        "invariants": {
            "search_field": "В команде 'search' поисковая строка передается в поле 'query' (или 'pattern'); возвращаются col (символы UTF-8) и col_bytes (байты).",
            "insert_at_col": "В 'insert_at' поле 'col' — символьный индекс UTF-8 (0-based, по умолчанию 0).",
            "docx_epub_odt_zip_guard": "На контейнерах .docx/.epub/.odt/.zip/.fb2 команда 'open' извлекает только Markdown-представление для чтения; обычный 'save' заблокирован защитой от порчи ZIP. Для правки используйте двухфазный CoW VFS: vfs_open -> replace_match -> vfs_save -> vfs_flush."
        },
        "quickstart_stdio": [
            "printf '%s\\n' '{\"id\":1,\"cmd\":\"open\",\"path\":\"file.md\"}' '{\"id\":2,\"cmd\":\"replace_match\",\"old_text\":\"старый\",\"new_text\":\"новый\",\"check_syntax\":true}' '{\"id\":3,\"cmd\":\"piece_diff\"}' '{\"id\":4,\"cmd\":\"save\"}' | poler-edit-core",
            "printf '%s\\n' '{\"id\":1,\"cmd\":\"vfs_list\",\"path\":\"doc.docx\"}' '{\"id\":2,\"cmd\":\"vfs_open\",\"path\":\"doc.docx\",\"entry\":\"word/document.xml\"}' '{\"id\":3,\"cmd\":\"replace_match\",\"doc\":1,\"old_text\":\"А\",\"new_text\":\"Б\"}' '{\"id\":4,\"cmd\":\"vfs_save\",\"doc\":1}' '{\"id\":5,\"cmd\":\"vfs_flush\",\"path\":\"doc.docx\"}' | poler-edit-core"
        ]
    });
    println!("{}", serde_json::to_string_pretty(&schema).unwrap_or_default());
}

fn print_schema() {
    let schema = json!({
        "engine": "poler",
        "version": env!("CARGO_PKG_VERSION"),
        "subcommands": {
            "harvest": "Полнодисковый сбор файлов и кода с L1-L3 адресацией (.t5a)",
            "grep": "Параллельный потоковый grep по кодовой базе с учётом .gitignore",
            "scan": "Топографический резонансный поиск (порт poler-engine): полные сцены, ε-плотность, R(t), K-hop граф, intent код↔проза; --format ai-json|md|simple, --resonance-mode hits|field|psi|poler",
            "edit": "Zero-Copy Piece-Table редакторский сервер (stdio JSON-RPC)",
            "view": "Быстрый просмотр любых файлов (включая 40 ГБ логи, DOCX, EPUB, PDF)",
            "patch": "Точечная атомарная замена подстроки в файле или внутри архива",
            "diff": "Unified diff правок в буфере",
            "mcp": "Единый Model Context Protocol сервер (stdio) с 39 инструментами"
        }
    });
    println!("{}", serde_json::to_string_pretty(&schema).unwrap());
}

fn parse_format(pos: Option<&str>, flag: Option<FormatArg>) -> HarvestFormat {
    if let Some(f) = flag {
        return match f {
            FormatArg::Address => HarvestFormat::Address,
            FormatArg::Json => HarvestFormat::Json,
            FormatArg::Markdown => HarvestFormat::Markdown,
            FormatArg::Corpus => HarvestFormat::Corpus,
        };
    }
    if let Some(p) = pos {
        match p.to_lowercase().as_str() {
            "json" => HarvestFormat::Json,
            "markdown" | "md" => HarvestFormat::Markdown,
            "corpus" => HarvestFormat::Corpus,
            _ => HarvestFormat::Address,
        }
    } else {
        HarvestFormat::Address
    }
}

fn run_harvest_cli(args: &[String]) -> ExitCode {
    let mut clap_args = vec!["poler-harvest".to_string()];
    clap_args.extend_from_slice(args);
    let cli = match HarvestCli::try_parse_from(clap_args) {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(2);
        }
    };

    let mut roots = cli.roots_flag;
    if roots.is_empty() {
        if let Some(r) = cli.root {
            roots.push(r);
        }
    }
    let query = cli.query_flag.or(cli.query);
    let Some(query) = query else {
        eprintln!("poler harvest: error: missing search query (QUERY)");
        eprintln!("Usage: poler harvest <ROOT_DIR> <QUERY> [FORMAT]");
        return ExitCode::from(2);
    };

    if roots.is_empty() {
        eprintln!("poler harvest: error: missing root directory (ROOT_DIR)");
        eprintln!("Usage: poler harvest <ROOT_DIR> <QUERY> [FORMAT]");
        return ExitCode::from(2);
    }

    let format = parse_format(cli.format_pos.as_deref(), cli.format_flag);
    let mode = match cli.mode {
        ModeArg::Sections => HarvestMode::Sections,
        ModeArg::Files => HarvestMode::Files,
    };

    let cfg = HarvestConfig {
        roots,
        query,
        regex_mode: cli.regex,
        out_path: cli.out.unwrap_or_else(|| PathBuf::from("-")),
        format,
        mode,
        context_lines: cli.context,
        max_files: cli.max_files,
        first_n: cli.first_n,
        min_matches: cli.min_matches,
        max_file_bytes: cli.max_file_bytes,
        max_out_bytes: cli.max_out_bytes,
        threads: cli.threads,
        include_hidden: cli.hidden,
        no_ignore: cli.no_ignore,
        chunk_bytes: 4 << 20,
        no_cache: cli.no_cache,
        color: cli.color,
    };

    match run_harvest(&cfg) {
        Ok(stats) => {
            let is_stderr_tty = std::io::stderr().is_terminal();
            let color_stats = cli.color.should_color(is_stderr_tty);
            eprintln!("{}", stats.summary_line_colored(color_stats));
            if stats.read_errors > 0 {
                eprintln!("poler harvest: read errors: {}", stats.read_errors);
            }
            if stats.files_selected == 0 {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("poler harvest: error: {e}");
            ExitCode::from(2)
        }
    }
}

fn run_grep_cli(args: &[String]) -> ExitCode {
    let mut clap_args = vec!["poler-grep".to_string()];
    clap_args.extend_from_slice(args);
    let cli = match GrepCli::try_parse_from(clap_args) {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(2);
        }
    };

    let roots = if cli.paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        cli.paths
    };

    let mode = if cli.regex {
        GrepMode::Regex
    } else {
        GrepMode::Literal
    };

    let output = if cli.count {
        GrepOutput::Count
    } else if cli.list_matching {
        GrepOutput::ListMatching
    } else if cli.list_non_matching {
        GrepOutput::ListNonMatching
    } else {
        GrepOutput::Content
    };

    let ctx_before = if cli.context > 0 { cli.context } else { cli.before };
    let ctx_after = if cli.context > 0 { cli.context } else { cli.after };

    let cfg = GrepConfig {
        pattern: cli.pattern,
        mode,
        case_insensitive: cli.ignore_case,
        before: ctx_before,
        after: ctx_after,
        include_hidden: cli.hidden,
        respect_ignore: !cli.no_ignore,
        output,
        max_count: if cli.max_results > 0 { Some(cli.max_results) } else { None },
        scan_archives: cli.archives,
        // Резолв пароля архивов: флаг → POLER_ARCHIVE_KEY (как в poler-engine).
        archive_password: cli
            .archive_password
            .or_else(|| std::env::var("POLER_ARCHIVE_KEY").ok()),
        ..Default::default()
    };

    match grep_run(&roots, &cfg) {
        Ok(rep) => {
            // Bug #1: truncation warning on max-results
            if rep.stats.truncated {
                eprintln!(
                    "poler grep: warning: output truncated at --max-results {} (files reached limit: {}; increase --max-results or use --max-results 0 for unlimited)",
                    cli.max_results,
                    rep.stats.files_truncated
                );
            }
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&rep).unwrap());
            } else {
                let color = std::io::stdout().is_terminal();
                print!("{}", render_text(&rep, color));
            }
            if rep.stats.lines_matched == 0 && rep.stats.files_matched == 0 {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("poler grep: error: {e}");
            ExitCode::from(2)
        }
    }
}

// ---------------------------------------------------------------------------
// poler scan — топографический резонансный ε-поиск (порт CLI-поверхности
// poler-engine): полные сцены/функции вместо обрывков строк, метрики
// плотности ε, резонанса R(t), K-hop связи и intent-ранжирование.
// ---------------------------------------------------------------------------

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum ScanFormat {
    /// Самодостаточные Context Anchors (контракт для ИИ-агента).
    AiJson,
    /// Человекочитаемый Markdown.
    Md,
    /// Компактный однострочный список.
    Simple,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum ScanResonance {
    Hits,
    Field,
    Psi,
    Poler,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum ScanIntent {
    Auto,
    Code,
    Prose,
    Off,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum ScanPii {
    Off,
    Mask,
    Redact,
}

#[derive(Parser, Debug)]
#[command(
    name = "poler scan",
    about = "Topographic resonance search: full scenes instead of line scraps, epsilon density, R(t), K-hop relations"
)]
struct ScanCli {
    /// Корень сканирования (файл или каталог).
    #[arg(value_name = "PATH")]
    path: PathBuf,

    /// Поисковый запрос (токены; синтаксис кода → приоритет кода).
    #[arg(short = 'q', long = "query")]
    query: String,

    #[arg(long, value_enum, default_value_t = ScanFormat::AiJson)]
    format: ScanFormat,

    /// Режим накопления резонанса.
    #[arg(long = "resonance-mode", value_enum, default_value_t = ScanResonance::Hits)]
    resonance: ScanResonance,

    /// Intent-ранжирование код ↔ проза.
    #[arg(long, value_enum, default_value_t = ScanIntent::Auto)]
    intent: ScanIntent,

    /// Режим очистки PII.
    #[arg(long, value_enum, default_value_t = ScanPii::Mask)]
    pii: ScanPii,

    /// Число возвращаемых якорей.
    #[arg(long = "top-n", default_value_t = 10)]
    top_n: usize,

    /// Радиус окна в токенах (полуокно).
    #[arg(long = "window", default_value_t = 40)]
    window_radius: usize,

    /// Глубина K-hop обхода графа.
    #[arg(short = 'k', long = "k-hop", default_value_t = 2)]
    k_hop: usize,

    /// Расширения файлов (comma-separated; по умолчанию — DEFAULT_EXTENSIONS).
    #[arg(long)]
    extensions: Option<String>,

    /// Показывать скрытые файлы.
    #[arg(long, default_value_t = false)]
    hidden: bool,

    /// Максимум K-hop отношений на якорь.
    #[arg(long = "max-relations", default_value_t = 64)]
    max_relations: usize,

    /// Бюджет рёбер графа сущностей.
    #[arg(long = "max-graph-triples", default_value_t = 200_000)]
    max_graph_triples: usize,

    /// POLER[Ψ]: η — скорость обучения внимания.
    #[arg(long = "psi-eta", default_value_t = 0.05)]
    psi_eta: f64,
    /// POLER[Ψ]: γ — вес резонансного члена.
    #[arg(long = "psi-gamma", default_value_t = 0.5)]
    psi_gamma: f64,
    /// POLER[Ψ]: ρ — затухание резонансной памяти.
    #[arg(long = "psi-rho", default_value_t = 0.9)]
    psi_rho: f64,
    /// POLER[Ψ]: K — глубина резонансной памяти.
    #[arg(long = "psi-depth", default_value_t = 8)]
    psi_depth: usize,

    /// POLER-цикл (P3): η.
    #[arg(long = "poler-eta", default_value_t = 0.01)]
    poler_eta: f64,
    /// POLER-цикл: γ.
    #[arg(long = "poler-gamma", default_value_t = 0.1)]
    poler_gamma: f64,
    /// POLER-цикл: mix (CORDIC-нормализация).
    #[arg(long = "poler-mix", default_value_t = 0.1)]
    poler_mix: f64,

    /// Подробная статистика в stderr.
    #[arg(short = 'v', long)]
    verbose: bool,
}

fn run_scan_cli(args: &[String]) -> ExitCode {
    use poler_cli::engine::Engine;
    use poler_cli::{render_markdown, render_simple, EngineConfig, PiiMode, ResonanceMode};

    let cli = match ScanCli::try_parse_from(
        std::iter::once("poler scan".to_string()).chain(args.iter().cloned()),
    ) {
        Ok(c) => c,
        Err(e) => {
            eprint!("{e}");
            return ExitCode::from(2);
        }
    };

    if !cli.path.exists() {
        eprintln!("poler scan: путь не найден: {}", cli.path.display());
        return ExitCode::from(2);
    }

    let config = EngineConfig {
        window_radius: cli.window_radius,
        top_n: cli.top_n,
        k_hop_depth: cli.k_hop,
        pii_mode: match cli.pii {
            ScanPii::Off => PiiMode::Off,
            ScanPii::Mask | ScanPii::Redact => PiiMode::Mask,
        },
        resonance_mode: match cli.resonance {
            ScanResonance::Hits => ResonanceMode::Hits,
            ScanResonance::Field => ResonanceMode::Field,
            ScanResonance::Psi => ResonanceMode::Psi,
            ScanResonance::Poler => ResonanceMode::Poler,
        },
        extensions: match &cli.extensions {
            Some(list) => list
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
            None => poler_cli::EngineConfig::default().extensions,
        },
        include_hidden: cli.hidden,
        max_relations: cli.max_relations,
        max_graph_triples: cli.max_graph_triples,
        psi_params: poler_cli::psi::PsiParams {
            eta: cli.psi_eta,
            gamma: cli.psi_gamma,
            rho: cli.psi_rho,
            memory_depth: cli.psi_depth,
        },
        poler_params: poler_cli::poler::PolerParams {
            eta: cli.poler_eta,
            gamma: cli.poler_gamma,
            mix: cli.poler_mix,
            ..poler_cli::poler::PolerParams::default()
        },
        intent_mode: match cli.intent {
            ScanIntent::Auto => poler_cli::intent::IntentMode::Auto,
            ScanIntent::Code => poler_cli::intent::IntentMode::Code,
            ScanIntent::Prose => poler_cli::intent::IntentMode::Prose,
            ScanIntent::Off => poler_cli::intent::IntentMode::Off,
        },
        ..EngineConfig::default()
    };

    let mut engine = Engine::new(config, false);
    let (result, stats) = engine.scan(&cli.path, &cli.query);

    if cli.verbose {
        eprintln!(
            "poler scan: files={}, with_hits={}, tokens={}, hits={}, graph_nodes={}, edges={}, time={}ms",
            stats.files_scanned,
            stats.files_with_hits,
            stats.total_tokens,
            stats.total_hits,
            stats.graph_nodes,
            stats.graph_edges,
            stats.elapsed_ms
        );
    }

    match cli.format {
        ScanFormat::AiJson => {
            println!("{}", serde_json::to_string_pretty(&result).unwrap_or_default());
        }
        ScanFormat::Md => print!("{}", render_markdown(&result)),
        ScanFormat::Simple => print!("{}", render_simple(&result)),
    }
    use std::io::Write;
    let _ = std::io::stdout().flush();

    if result.total_hits == 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn run_view_cli(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("Usage: poler view <PATH[::ENTRY]> [--lines START..END] [--json]");
        return ExitCode::from(2);
    }
    let as_json = args.iter().any(|a| a == "--json");
    let target_spec = &args[0];
    let (path, entry_from_spec) = if let Some((p, e)) = target_spec.split_once("::") {
        (PathBuf::from(p), Some(e.to_string()))
    } else {
        (PathBuf::from(target_spec), None)
    };

    if !path.exists() {
        if as_json {
            println!("{}", json!({"ok": false, "error_code": "file_not_found", "error": format!("file not found: {}", path.display())}));
        } else {
            eprintln!("poler view: file not found: {}", path.display());
        }
        return ExitCode::from(3);
    }

    // If VFS container and specific entry specified:
    if let Some(entry) = entry_from_spec {
        let norm = poler_cli::core::vfs::norm_name(&entry);
        match poler_cli::core::vfs::vfs_read(&path, &norm, 4 << 20) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => {
                    if as_json {
                        println!("{}", json!({
                            "ok": true,
                            "path": path.to_string_lossy(),
                            "entry": norm,
                            "bytes": text.len(),
                            "text": text,
                        }));
                    } else {
                        print!("{text}");
                    }
                    return ExitCode::SUCCESS;
                }
                Err(_) => {
                    if as_json {
                        println!("{}", json!({"ok": false, "error_code": "binary_entry", "error": format!("entry '{norm}' contains binary data")}));
                    } else {
                        eprintln!("poler view: entry '{norm}' contains binary data");
                    }
                    return ExitCode::from(1);
                }
            },
            Err(e) => {
                if as_json {
                    println!("{}", json!({"ok": false, "error_code": "entry_not_found", "error": format!("failed to read entry '{norm}': {e}")}));
                } else {
                    eprintln!("poler view: failed to read entry '{norm}': {e}");
                }
                return ExitCode::from(3);
            }
        }
    }

    // If document (DOCX, EPUB, PDF, MOBI, FB2) — extract clean text
    if poler_cli::core::extractor::is_extractable_format(&path) {
        match poler_cli::core::extractor::extract_text(&path) {
            Ok(Some(text)) => {
                if as_json {
                    println!("{}", json!({
                        "ok": true,
                        "path": path.to_string_lossy(),
                        "extracted": true,
                        "bytes": text.len(),
                        "text": text,
                    }));
                } else {
                    println!("{text}");
                }
                return ExitCode::SUCCESS;
            }
            Ok(None) => {}
            Err(e) => {
                if as_json {
                    println!("{}", json!({"ok": false, "error_code": "extraction_error", "error": format!("text extraction error: {e}")}));
                } else {
                    eprintln!("poler view: text extraction error: {e}");
                }
                return ExitCode::from(1);
            }
        }
    }

    // If VFS container (.zip, .tar, .poler, etc.) without entry specified —
    // print formatted archive listing instead of dumping raw binary bytes!
    if poler_cli::core::vfs::is_vfs_container(&path) {
        match poler_cli::core::vfs::vfs_list(&path) {
            Ok((kind, entries)) => {
                if as_json {
                    let list: Vec<serde_json::Value> = entries.iter().map(|e| json!({
                        "name": e.name,
                        "size": e.size,
                        "compressed": e.compressed,
                        "is_dir": e.is_dir,
                    })).collect();
                    println!("{}", json!({
                        "ok": true,
                        "kind": format!("{kind:?}"),
                        "path": path.to_string_lossy(),
                        "total_entries": entries.len(),
                        "entries": list,
                    }));
                    return ExitCode::SUCCESS;
                }
                println!("{kind:?} container: {} (total entries: {})", path.display(), entries.len());
                for e in entries.iter().take(50) {
                    let d = if e.is_dir { "/" } else { "" };
                    println!("{:>10}  {}{}", e.size, e.name, d);
                }
                if entries.len() > 50 {
                    println!("... and {} more entries (view entry: poler view {}::<ENTRY>)", entries.len() - 50, path.display());
                }
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                if as_json {
                    println!("{}", json!({"ok": false, "error_code": "archive_listing_error", "error": format!("archive listing error: {e}")}));
                } else {
                    eprintln!("poler view: archive listing error: {e}");
                }
                return ExitCode::from(1);
            }
        }
    }

    // Otherwise open via Piece-Table mmap2
    match poler_cli::core::buffer::PolerBuffer::open(&path) {
        Ok(mut b) => {
            let total_len = b.total_len();
            let mut line = 0u64;
            let mut count = 50usize;
            if let Some(pos) = args.iter().position(|a| a == "--lines") {
                if let Some(spec) = args.get(pos + 1) {
                    if let Some((s, e)) = spec.split_once("..") {
                        line = s.parse().unwrap_or(0);
                        let end: u64 = e.parse().unwrap_or(line + 50);
                        count = end.saturating_sub(line).max(1) as usize;
                    }
                }
            }
            match b.read_span(line, count, 4 << 20) {
                Ok(span) => {
                    if as_json {
                        println!("{}", json!({
                            "ok": true,
                            "path": path.to_string_lossy(),
                            "total_bytes": total_len,
                            "start_line": line,
                            "count": count,
                            "byte_capped": span.byte_capped,
                            "text": span.text,
                        }));
                    } else {
                        print!("{}", span.text);
                        if span.byte_capped {
                            eprintln!("\n[poler view: output capped at 4 MiB, total size: {} bytes]", total_len);
                        }
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    if as_json {
                        println!("{}", json!({"ok": false, "error_code": "read_error", "error": format!("read error: {e}")}));
                    } else {
                        eprintln!("poler view: read error: {e}");
                    }
                    ExitCode::from(1)
                }
            }
        }
        Err(e) => {
            if as_json {
                println!("{}", json!({"ok": false, "error_code": "open_error", "error": format!("open error: {e}")}));
            } else {
                eprintln!("poler view: open error: {e}");
            }
            ExitCode::from(1)
        }
    }
}

fn run_patch_cli(args: &[String]) -> ExitCode {
    if args.len() < 3 {
        eprintln!("Usage: poler patch <PATH[::ENTRY]> <OLD_TEXT> <NEW_TEXT> [--entry <ENTRY>] [--dry-run|--diff] [--json]");
        return ExitCode::from(2);
    }
    let target_spec = &args[0];
    let old_text = &args[1];
    let new_text = &args[2];

    let mut entry_override: Option<String> = None;
    let mut dry_run = false;
    let mut as_json = false;
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--entry" | "-e" => {
                if i + 1 < args.len() {
                    entry_override = Some(args[i + 1].clone());
                    i += 2;
                    continue;
                }
            }
            "--dry-run" | "--diff" => {
                dry_run = true;
            }
            "--json" => {
                as_json = true;
            }
            _ => {}
        }
        i += 1;
    }

    let report_err = |code_str: &str, exit_code: u8, msg: &str| -> ExitCode {
        if as_json {
            println!("{}", json!({
                "ok": false,
                "error_code": code_str,
                "error": msg,
            }));
        } else {
            eprintln!("poler patch: {msg}");
        }
        ExitCode::from(exit_code)
    };

    let (path, entry_from_spec) = if let Some((p, e)) = target_spec.split_once("::") {
        (PathBuf::from(p), Some(e.to_string()))
    } else {
        (PathBuf::from(target_spec), None)
    };

    let target_entry = entry_override.or(entry_from_spec);

    if !path.exists() {
        return report_err("file_not_found", 3, &format!("file not found: {}", path.display()));
    }

    // If VFS container (.zip, .docx, .epub, .odt, .jar, .tar, .poler, etc.) —
    // route through CoW VFS and flush_archive to preserve container structure and CRC-32!
    if poler_cli::core::vfs::is_vfs_container(&path) {
        let entry_name = match target_entry {
            Some(e) => poler_cli::core::vfs::norm_name(&e),
            None => {
                // Auto-detect target entry containing old_text if not specified
                match poler_cli::core::vfs::vfs_list(&path) {
                    Ok((_, entries)) => {
                        let mut candidates = Vec::new();
                        for e in entries.iter().filter(|e| !e.is_dir && e.size <= poler_cli::core::vfs::DEFAULT_MAX_ENTRY_BYTES) {
                            if let Ok(bytes) = poler_cli::core::vfs::vfs_read(&path, &e.name, poler_cli::core::vfs::DEFAULT_MAX_ENTRY_BYTES) {
                                if let Ok(s) = String::from_utf8(bytes) {
                                    if s.contains(old_text) {
                                        candidates.push(e.name.clone());
                                    }
                                }
                            }
                        }
                        if candidates.is_empty() {
                            return report_err("match_not_found", 3, &format!("pattern {:?} not found in any entry of archive {}", old_text, path.display()));
                        } else if candidates.len() == 1 {
                            let chosen = candidates.remove(0);
                            if !as_json {
                                eprintln!("poler patch: auto-detected target entry '{}' in archive {}", chosen, path.display());
                            }
                            chosen
                        } else {
                            return report_err("ambiguous_match", 4, &format!("pattern found in multiple entries: {:?}. Specify target entry: poler patch {}::<ENTRY> {:?} {:?}", candidates, path.display(), old_text, new_text));
                        }
                    }
                    Err(e) => {
                        return report_err("container_read_error", 1, &format!("container read error for {}: {e}", path.display()));
                    }
                }
            }
        };

        // Read entry via vfs_read
        let bytes = match poler_cli::core::vfs::vfs_read(&path, &entry_name, poler_cli::core::vfs::DEFAULT_MAX_ENTRY_BYTES) {
            Ok(b) => b,
            Err(e) => {
                return report_err("entry_not_found", 3, &format!("failed to read entry '{entry_name}': {e}"));
            }
        };

        if std::str::from_utf8(&bytes).is_err() {
            return report_err("binary_entry", 1, &format!("entry '{entry_name}' contains binary data, text patching not supported"));
        }

        let mut tmp = match tempfile::NamedTempFile::new() {
            Ok(t) => t,
            Err(e) => return report_err("io_error", 1, &format!("failed to create temp file: {e}")),
        };
        use std::io::Write as _;
        if let Err(e) = tmp.write_all(&bytes) {
            return report_err("io_error", 1, &format!("failed to write temp file: {e}"));
        }
        if let Err(e) = tmp.flush() {
            return report_err("io_error", 1, &format!("failed to flush temp file: {e}"));
        }
        let mut buf = match poler_cli::core::buffer::PolerBuffer::open(tmp.path()) {
            Ok(mut b) => {
                let disp = format!("{}::{}", path.display(), entry_name);
                b.set_path(std::path::Path::new(&disp));
                b
            }
            Err(e) => return report_err("open_error", 1, &format!("failed to mmap container entry: {e}")),
        };
        match buf.replace_match(old_text, new_text, None, None, false, true) {
            Ok(_) => {
                if dry_run {
                    let diff = buf.piece_diff(3).unwrap_or_default();
                    if as_json {
                        println!("{}", json!({
                            "ok": true,
                            "path": path.to_string_lossy(),
                            "entry": entry_name,
                            "dry_run": true,
                            "diff": diff,
                        }));
                    } else {
                        print!("{diff}");
                    }
                    ExitCode::SUCCESS
                } else {
                    let new_bytes = buf.read_range(0, buf.total_len() as usize);
                    let mut ov = poler_cli::core::vfs_edit::VfsOverlay::default();
                    ov.write(&entry_name, new_bytes);
                    match poler_cli::core::vfs_edit::flush_archive(&path, &ov, &path, poler_cli::core::vfs_edit::DEFAULT_MAX_WRITE_BYTES) {
                        Ok(stats) => {
                            if as_json {
                                println!("{}", json!({
                                    "ok": true,
                                    "path": path.to_string_lossy(),
                                    "entry": entry_name,
                                    "bytes": stats.bytes,
                                    "dry_run": false,
                                }));
                            } else {
                                eprintln!("poler patch: successfully patched entry '{}' in archive ({} bytes)", entry_name, stats.bytes);
                            }
                            ExitCode::SUCCESS
                        }
                        Err(e) => {
                            report_err("archive_rebuild_error", 1, &format!("failed to rebuild archive: {e}"))
                        }
                    }
                }
            }
            Err(e) => {
                let (code, ec) = if e.starts_with("match_not_found:") {
                    ("match_not_found", 3)
                } else if e.starts_with("ambiguous_match:") {
                    ("ambiguous_match", 4)
                } else if e.starts_with("trit_syntax_broken:") {
                    ("trit_syntax_broken", 5)
                } else if e.starts_with("invalid_argument:") {
                    ("invalid_argument", 2)
                } else {
                    ("replacement_failed", 1)
                };
                report_err(code, ec, &format!("replacement failed: {e}"))
            }
        }
    } else {
        match poler_cli::core::buffer::PolerBuffer::open(&path) {
            Ok(mut b) => {
                match b.replace_match(old_text, new_text, None, None, false, true) {
                    Ok(_) => {
                        if dry_run {
                            let diff = b.piece_diff(3).unwrap_or_default();
                            if as_json {
                                println!("{}", json!({
                                    "ok": true,
                                    "path": path.to_string_lossy(),
                                    "dry_run": true,
                                    "diff": diff,
                                }));
                            } else {
                                print!("{diff}");
                            }
                            ExitCode::SUCCESS
                        } else {
                            match b.save() {
                                Ok(bytes) => {
                                    if as_json {
                                        println!("{}", json!({
                                            "ok": true,
                                            "path": path.to_string_lossy(),
                                            "bytes": bytes,
                                            "dry_run": false,
                                        }));
                                    } else {
                                        eprintln!("poler patch: successfully patched ({} bytes)", bytes);
                                    }
                                    ExitCode::SUCCESS
                                }
                                Err(e) => {
                                    report_err("save_error", 1, &format!("save error: {e}"))
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let (code, ec) = if e.starts_with("match_not_found:") {
                            ("match_not_found", 3)
                        } else if e.starts_with("ambiguous_match:") {
                            ("ambiguous_match", 4)
                        } else if e.starts_with("trit_syntax_broken:") {
                            ("trit_syntax_broken", 5)
                        } else if e.starts_with("invalid_argument:") {
                            ("invalid_argument", 2)
                        } else {
                            ("replacement_failed", 1)
                        };
                        report_err(code, ec, &format!("replacement failed: {e}"))
                    }
                }
            }
            Err(e) => {
                report_err("file_open_error", 1, &format!("failed to open file: {e}"))
            }
        }
    }
}

fn run_diff_cli(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("Usage: poler diff <PATH>");
        return ExitCode::from(2);
    }
    let path = PathBuf::from(&args[0]);
    match poler_cli::core::buffer::PolerBuffer::open(&path) {
        Ok(mut b) => {
            match b.piece_diff(3) {
                Ok(diff) => {
                    print!("{diff}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("poler diff: {e}");
                    ExitCode::from(1)
                }
            }
        }
        Err(e) => {
            eprintln!("poler diff: error: {e}");
            ExitCode::from(1)
        }
    }
}

fn run_edit_cli(args: &[String]) -> ExitCode {
    // Если передан флаг --schema
    if args.iter().any(|a| a == "--schema") {
        print_editor_schema();
        return ExitCode::SUCCESS;
    }

    // Если передан флаг --mcp
    if args.iter().any(|a| a == "--mcp") {
        let code = poler_cli::core::mcp::run_mcp_stdio();
        return ExitCode::from(code as u8);
    }

    // Если передан флаг --exec
    if let Some(pos) = args.iter().position(|a| a == "--exec") {
        let Some(raw) = args.get(pos + 1) else {
            eprintln!("{{\"ok\":false,\"error\":\"--exec requires an argument\"}}");
            return ExitCode::from(2);
        };
        let trimmed = raw.trim();
        let mut cmd: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                println!("{}", json!({"ok": false, "error": format!("bad_json: {e}")}));
                return ExitCode::from(2);
            }
        };
        if let Some(c) = cmd.get("cmd").and_then(|x| x.as_str()) {
            if matches!(
                c,
                "replace_match" | "apply_batch" | "apply_diff" | "insert_at" | "delete"
            ) && cmd.get("auto_save").is_none()
                && cmd.get("path").is_some()
            {
                cmd["auto_save"] = json!(true);
            }
        }
        let mut sess = poler_cli::core::protocol::CoreSession::new();
        let resp = sess.execute(&cmd, true, None).unwrap_or_else(|| json!({"ok": true}));
        println!("{resp}");
        let ok = resp.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
        return if ok { ExitCode::SUCCESS } else { ExitCode::from(1) };
    }

    let listen_pos = args.iter().position(|a| a == "--listen");
    let listen_addr = listen_pos.and_then(|i| args.get(i + 1)).map(|s| s.as_str());

    let code = poler_cli::core::protocol::run_edit_server_with_listen(listen_addr);
    ExitCode::from(code as u8)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let arg0 = args.get(0).map(|s| s.as_str()).unwrap_or("");
    let file_name = std::path::Path::new(arg0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    // Multicall support
    if file_name == "poler-harvest" {
        return run_harvest_cli(&args[1..]);
    }
    if file_name == "poler-edit-core" {
        return run_edit_cli(&args[1..]);
    }

    if args.len() < 2 {
        print_help();
        return ExitCode::SUCCESS;
    }

    let cmd = args[1].as_str();
    match cmd {
        "harvest" => run_harvest_cli(&args[2..]),
        "grep" => run_grep_cli(&args[2..]),
        "scan" => run_scan_cli(&args[2..]),
        "edit" => run_edit_cli(&args[2..]),
        "view" => run_view_cli(&args[2..]),
        "patch" => run_patch_cli(&args[2..]),
        "diff" => run_diff_cli(&args[2..]),
        "mcp" => {
            let code = poler_cli::core::mcp::run_mcp_stdio();
            ExitCode::from(code as u8)
        }
        "schema" | "--schema" => {
            print_schema();
            ExitCode::SUCCESS
        }
        "-h" | "--help" | "help" => {
            print_help();
            ExitCode::SUCCESS
        }
        "-V" | "--version" | "version" => {
            println!("poler {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        other if other.starts_with("--") => {
            run_edit_cli(&args[1..])
        }
        other => {
            eprintln!("poler: unknown command '{other}'. Use 'poler --help'.");
            ExitCode::from(1)
        }
    }
}
