//! poler-edit :: Нативный MCP-сервер (Model Context Protocol, JSON-RPC 2.0 поверх stdio).
//!
//! Запуск: `poler-edit-core --mcp`
//! * Автоматически поддерживает оба транспорта фрейминга на stdin/stdout:
//!   1) стандартный MCP JSON-lines (один JSON-объект на строку, спецификация 2024-11-05 / 2025-03-26);
//!   2) LSP-фрейминг `Content-Length: N\r\n\r\n{...}`.
//! * Если в окружении заданы `POLER_EDIT_ADDR` и `POLER_EDIT_TOKEN` (встроенный терминал
//!   POLER Edit или внешний агент, подключённый к открытому окну IDE), команды прозрачно
//!   маршрутизируются в живой GUI-процесс — вкладки редактора обновляются на экране в реальном времени!

use std::io::{self, BufRead, BufReader, Write};

use serde_json::{json, Value};

use crate::protocol::{send_tcp_command, CoreSession};

#[derive(Clone, Copy, PartialEq, Eq)]
enum WireFraming {
    JsonLines,
    ContentLength,
}

pub struct McpServer {
    session: CoreSession,
    live_gui: Option<(String, String)>,
}

impl McpServer {
    pub fn new() -> Self {
        let live_gui = match (
            std::env::var("POLER_EDIT_ADDR").ok(),
            std::env::var("POLER_EDIT_TOKEN").ok(),
        ) {
            (Some(a), Some(t)) if !a.trim().is_empty() && !t.trim().is_empty() => {
                Some((a.trim().to_string(), t.trim().to_string()))
            }
            _ => None,
        };
        Self {
            session: CoreSession::new(),
            live_gui,
        }
    }

    fn run_core_cmd(&mut self, cmd: Value) -> Value {
        if let Some((addr, token)) = &self.live_gui {
            if let Ok(resp) = send_tcp_command(addr, token, cmd.clone()) {
                return resp;
            }
        }
        self.session
            .execute(&cmd, true, None)
            .unwrap_or_else(|| json!({"ok": false, "error": "session_closed"}))
    }

    /// Обработать входящее JSON-RPC 2.0 сообщение MCP. Возвращает `None` для уведомлений (notifications).
    pub fn handle_rpc(&mut self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");

        // JSON-RPC 2.0 Notifications (без поля `id`) не требуют ответа.
        id.as_ref()?;
        let req_id = id.unwrap();

        match method {
            "initialize" => {
                let client_proto = msg
                    .get("params")
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("2024-11-05");
                Some(rpc_ok(
                    req_id,
                    json!({
                        "protocolVersion": client_proto,
                        "capabilities": {
                            "tools": { "listChanged": false }
                        },
                        "serverInfo": {
                            "name": "poler-edit-core",
                            "version": env!("CARGO_PKG_VERSION")
                        },
                        "instructions": "Суверенное ядро POLER Edit (Zero-Copy Piece-Table + Trit-Silicon 𝕋={-1,0,+1}, барьер скобок 𝕋=0, O(pieces) diff, pack5 CSR Call-Graph 0.2 B/edge). Виртуальные глаза ИИ: poler_saccade (ε_high — карта документа за 1 шаг) → poler_scroll (ε_low — фовеальное окно с удержанием курсора) → poler_seek_energy (прыжок к пику ε). Для правок: poler_replace_match / poler_apply_batch с check_syntax=true. poler_search с rank_by=\"epsilon\" и min_epsilon экономит ~90% токенов; poler_project_impact — перед изменением публичных сигнатур. Фаза 5+ — ЖИВОЙ движок poler-engine как сетевой гейт: poler_engine_status (демон/версия), poler_engine_search / poler_engine_grep (ε-сцены и полный grep по репозиторию через демона), poler_engine_pack / poler_engine_squeeze (дотискание форматов TAR/GZ/ZST в .poler — закон семантики), poler_engine_archive_ls (любой архив без распаковки), poler_engine_impact (AIDDE call-graph), poler_engine_crystal_ingest (корпус → Кристалл .t5c)."
                    }),
                ))
            }

            "ping" => Some(rpc_ok(req_id, json!({}))),

            "tools/list" => Some(rpc_ok(req_id, json!({ "tools": mcp_tools_catalog() }))),

            "tools/call" => {
                let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
                let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let tool_out = self.dispatch_tool(name, &args);
                Some(rpc_ok(req_id, tool_out))
            }

            other => Some(rpc_err(
                req_id,
                -32601,
                &format!("Method not found: {other}"),
            )),
        }
    }

