//! poler — Sovereign Ultra-Fast Harvester, Grep, Zero-Copy Piece-Table Editor & AI MCP Engine

use clap::{Parser, ValueEnum};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use serde_json::json;

use poler_cli::harvest::{
    run_harvest, ColorChoice, HarvestConfig, HarvestFormat, HarvestMode,
};
use poler_cli::grep::{
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

    #[arg(long = "max-results", default_value_t = 50_000)]
    max_results: usize,

    #[arg(long = "json")]
    json: bool,
}

fn print_help() {
    println!(
        "poler {} — Sovereign Ultra-Fast Harvester, Grep, Zero-Copy Piece-Table & AI MCP Engine\n\n\
         ИСПОЛЬЗОВАНИЕ:\n  \
           poler <КОМАНДА> [АРГУМЕНТЫ...]\n\n\
         ОСНОВНЫЕ КОМАНДЫ:\n  \
           poler harvest <ROOT> \"<QUERY>\" [flags]   Полнодисковый сбор с кэшем L1-L3 (.t5a) и ANSI-подсветкой\n  \
           poler grep \"<PATTERN>\" [PATH] [flags]    Потоковый grep по проекту с учётом .gitignore (замена ripgrep)\n  \
           poler edit [PATH]                       Запуск stdio JSON-RPC ядра редактора для IDE и агентов\n  \
           poler view <PATH> [--lines A..B]        Мгновенный просмотр текста, 40 ГБ логов, DOCX, EPUB, PDF\n  \
           poler patch <PATH> <OLD> <NEW>          Атомарная хирургическая замена текста в файле или архиве\n  \
           poler diff <PATH>                       Показать diff несохранённых правок Piece-Table\n  \
           poler mcp                               Запуск единого MCP-сервера для Claude/Cursor/Antigravity\n  \
           poler schema                            Самодокументирующийся JSON-манифест всех инструментов\n\n\
         СОВМЕСТИМОСТЬ:\n  \
           poler-harvest ...                       Прямой алиас подкоманды 'harvest'\n  \
           poler-edit-core ...                     Прямой алиас подкоманды 'edit'\n",
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
        eprintln!("poler harvest: ошибка: не указан поисковый запрос (QUERY)");
        eprintln!("Использование: poler harvest <ROOT_DIR> <QUERY> [FORMAT]");
        return ExitCode::from(2);
    };

    if roots.is_empty() {
        eprintln!("poler harvest: ошибка: не указан корневой каталог (ROOT_DIR)");
        eprintln!("Использование: poler harvest <ROOT_DIR> <QUERY> [FORMAT]");
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
                eprintln!("poler harvest: ошибок чтения: {}", stats.read_errors);
            }
            if stats.files_selected == 0 {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("poler harvest: ошибка: {e}");
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
        ..Default::default()
    };

    match grep_run(&roots, &cfg) {
        Ok(rep) => {
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
            eprintln!("poler grep: ошибка: {e}");
            ExitCode::from(2)
        }
    }
}

fn run_view_cli(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("Использование: poler view <PATH> [--lines START..END]");
        return ExitCode::from(2);
    }
    let path = PathBuf::from(&args[0]);
    if !path.exists() {
        eprintln!("poler view: файл не найден: {}", path.display());
        return ExitCode::from(2);
    }

    // Если это документ (DOCX, EPUB, PDF, MOBI, FB2) — извлекаем чистый текст
    if poler_cli::core::extractor::is_extractable_format(&path) {
        match poler_cli::core::extractor::extract_text(&path) {
            Ok(Some(text)) => {
                println!("{text}");
                return ExitCode::SUCCESS;
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("poler view: ошибка извлечения: {e}");
                return ExitCode::from(1);
            }
        }
    }

    // Иначе открываем через Piece-Table mmap2 (работает хоть на 40 ГБ файлах)
    match poler_cli::core::buffer::PolerBuffer::open(&path) {
        Ok(mut b) => {
            let total_len = b.total_len();
            // По умолчанию читаем первые 50 строк
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
                    print!("{}", span.text);
                    if span.byte_capped {
                        eprintln!("\n[poler view: вывод усечён до 4 МиБ, общий размер: {} байт]", total_len);
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("poler view: ошибка чтения: {e}");
                    ExitCode::from(1)
                }
            }
        }
        Err(e) => {
            eprintln!("poler view: ошибка открытия: {e}");
            ExitCode::from(1)
        }
    }
}

fn run_patch_cli(args: &[String]) -> ExitCode {
    if args.len() < 3 {
        eprintln!("Использование: poler patch <PATH> <OLD_TEXT> <NEW_TEXT>");
        return ExitCode::from(2);
    }
    let path = PathBuf::from(&args[0]);
    let old_text = &args[1];
    let new_text = &args[2];

    match poler_cli::core::buffer::PolerBuffer::open(&path) {
        Ok(mut b) => {
            match b.replace_match(old_text, new_text, None, None, false, true) {
                Ok(_) => match b.save() {
                    Ok(bytes) => {
                        eprintln!("poler patch: успешно обновлено ({} байт)", bytes);
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("poler patch: ошибка сохранения: {e}");
                        ExitCode::from(1)
                    }
                },
                Err(e) => {
                    eprintln!("poler patch: замена не удалась: {e}");
                    ExitCode::from(1)
                }
            }
        }
        Err(e) => {
            eprintln!("poler patch: ошибка открытия файла: {e}");
            ExitCode::from(1)
        }
    }
}

fn run_diff_cli(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("Использование: poler diff <PATH>");
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
            eprintln!("poler diff: ошибка: {e}");
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
            eprintln!("{{\"ok\":false,\"error\":\"--exec требует аргумент\"}}");
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
            eprintln!("poler: неизвестная команда '{other}'. Используйте 'poler --help'.");
            ExitCode::from(1)
        }
    }
}