    fn dispatch_tool(&mut self, name: &str, args: &Value) -> Value {
        if name == "poler_harvest" {
            let roots = args.get("roots")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(std::path::PathBuf::from)).collect())
                .unwrap_or_else(|| {
                    if let Some(r) = args.get("root").and_then(|x| x.as_str()) {
                        vec![std::path::PathBuf::from(r)]
                    } else {
                        vec![std::path::PathBuf::from(".")]
                    }
                });
            let query = args.get("query").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let format = match args.get("format").and_then(|x| x.as_str()).unwrap_or("markdown") {
                "json" => crate::harvest::HarvestFormat::Json,
                "address" => crate::harvest::HarvestFormat::Address,
                "corpus" => crate::harvest::HarvestFormat::Corpus,
                _ => crate::harvest::HarvestFormat::Markdown,
            };
            let context = args.get("context").and_then(|x| x.as_u64()).unwrap_or(2) as usize;
            let temp_out = match tempfile::NamedTempFile::new() {
                Ok(t) => t.into_temp_path(),
                Err(e) => return tool_result(json!({ "error": e.to_string() }), true),
            };
            let mut cfg = crate::harvest::HarvestConfig::new(roots, query, temp_out.to_path_buf());
            cfg.format = format;
            cfg.context_lines = context;
            return match crate::harvest::run_harvest(&cfg) {
                Ok(rep) => {
                    let text = std::fs::read_to_string(&temp_out).unwrap_or_default();
                    let _ = std::fs::remove_file(&temp_out);
                    tool_result(json!({
                        "files_scanned": rep.files_scanned,
                        "files_matched": rep.files_matched,
                        "files_selected": rep.files_selected,
                        "sections_written": rep.sections_written,
                        "duration_ms": rep.duration_ms,
                        "is_warm": rep.is_warm,
                        "output": text,
                    }), false)
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&temp_out);
                    tool_result(json!({ "error": e }), true)
                }
            };
        }
        if name == "poler_grep" {
            let roots = args.get("roots")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(std::path::PathBuf::from)).collect())
                .unwrap_or_else(|| {
                    if let Some(r) = args.get("root").and_then(|x| x.as_str()) {
                        vec![std::path::PathBuf::from(r)]
                    } else {
                        vec![std::path::PathBuf::from(".")]
                    }
                });
            let pattern = args.get("pattern").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let mode = if args.get("mode").and_then(|x| x.as_str()) == Some("regex") {
                crate::grep::GrepMode::Regex
            } else {
                crate::grep::GrepMode::Literal
            };
            let case_insensitive = args.get("case_insensitive").and_then(|x| x.as_bool()).unwrap_or(false);
            let before = args.get("context_before").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
            let after = args.get("context_after").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
            let max_count = args.get("max_count").and_then(|x| x.as_u64()).map(|n| n as usize);

            let cfg = crate::grep::GrepConfig {
                pattern,
                mode,
                case_insensitive,
                before,
                after,
                max_count,
                ..Default::default()
            };
            return match crate::grep::grep_run(&roots, &cfg) {
                Ok(rep) => {
                    let text = crate::grep::render_text(&rep, false);
                    tool_result(json!({
                        "lines_matched": rep.stats.lines_matched,
                        "files_matched": rep.stats.files_matched,
                        "files_scanned": rep.stats.files_scanned,
                        "occurrences": rep.stats.occurrences,
                        "elapsed_ms": rep.stats.elapsed_ms,
                        "output": text,
                    }), false)
                }
                Err(e) => tool_result(json!({ "error": e }), true),
            };
        }
        let core_req = match name {
            "poler_read_span" => {
                let mut q = args.clone();
                set_cmd(&mut q, "read_span");
                q
            }
            "poler_replace_match" => {
                let mut q = args.clone();
                set_cmd(&mut q, "replace_match");
                default_bool(&mut q, "check_syntax", true);
                let has_explicit_doc = q.get("doc").and_then(|x| x.as_u64()).unwrap_or(0) != 0
                    && q.get("path").is_none();
                default_bool(&mut q, "auto_save", !has_explicit_doc);
                q
            }
            "poler_apply_batch" => {
                let mut q = args.clone();
                set_cmd(&mut q, "apply_batch");
                default_bool(&mut q, "check_syntax", true);
                let has_explicit_doc = q.get("doc").and_then(|x| x.as_u64()).unwrap_or(0) != 0
                    && q.get("path").is_none();
                default_bool(&mut q, "auto_save", !has_explicit_doc);
                q
            }
            "poler_apply_diff" => {
                let mut q = args.clone();
                set_cmd(&mut q, "apply_diff");
                default_bool(&mut q, "check_syntax", true);
                let has_explicit_doc = q.get("doc").and_then(|x| x.as_u64()).unwrap_or(0) != 0
                    && q.get("path").is_none();
                default_bool(&mut q, "auto_save", !has_explicit_doc);
                q
            }
            "poler_piece_diff" => {
                let mut q = args.clone();
                set_cmd(&mut q, "piece_diff");
                q
            }
            "poler_search" => {
                let mut q = args.clone();
                set_cmd(&mut q, "search");
                if q.get("limit").is_none() {
                    q["limit"] = json!(50);
                }
                q
            }
            // Виртуальные глаза ИИ (двухэнергетический протокол чтения):
            // ε_high — саккада (карта документа), ε_low — фовеальный скролл
            // с удержанием курсора, seek_energy — авто-прыжок к пику ε.
            "poler_saccade" => {
                let mut q = args.clone();
                set_cmd(&mut q, "saccade");
                q
            }
            "poler_scroll" => {
                let mut q = args.clone();
                set_cmd(&mut q, "scroll");
                default_u64(&mut q, "lines", 25);
                default_str(&mut q, "direction", "forward");
                q
            }
            "poler_seek_energy" => {
                let mut q = args.clone();
                set_cmd(&mut q, "seek_energy");
                default_str(&mut q, "direction", "next_peak");
                q
            }
            "poler_verify_syntax" => {
                let mut q = args.clone();
                set_cmd(&mut q, "verify_syntax");
                q
            }
            "poler_outline" => {
                if let Some(line) = args.get("line").and_then(|x| x.as_u64()) {
                    let mut q = args.clone();
                    set_cmd(&mut q, "breadcrumbs");
                    q["line"] = json!(line);
                    q
                } else {
                    let mut q = args.clone();
                    set_cmd(&mut q, "outline");
                    q
                }
            }
            "poler_project_impact" => {
                let mut q = args.clone();
                set_cmd(&mut q, "project_impact");
                q
            }
            "poler_semantic_search" => {
                let mut q = args.clone();
                set_cmd(&mut q, "semantic_search");
                q
            }
            "poler_fork_doc" => {
                let mut q = args.clone();
                set_cmd(&mut q, "fork_doc");
                q
            }
            "poler_merge_fork" => {
                let mut q = args.clone();
                set_cmd(&mut q, "merge_fork");
                q
            }
            "poler_tx_save_all" => {
                let mut q = args.clone();
                set_cmd(&mut q, "tx_save_all");
                q
            }
            "poler_extract_doc" => {
                let mut q = args.clone();
                set_cmd(&mut q, "extract_doc");
                q
            }
            "poler_list_docs" => {
                let list_r = self.run_core_cmd(json!({"id": 1, "cmd": "list_docs"}));
                let sil_r = self.run_core_cmd(json!({"id": 2, "cmd": "silicon_info"}));
                let combined = json!({
                    "ok": true,
                    "live_gui_connected": self.live_gui.is_some(),
                    "open_docs": list_r.get("docs").cloned().unwrap_or_else(|| json!([])),
                    "silicon_info": sil_r,
                });
                return tool_result(combined, false);
            }

            // ── Фаза 5 «MCP-глаза на VFS»: RAM-полигон vfs://ai_scratch/ ──
            // Кристаллический_scratchpad агента (ноль байт на диске,
            // дедупликация FastCDC+BLAKE3) + Кристалл Знаний .t5c.
            "poler_vfs_mount" => {
                let mut q = args.clone();
                set_cmd(&mut q, "vfs_list");
                q
            }
            "poler_scratch_put" => {
                let mut q = args.clone();
                set_cmd(&mut q, "scratch_put");
                q
            }
            "poler_scratch_get" => {
                let mut q = args.clone();
                set_cmd(&mut q, "scratch_get");
                default_bool(&mut q, "text", true);
                q
            }
            "poler_scratch_list" => {
                let mut q = args.clone();
                set_cmd(&mut q, "scratch_list");
                q
            }
            "poler_scratch_discard" => {
                let mut q = args.clone();
                set_cmd(&mut q, "scratch_discard");
                q
            }
            "poler_vfs_apply" => {
                let mut q = args.clone();
                set_cmd(&mut q, "scratch_apply");
                q
            }
            "poler_crystal_note" => {
                let mut q = args.clone();
                set_cmd(&mut q, "crystal_note");
                q
            }
            "poler_crystal_assoc" => {
                let mut q = args.clone();
                set_cmd(&mut q, "crystal_assoc");
                default_u64(&mut q, "sem_k", 4);
                default_u64(&mut q, "bi_k", 8);
                q
            }
            "poler_crystal_stats" => {
                let mut q = args.clone();
                set_cmd(&mut q, "crystal_stats");
                q
            }
            "poler_crystal_load" => {
                let mut q = args.clone();
                set_cmd(&mut q, "crystal_load");
                q
            }
            "poler_crystal_save" => {
                let mut q = args.clone();
                set_cmd(&mut q, "crystal_save");
                q
            }
            // ── Фаза 5+ «Живой движок»: пол демона poler-engine ──
            // Редактор как тонкий терминал: тяжёлые операции исполняет
            // живой движок (ε-поиск/grep через MCP-гейт демона; дотискание
            // форматов, листинг архивов, AIDDE impact, кристаллизация — CLI).
            "poler_engine_status" => {
                let mut q = args.clone();
                set_cmd(&mut q, "engine_status");
                q
            }
            "poler_engine_search" => {
                let mut q = args.clone();
                set_cmd(&mut q, "engine_search");
                if q.get("top").is_none() {
                    q["top"] = json!(5);
                }
                q
            }
            "poler_engine_grep" => {
                let mut q = args.clone();
                set_cmd(&mut q, "engine_grep");
                default_str(&mut q, "output", "content");
                q
            }
            "poler_engine_pack" => {
                let mut q = args.clone();
                set_cmd(&mut q, "engine_pack");
                q
            }
            "poler_engine_squeeze" => {
                let mut q = args.clone();
                set_cmd(&mut q, "engine_squeeze");
                q
            }
            "poler_engine_archive_ls" => {
                let mut q = args.clone();
                set_cmd(&mut q, "engine_archive_ls");
                q
            }
            "poler_engine_impact" => {
                let mut q = args.clone();
                set_cmd(&mut q, "engine_impact");
                q
            }
            "poler_engine_crystal_ingest" => {
                let mut q = args.clone();
                set_cmd(&mut q, "engine_crystal_ingest");
                q
            }
            unknown => {
                return tool_result(
                    json!({"ok": false, "error": format!("Неизвестный инструмент MCP: {unknown}")}),
                    true,
                );
            }
        };

        let resp = self.run_core_cmd(core_req);
        let is_err = !resp.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
        tool_result(resp, is_err)
    }
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new()
    }
}

fn set_cmd(v: &mut Value, cmd: &str) {
    if !v.is_object() {
        *v = json!({});
    }
    v["id"] = json!(1);
    v["cmd"] = json!(cmd);
}

fn default_bool(v: &mut Value, key: &str, default_val: bool) {
    if v.get(key).is_none() {
        v[key] = json!(default_val);
    }
}

fn default_str(v: &mut Value, key: &str, default_val: &str) {
    if v.get(key).and_then(|x| x.as_str()).is_none() {
        v[key] = json!(default_val);
    }
}

fn default_u64(v: &mut Value, key: &str, default_val: u64) {
    if v.get(key).and_then(|x| x.as_u64()).is_none() {
        v[key] = json!(default_val);
    }
}

fn tool_result(payload: Value, is_error: bool) -> Value {
    let pretty = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    json!({
        "content": [
            {
                "type": "text",
                "text": pretty
            }
        ],
        "structuredContent": payload,
        "isError": is_error
    })
}

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn rpc_err(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

/// Запустить MCP-сервер поверх `stdin` / `stdout`.
pub fn run_mcp_stdio() -> i32 {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();
    let mut server = McpServer::new();

    while let Some((msg, framing)) = read_mcp_message(&mut reader) {
        if let Some(resp) = server.handle_rpc(&msg) {
            if write_mcp_message(&mut writer, &resp, framing).is_err() {
                break;
            }
        }
    }
    0
}

fn read_mcp_message<R: BufRead>(reader: &mut R) -> Option<(Value, WireFraming)> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).ok()?;
        if n == 0 {
            return None; // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed
            .strip_prefix("Content-Length:")
            .or_else(|| trimmed.strip_prefix("content-length:"))
        {
            let content_len: usize = rest.trim().parse().ok()?;
            // Дочитываем заголовки до пустой строки `\r\n` / `\n`.
            loop {
                line.clear();
                let h = reader.read_line(&mut line).ok()?;
                if h == 0 || line.trim().is_empty() {
                    break;
                }
            }
            let mut body = vec![0u8; content_len];
            reader.read_exact(&mut body).ok()?;
            let val: Value = serde_json::from_slice(&body).ok()?;
            return Some((val, WireFraming::ContentLength));
        }
        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
            return Some((val, WireFraming::JsonLines));
        }
    }
}

fn write_mcp_message<W: Write>(
    writer: &mut W,
    msg: &Value,
    framing: WireFraming,
) -> io::Result<()> {
    let s = serde_json::to_string(msg)?;
    match framing {
        WireFraming::JsonLines => {
            writeln!(writer, "{s}")?;
        }
        WireFraming::ContentLength => {
            write!(writer, "Content-Length: {}\r\n\r\n{}", s.len(), s)?;
        }
    }
    writer.flush()
}

fn mcp_tools_catalog() -> Vec<Value> {
    vec![
        json!({
            "name": "poler_harvest",
            "description": "Полнодисковый сверхскоростной сбор файлов и поиск фрагментов кода с L1-L3 кэшем адресов (.t5a). Возвращает структурированные секции совпадений с номерами строк и байтовыми смещениями.",
            "inputSchema": {
                "type": "object",
                "required": ["roots", "query"],
                "properties": {
                    "roots": { "type": "array", "items": { "type": "string" }, "description": "Корневые каталоги для сканирования" },
                    "query": { "type": "string", "description": "Поисковый запрос / подстрока" },
                    "format": { "type": "string", "enum": ["sections", "markdown", "json", "address"], "default": "sections" },
                    "context": { "type": "integer", "default": 2, "description": "Число контекстных строк вокруг совпадения" }
                }
            }
        }),
        json!({
            "name": "poler_grep",
            "description": "Потоковый параллельный grep по дереву каталогов с учётом .gitignore (замена ripgrep). Быстрый поиск строк или regex с номерами строк, смещениями и контекстом -A/-B/-C.",
            "inputSchema": {
                "type": "object",
                "required": ["roots", "pattern"],
                "properties": {
                    "roots": { "type": "array", "items": { "type": "string" }, "description": "Каталоги или файлы" },
                    "pattern": { "type": "string", "description": "Искомая строка или регулярное выражение" },
                    "mode": { "type": "string", "enum": ["literal", "regex"], "default": "literal" },
                    "case_sensitive": { "type": "boolean", "default": true },
                    "context_before": { "type": "integer", "default": 0 },
                    "context_after": { "type": "integer", "default": 0 },
                    "max_results": { "type": "integer", "default": 1000 }
                }
            }
        }),
        json!({
            "name": "poler_read_span",
            "description": "Прочитать диапазон строк [line .. line+count) из файла любого размера (zero-copy mmap Piece-Table) БЕЗ обрезки длинных строк на 8192 байтах. Возвращает текст и текущую CAS-ревизию rev.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Путь к файлу на диске" },
                    "doc": { "type": "integer", "description": "Опциональный ID уже открытого буфера/форка" },
                    "line": { "type": "integer", "description": "Начальная строка (0-based)", "default": 0 },
                    "count": { "type": "integer", "description": "Количество строк для чтения", "default": 200 },
                    "max_bytes": { "type": "integer", "description": "Лимит байтов ответа (до 64 МиБ)", "default": 4194304 }
                }
            }
        }),
        json!({
            "name": "poler_replace_match",
            "description": "Контент-адресуемая замена подстроки old_text -> new_text без ручного подсчёта байтов. Автоматически адаптирует CRLF/LF, проверяет уникальность (или привязывает к near_line) и при check_syntax=true мгновенно откатывает правку при нарушении баланса скобок 𝕋≠0.",
            "inputSchema": {
                "type": "object",
                "required": ["old_text", "new_text"],
                "properties": {
                    "path": { "type": "string", "description": "Путь к файлу" },
                    "doc": { "type": "integer", "description": "Опциональный ID буфера/форка" },
                    "old_text": { "type": "string", "description": "Точный заменяемый фрагмент" },
                    "new_text": { "type": "string", "description": "Новый текст" },
                    "near_line": { "type": "integer", "description": "Опциональный ориентир строки (0-based) при нескольких вхождениях" },
                    "occurrence": { "type": "integer", "description": "1-based номер конкретного вхождения" },
                    "replace_all": { "type": "boolean", "default": false },
                    "check_syntax": { "type": "boolean", "default": true, "description": "Атомарный откат, если правка ломает баланс скобок 𝕋=0" },
                    "expected_rev": { "type": "integer", "description": "Опциональная CAS-защита ревизии буфера" },
                    "auto_save": { "type": "boolean", "default": true }
                }
            }
        }),
        json!({
            "name": "poler_apply_batch",
            "description": "Атомарная транзакция из нескольких правок (все или ничего, один шаг Undo, применение снизу вверх без сдвига смещений, синтаксический барьер 𝕋=0).",
            "inputSchema": {
                "type": "object",
                "required": ["ops"],
                "properties": {
                    "path": { "type": "string" },
                    "doc": { "type": "integer" },
                    "ops": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "description": "Элемент с op='replace_match' ({old_text,new_text,near_line?}) или op='replace_range' ({start_line,start_col,end_line,end_col,text})"
                        }
                    },
                    "check_syntax": { "type": "boolean", "default": true },
                    "expected_rev": { "type": "integer" },
                    "auto_save": { "type": "boolean", "default": true }
                }
            }
        }),
        json!({
            "name": "poler_apply_diff",
            "description": "Наложить стандартный Unified Diff (@@ -a,b +c,d @@) напрямую на Piece-Table буфер с нечётким поиском контекста ±fuzz строк и проверкой баланса скобок 𝕋=0.",
            "inputSchema": {
                "type": "object",
                "required": ["diff"],
                "properties": {
                    "path": { "type": "string" },
                    "doc": { "type": "integer" },
                    "diff": { "type": "string", "description": "Текст патча Unified Diff" },
                    "fuzz": { "type": "integer", "default": 3 },
                    "check_syntax": { "type": "boolean", "default": true },
                    "expected_rev": { "type": "integer" },
                    "auto_save": { "type": "boolean", "default": true }
                }
            }
        }),
        json!({
            "name": "poler_piece_diff",
            "description": "Сгенерировать Unified Diff несохранённых изменений буфера за O(pieces) БЕЗ сканирования нетронутых гигабайт mmap.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "doc": { "type": "integer" },
                    "context_lines": { "type": "integer", "default": 3 }
                }
            }
        }),
        json!({
            "name": "poler_search",
            "description": "Многопоточный SIMD / Aho-Corasick поиск по файлу любого размера с ε-значимостью, проективной плотностью [N:D] и Q8-резонансом. rank_by=\"epsilon\" (или \"resonance\") + min_epsilon отсекают шаблонный шум: 5000 тривиальных вхождений сжимаются до десятка смысловых фрагментов — экономия ~90% токенов агента.",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "path": { "type": "string" },
                    "doc": { "type": "integer" },
                    "query": { "type": "string" },
                    "case_sensitive": { "type": "boolean", "default": true },
                    "limit": { "type": "integer", "default": 50 },
                    "rank_by": {
                        "type": "string",
                        "enum": ["byte", "epsilon", "resonance"],
                        "default": "byte",
                        "description": "byte — порядок в файле; epsilon — по убыванию ε (смысловые первыми); resonance — по Q8-резонансу R"
                    },
                    "min_epsilon": {
                        "type": "number",
                        "description": "Порог фильтра ε (например 0.4): совпадения с меньшей значимостью отбрасываются (счётчик dropped_low_epsilon)"
                    }
                }
            }
        }),
        json!({
            "name": "poler_saccade",
            "description": "ε_high (макро-энергия, Виртуальные глаза ИИ): топологический скелет файла за 1 шаг — символы (fn/struct/impl/mod/заголовки Markdown) с координатами строк + пики построчной ε-значимости. Карта документа без чтения всего текста; прыгайте к узлу через poler_scroll(from_line=...) или poler_seek_energy.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Путь к файлу на диске" },
                    "doc": { "type": "integer", "description": "Опциональный ID открытого буфера" },
                    "max_nodes": { "type": "integer", "default": 120, "description": "Максимум узлов и пиков в ответе" }
                }
            }
        }),
        json!({
            "name": "poler_scroll",
            "description": "ε_low (микро-энергия) + построчный скролл с УДЕРЖАНИЕМ КУРСОРА: считывает N строк от текущей позиции виртуального глаза (line_cursor хранится ядром между вызовами). Фовеальное окно ИИ: читать гигабайтные файлы шагами, перечитывать любую строку остановки, двигаться вперёд/назад.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Путь к файлу на диске" },
                    "doc": { "type": "integer", "description": "Опциональный ID открытого буфера" },
                    "from_line": { "type": "integer", "description": "0-based; если опущено — продолжить с последней точки остановки курсора глаза" },
                    "lines": { "type": "integer", "default": 25 },
                    "direction": { "type": "string", "enum": ["forward", "backward"], "default": "forward" }
                }
            }
        }),
        json!({
            "name": "poler_seek_energy",
            "description": "Авто-саккада: прыжок курсора виртуального глаза к следующему/предыдущему пику ε (энергетическому всплеску значимости) от текущей позиции. Возвращает фовеальное окно 5 строк на пике.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Путь к файлу на диске" },
                    "doc": { "type": "integer", "description": "Опциональный ID открытого буфера" },
                    "direction": { "type": "string", "enum": ["next_peak", "prev_peak"], "default": "next_peak" },
                    "from_line": { "type": "integer", "description": "Опциональная стартовая позиция (иначе курсор глаза)" }
                }
            }
        }),
        json!({
            "name": "poler_verify_syntax",
            "description": "Мгновенная предкомпиляционная верификация троичной ленты скобок ({-1, 0, +1}, вакуум pack5 = 121) с точными координатами первой сломанной скобки.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "doc": { "type": "integer" }
                }
            }
        }),
        json!({
            "name": "poler_outline",
            "description": "Извлечь структурное дерево символов кода (fn, struct, class, impl, mod, enum) или заголовков Markdown с точными строками начала/конца скоупа (или breadcrumbs для конкретной line).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "doc": { "type": "integer" },
                    "line": { "type": "integer", "description": "Если указана, возвращает цепочку вложенных скоупов breadcrumbs для этой строки" }
                }
            }
        }),
        json!({
            "name": "poler_project_impact",
            "description": "AIDDE-анализ радиуса поражения символа по проектному CSR Call-Graph в pack5 (0.2 Б/связь в L3-кэше): определения, вызывающие функции (+1), мутирующие/unsafe вызовы (-1) и уровень риска.",
            "inputSchema": {
                "type": "object",
                "required": ["symbol"],
                "properties": {
                    "root": { "type": "string", "description": "Корневая директория проекта" },
                    "symbol": { "type": "string", "description": "Имя функции, метода или структуры" },
                    "refresh": { "type": "boolean", "default": false }
                }
            }
        }),
        json!({
            "name": "poler_semantic_search",
            "description": "Сверхбыстрый семантический поиск по скоупам проекта на 52-байтных троичных сигнатурах TritSig256 (SSSE3 psignb без умножителей + проективное ранжирование [N:D] + контрактный фильтр LENS).",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "root": { "type": "string" },
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "default": 15 },
                    "require_flags": { "type": "integer", "default": 0, "description": "Битовая маска: 1=pub, 2=fn, 4=type, 8=unsafe/mut, 16=test, 32=markdown" },
                    "forbid_flags": { "type": "integer", "default": 0 }
                }
            }
        }),
        json!({
            "name": "poler_fork_doc",
            "description": "Создать мгновенный zero-copy форк буфера за O(pieces) в оперативной памяти для параллельной проверки гипотез несколькими агентами без копирования файла.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "doc": { "type": "integer" }
                }
            }
        }),
        json!({
            "name": "poler_merge_fork",
            "description": "Трёхстороннее слияние спекулятивных форков (base_doc, ours_doc, theirs_doc) на троичной алгебре Клини 𝕋.",
            "inputSchema": {
                "type": "object",
                "required": ["base_doc", "ours_doc", "theirs_doc"],
                "properties": {
                    "base_doc": { "type": "integer" },
                    "ours_doc": { "type": "integer" },
                    "theirs_doc": { "type": "integer" },
                    "target_doc": { "type": "integer" },
                    "insert_markers": { "type": "boolean", "default": false }
                }
            }
        }),
        json!({
            "name": "poler_tx_save_all",
            "description": "Двухфазное транзакционное сохранение всех изменённых буферов с предварительной проверкой внешних конфликтов на диске (disk_conflict).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "docs": { "type": "array", "items": { "type": "integer" } },
                    "force": { "type": "boolean", "default": false }
                }
            }
        }),
        json!({
            "name": "poler_extract_doc",
            "description": "Офлайн-извлечение чистого структурированного текста из PDF, DOCX, EPUB, FB2, MOBI, HTML.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string" },
                    "max_chars": { "type": "integer", "default": 500000 }
                }
            }
        }),
        json!({
            "name": "poler_list_docs",
            "description": "Список всех открытых вкладок/буферов ядра, их CAS-ревизий rev и аппаратный CPUID-паспорт кремния Trit-Silicon.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),

        json!({
            "name": "poler_vfs_mount",
            "description": "Смонтировать любой архив/контейнер (.poler, .zip, .tar, .tar.gz, .epub...) в VFS-полигон БЕЗ распаковки: возвращает тип контейнера и листинг записей. Дальнейшие poler_scratch_put с provenance_path дедуплицируют черновики против содержимого архива в RAM.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "Путь к архиву/контейнеру на диске" }
                }
            }
        }),
        json!({
            "name": "poler_scratch_put",
            "description": "Сохранить заметку/черновик/граф гипотезы в RAM-полигон vfs://ai_scratch/ — НОЛЬ байт на диске (git чист, SSD не трогается). Content-defined нарезка FastCDC + BLAKE3: черновик, дублирующий источник, разделяет с ним чанки и не занимает лишнюю RAM. provenance_path (+provenance_entry для записи архива) регистрирует базу-источник.",
            "inputSchema": {
                "type": "object",
                "required": ["uri", "text"],
                "properties": {
                    "uri": { "type": "string", "description": "Имя записи, напр. hypo/refactor.md или полный vfs://ai_scratch/hypo/refactor.md" },
                    "text": { "type": "string", "description": "Текст заметки/черновика" },
                    "data_base64": { "type": "string", "description": "Альтернатива text: бинарные данные base64" },
                    "provenance": { "type": "string", "description": "URI источника для отображения в GUI (например file:///src/main.rs)" },
                    "provenance_path": { "type": "string", "description": "Путь файла-источника: его чанки станут общими с черновиком (дедуп RAM)" },
                    "provenance_entry": { "type": "string", "description": "Запись внутри архива provenance_path (с учётом CoW-оверлея)" }
                }
            }
        }),
        json!({
            "name": "poler_scratch_get",
            "description": "Пересобрать запись RAM-полигона из чанков (бит-в-бит) и вернуть текст (или base64 при text=false).",
            "inputSchema": {
                "type": "object",
                "required": ["uri"],
                "properties": {
                    "uri": { "type": "string", "description": "Имя записи или полный vfs://ai_scratch/... URI" },
                    "text": { "type": "boolean", "default": true, "description": "false — вернуть data_base64" }
                }
            }
        }),
        json!({
            "name": "poler_scratch_list",
            "description": "Список записей RAM-полигона с дедуп-статистикой: логический объём, физический след в RAM (только уникальные чанки), коэффициент дедупликации. Диск всегда 0 байт.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Подстрока-фильтр имён" }
                }
            }
        }),
        json!({
            "name": "poler_scratch_discard",
            "description": "Мгновенный откат идеи: стереть одну запись полигона (или всё поле all=true) за микросекунды. Сиротские чанки освобождаются, RAM возвращается.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uri": { "type": "string", "description": "Имя записи (либо all=true)" },
                    "all": { "type": "boolean", "default": false, "description": "Стереть весь полигон" }
                }
            }
        }),
        json!({
            "name": "poler_vfs_apply",
            "description": "Атомарная материализация выверенной идеи из полигона: target+entry → CoW-оверлей смонтированного архива (на диск — vfs_flush), либо target-файл → atomic write (tmp+fsync+rename). Полу-записанных файлов не бывает.",
            "inputSchema": {
                "type": "object",
                "required": ["uri", "target"],
                "properties": {
                    "uri": { "type": "string", "description": "Запись полигона для материализации" },
                    "target": { "type": "string", "description": "Путь файла на диске ИЛИ путь архива (тогда обязательно entry)" },
                    "entry": { "type": "string", "description": "Запись внутри архива target (режим CoW-оверлея)" }
                }
            }
        }),
        json!({
            "name": "poler_crystal_note",
            "description": "Прошить заметку в Кристалл Знаний (ассоциативная память агента): новые слова получают id за O(1) (Dynamic Vocab Expansion), пары соседей — Хеббовское укрепление троичных синапсов {−1,0,+1}. Кристалл переживает перезапуск через crystal_save/crystal_load (.t5c).",
            "inputSchema": {
                "type": "object",
                "required": ["text"],
                "properties": {
                    "text": { "type": "string", "description": "Текст заметки (структура бага, план, гипотеза)" }
                }
            }
        }),
        json!({
            "name": "poler_crystal_assoc",
            "description": "Резонансный запрос к Кристаллу: что связано с этим словом? Семантические соседи (CSE-триты золотой фазы) + партнёры совместной встречаемости (биграммы +1 в обе стороны). Subword-фолбек раскладывает неизвестное слово на подтокены.",
            "inputSchema": {
                "type": "object",
                "required": ["word"],
                "properties": {
                    "word": { "type": "string", "description": "Слово (неизвестное разложится на подтокены)" },
                    "sem_k": { "type": "integer", "default": 4, "description": "Сколько семантических соседей (1..32)" },
                    "bi_k": { "type": "integer", "default": 8, "description": "Сколько партнёров встречаемости (1..64)" }
                }
            }
        }),
        json!({
            "name": "poler_crystal_stats",
            "description": "Паспорт Кристалла Знаний сессии: словарь, корпус, плотность биграммной топологии, порог θ_hi, формат t5c/v1 и источник (зашитый crystal_ru.t5c или загруженный файл).",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "poler_crystal_load",
            "description": "Смонтировать Кристалл Знаний из файла .t5c (Trit5 Crystal v1): SHA-256 тела проверяется, повреждённый кристалл отвергается. Заменяет кристалл сессии.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "Путь к .t5c" },
                    "dims": { "type": "integer", "default": 96, "description": "Размерность CSE-вложений (16..512)" }
                }
            }
        }),
        json!({
            "name": "poler_crystal_save",
            "description": "Сохранить Кристалл сессии (со всеми Хеббовскими сдвигами и расширенным словарём) как детерминированный .t5c: заголовок 0x50, Trit5-упакованные биграммы, SHA-256 тела. Файл 20 КБ – 2 МБ, монтируется обратно за миллисекунды.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "Куда записать .t5c" }
                }
            }
        }),
        json!({
            "name": "poler_engine_status",
            "description": "Фаза 5+ «Живой движок»: статус поля демона poler-engine — где найден бинарь (PATH / ~/.local/bin / POLER_ENGINE_BIN / вендоренная копия), версия движка, состояние MCP-гейта (addr/pid/uptime). Демон поднимается лениво при первом engine_search / engine_grep.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "poler_engine_search",
            "description": "ε-поиск ЖИВОГО движка poler-engine по файлу/репозиторию через сетевой MCP-гейт: полные логические сцены (не строки grep), информационная плотность ε, резонанс R, K-hop связи. Демон тёплый — индекс живёт между вызовами.",
            "inputSchema": {
                "type": "object",
                "required": ["path", "query"],
                "properties": {
                    "path": { "type": "string", "description": "Файл или корень репозитория" },
                    "query": { "type": "string", "description": "Слово или фраза" },
                    "top": { "type": "integer", "default": 5, "minimum": 1, "maximum": 20 }
                }
            }
        }),
        json!({
            "name": "poler_engine_grep",
            "description": "Точный grep живого движка (замена ripgrep): ВСЕ совпадения, обход с .gitignore, контекст -A/-B, счётчики, машинно-читаемый JSON-отчёт. Гарантия полноты: «ноль значит ноль».",
            "inputSchema": {
                "type": "object",
                "required": ["pattern"],
                "properties": {
                    "path": { "type": "string", "default": ".", "description": "Файл или корень обхода" },
                    "pattern": { "type": "string", "description": "Искомая строка или regex" },
                    "regex": { "type": "boolean", "default": false },
                    "ignore_case": { "type": "boolean", "default": false },
                    "output": { "type": "string", "enum": ["content", "count", "list", "list_nonmatching"], "default": "content" },
                    "after": { "type": "integer", "default": 0, "minimum": 0, "maximum": 50 },
                    "before": { "type": "integer", "default": 0, "minimum": 0, "maximum": 50 },
                    "max_count": { "type": "integer", "minimum": 1 }
                }
            }
        }),
        json!({
            "name": "poler_engine_pack",
            "description": "Упаковка каталога в сжатый .poler живым движком (--pack): FastCDC + BLAKE3-дедуп между файлами + zstd, пофайловая таблица, права 0o755 сохраняются. Закон масштаба: повторы между файлами схлопываются. Возвращает статистику (files, raw_bytes, ratio, peak_rss_kb).",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "Каталог для упаковки (WYSIWYG: всё содержимое)" },
                    "out": { "type": "string", "description": "Выходной .poler (по умолчанию <путь>.poler)" },
                    "gitignore": { "type": "boolean", "default": false, "description": "Уважать .gitignore и пропускать скрытые (режим репо-чекаута)" },
                    "jobs": { "type": "integer", "default": 0, "description": "Потоки сжатия (0 = все ядра)" }
                }
            }
        }),
        json!({
            "name": "poler_engine_squeeze",
            "description": "Дотискание контейнера живым движком (--stream-file) — «закон семантики»: TAR/tar.gz/tar.zst/gz/zst РАЗВОРАЧИВАЮТСЯ НА ЛЕТУ, файлы ложатся в .poler пофайловой таблицей с BLAKE3-дедупом. ELF/PE64 (v0.94.0 BINFMT) сегментируются по секциям .text/.rdata: границы чанков совпадают с границами секций, записи pe:.text/elf:.rodata — дедупликация кода между соседними сборками. Сжатое сжимается ещё сильнее, потому что движок понимает формат.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "Файл-контейнер: tar / tar.gz / tar.zst / gz / zst / ELF / PE64 (.exe/.dll) / ..." },
                    "out": { "type": "string", "description": "Выходной .poler (по умолчанию <имя>.poler)" }
                }
            }
        }),
        json!({
            "name": "poler_engine_archive_ls",
            "description": "Листинг любого архива живым движком БЕЗ распаковки (--archive-list --archive-json): .poler, .zip, .tar, .tar.gz, .epub — записи, размеры, флаги сжатия/шифрования.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "Путь к архиву на диске" }
                }
            }
        }),
        json!({
            "name": "poler_engine_impact",
            "description": "AIDDE impact-радар живого движка: call graph + upstream/downstream паспорт символа по всему проекту (кто вызовет, кого затронет правка) — до компиляции. Работает на гигантских базах (--impact-cache SQLite).",
            "inputSchema": {
                "type": "object",
                "required": ["path", "symbol"],
                "properties": {
                    "path": { "type": "string", "description": "Корень проекта" },
                    "symbol": { "type": "string", "description": "Имя функции/метода/структуры" },
                    "depth": { "type": "integer", "description": "Глубина BFS (0 = по умолчанию движка)" }
                }
            }
        }),
        json!({
            "name": "poler_engine_crystal_ingest",
            "description": "Кристаллизация корпуса живым движком (--crystal-ingest-dir): каталог с текстами/кодом или .poler-архив → ассоциативный Кристалл Знаний .t5c (Trit5, троичные синапсы). Корпусная память для долгих сессий агента.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "Каталог с текстами или .poler-архив" },
                    "out": { "type": "string", "description": "Выходной .t5c (по умолчанию <имя>.t5c)" },
                    "vocab": { "type": "integer", "default": 0, "description": "Размер словаря (0 = авто: 4096)" }
                }
            }
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_initialize_list_and_call_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let fp = dir.path().join("engine.rs");
        std::fs::write(
            &fp,
            "pub fn compute_energy(x: i64) -> i64 {\n    x * 2\n}\n",
        )
        .unwrap();

        let mut srv = McpServer::new();

        // 1. initialize
        let init = srv
            .handle_rpc(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "2024-11-05" }
            }))
            .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "poler-edit-core");

        // 2. notification без id -> None
        assert!(srv
            .handle_rpc(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .is_none());

        // 3. tools/list -> 37 инструментов (без poler_calc, Фаза 5+ добавила 8 poler_engine_*)
        let list = srv
            .handle_rpc(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
            .unwrap();
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 39);
        let names: Vec<&str> = tools.iter()
            .filter_map(|t| t["name"].as_str()).collect();
        for expected in ["poler_engine_status", "poler_engine_search", "poler_engine_grep",
                         "poler_engine_pack", "poler_engine_squeeze", "poler_engine_archive_ls",
                         "poler_engine_impact", "poler_engine_crystal_ingest"] {
            assert!(names.contains(&expected), "нет инструмента {expected}");
        }

        // 4. tools/call poler_replace_match с синтаксическим барьером и автосохранением
        let call = srv
            .handle_rpc(&json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "poler_replace_match",
                    "arguments": {
                        "path": fp,
                        "old_text": "x * 2",
                        "new_text": "(x + 1) * 3"
                    }
                }
            }))
            .unwrap();
        assert_eq!(call["result"]["isError"], false);
        assert_eq!(
            call["result"]["structuredContent"]["syntax_balanced"],
            true
        );
        assert!(std::fs::read_to_string(&fp)
            .unwrap()
            .contains("(x + 1) * 3"));

    }

    #[test]
    fn mcp_virtual_eyes_saccade_scroll_seek() {
        let dir = tempfile::tempdir().unwrap();
        let fp = dir.path().join("eyes.rs");
        // 40 строк boilerplate + редкая структура + 40 строк boilerplate:
        // ε-пики обязаны указывать на редкие строки, а не на шаблон.
        let mut src = String::new();
        for _ in 0..40 {
            src.push_str("fn getter_x() -> u32 { self.value }\n");
        }
        src.push_str("pub struct QuantumLatticeAnchor { field_z: [u8; 17] }\n");
        src.push_str("impl QuantumLatticeAnchor { pub fn recalibrate_vortex(&self) -> u32 { 42 } }\n");
        for _ in 0..40 {
            src.push_str("fn getter_x() -> u32 { self.value }\n");
        }
        std::fs::write(&fp, src).unwrap();

        let mut srv = McpServer::new();

        // 1. Саккада: скелет документа за 1 шаг.
        let sac = srv.handle_rpc(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "poler_saccade", "arguments": { "path": fp } }
        })).unwrap();
        let sac_c = &sac["result"]["structuredContent"];
        assert_eq!(sac_c["ok"], true);
        assert_eq!(sac_c["total_lines"], 82);
        assert!(sac_c["nodes"].as_array().unwrap().len() >= 2);
        assert!(sac_c["peaks"].as_array().unwrap().len() >= 1);

        // 2. Скролл: фовеальное окно 4 строки от нулевого курсора.
        let scr = srv.handle_rpc(&json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "poler_scroll", "arguments": { "path": fp, "lines": 4 } }
        })).unwrap();
        let scr_c = &scr["result"]["structuredContent"];
        assert_eq!(scr_c["ok"], true);
        assert_eq!(scr_c["lines"].as_array().unwrap().len(), 4);
        assert_eq!(scr_c["cursor"]["line"], 4);

        // 3. Скролл без from_line ПРОДОЛЖАЕТ с курсора (удержание состояния).
        let scr2 = srv.handle_rpc(&json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "poler_scroll", "arguments": { "path": fp, "lines": 2 } }
        })).unwrap();
        assert_eq!(scr2["result"]["structuredContent"]["start_line"], 4);
        assert_eq!(scr2["result"]["structuredContent"]["cursor"]["line"], 6);

        // 4. Саккада-прыжок: poler_seek_energy ведёт глаз к пику ε.
        let seek = srv.handle_rpc(&json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": { "name": "poler_seek_energy",
                        "arguments": { "path": fp, "direction": "next_peak" } }
        })).unwrap();
        let seek_c = &seek["result"]["structuredContent"];
        assert_eq!(seek_c["ok"], true);
        assert_eq!(seek_c["found"], true);
        let peak_line = seek_c["peak"]["line"].as_u64().unwrap();
        assert!(peak_line == 40 || peak_line == 41, "peak_line={peak_line}");
        // Пик вернул фовеальное окно и передвинул курсор.
        assert!(seek_c["lines"].as_array().unwrap().len() >= 1);
        assert!(seek_c["cursor"]["line"].as_u64().unwrap() > peak_line);

        // 5. Поиск с ранжированием по ε: выдача упорядочена по убыванию.
        let srch = srv.handle_rpc(&json!({
            "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": { "name": "poler_search",
                        "arguments": { "path": fp, "query": "getter_x",
                                       "rank_by": "epsilon", "limit": 5 } }
        })).unwrap();
        let s_c = &srch["result"]["structuredContent"];
        assert_eq!(s_c["ok"], true);
        assert_eq!(s_c["ranked_by"], "epsilon");
        assert_eq!(s_c["hits"].as_array().unwrap().len(), 5);
        let eps: Vec<f64> = s_c["hits"].as_array().unwrap().iter()
            .map(|h| h["eps"].as_f64().unwrap()).collect();
        assert!(eps.windows(2).all(|w| w[0] >= w[1]), "ε должна убывать: {eps:?}");
    }
}
