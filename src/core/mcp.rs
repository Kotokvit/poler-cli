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
                        "instructions": "Sovereign POLER Edit Engine (Zero-Copy Piece-Table + Trit-Silicon 𝕋={-1,0,+1}, bracket barrier 𝕋=0, O(pieces) diff, pack5 CSR Call-Graph 0.2 B/edge). Virtual AI Eyes: poler_saccade (ε_high — full document landscape in 1 step) → poler_scroll (ε_low — foveal reading window with cursor anchor) → poler_seek_energy (jump to semantic peak). For editing: poler_replace_match / poler_apply_batch with check_syntax=true. poler_search with rank_by=\"epsilon\" and min_epsilon saves ~90% tokens; poler_project_impact — verify callers before modifying public signatures. Live Engine Gate: poler_engine_status (daemon/version), poler_engine_search / poler_engine_grep (ε-scenes and complete repo grep), poler_engine_pack / poler_engine_squeeze (stream container compression into .poler), poler_engine_archive_ls (in-place archive inspection), poler_engine_impact (AIDDE call-graph), poler_engine_crystal_ingest (corpus → Address Crystal .t5c)."
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
                crate::retrieval::grep::GrepMode::Regex
            } else {
                crate::retrieval::grep::GrepMode::Literal
            };
            let case_insensitive = args.get("case_insensitive").and_then(|x| x.as_bool()).unwrap_or(false);
            let before = args.get("context_before").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
            let after = args.get("context_after").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
            let max_count = args.get("max_count").and_then(|x| x.as_u64()).map(|n| n as usize);

            let cfg = crate::retrieval::grep::GrepConfig {
                pattern,
                mode,
                case_insensitive,
                before,
                after,
                max_count,
                ..Default::default()
            };
            return match crate::retrieval::grep::grep_run(&roots, &cfg) {
                Ok(rep) => {
                    let text = crate::retrieval::grep::render_text(&rep, false);
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
            "description": "Full-disk ultra-fast harvest and code fragment search with L1-L3 address cache (.t5a). Returns structured match sections with line numbers and byte offsets.",
            "inputSchema": {
                "type": "object",
                "required": ["roots", "query"],
                "properties": {
                    "roots": { "type": "array", "items": { "type": "string" }, "description": "Root directories to scan" },
                    "query": { "type": "string", "description": "Search query / substring" },
                    "format": { "type": "string", "enum": ["sections", "markdown", "json", "address"], "default": "sections" },
                    "context": { "type": "integer", "default": 2, "description": "Number of context lines around matches" }
                }
            }
        }),
        json!({
            "name": "poler_grep",
            "description": "Streaming parallel project grep with .gitignore awareness (replaces ripgrep). Fast literal or regex matching with line numbers, offsets, and -A/-B/-C context.",
            "inputSchema": {
                "type": "object",
                "required": ["roots", "pattern"],
                "properties": {
                    "roots": { "type": "array", "items": { "type": "string" }, "description": "Directories or files to scan" },
                    "pattern": { "type": "string", "description": "Search string or regular expression" },
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
            "description": "Read line span [line .. line+count) from a file of any size (zero-copy mmap Piece-Table) without line truncation. Returns text and current CAS revision rev.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to file on disk" },
                    "doc": { "type": "integer", "description": "Optional ID of already open buffer/fork" },
                    "line": { "type": "integer", "description": "Starting line (0-based)", "default": 0 },
                    "count": { "type": "integer", "description": "Number of lines to read", "default": 200 },
                    "max_bytes": { "type": "integer", "description": "Maximum response bytes (up to 64 MiB)", "default": 4194304 }
                }
            }
        }),
        json!({
            "name": "poler_replace_match",
            "description": "Content-addressed replacement of old_text -> new_text without manual byte counting. Automatically adapts CRLF/LF, checks uniqueness (or binds to near_line), and atomically rolls back on bracket imbalance 𝕋≠0 when check_syntax=true.",
            "inputSchema": {
                "type": "object",
                "required": ["old_text", "new_text"],
                "properties": {
                    "path": { "type": "string", "description": "Path to file" },
                    "doc": { "type": "integer", "description": "Optional ID of open buffer/fork" },
                    "old_text": { "type": "string", "description": "Exact text snippet to replace" },
                    "new_text": { "type": "string", "description": "New replacement text" },
                    "near_line": { "type": "integer", "description": "Optional line hint (0-based) for disambiguation" },
                    "occurrence": { "type": "integer", "description": "1-based index of specific occurrence" },
                    "replace_all": { "type": "boolean", "default": false },
                    "check_syntax": { "type": "boolean", "default": true, "description": "Atomic rollback if edit breaks bracket balance 𝕋=0" },
                    "expected_rev": { "type": "integer", "description": "Optional CAS buffer revision guard" },
                    "auto_save": { "type": "boolean", "default": true }
                }
            }
        }),
        json!({
            "name": "poler_apply_batch",
            "description": "Atomic multi-edit transaction (all or nothing, single undo step, bottom-up application without offset shifting, 𝕋=0 syntax barrier).",
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
                            "description": "Item with op='replace_match' ({old_text,new_text,near_line?}) or op='replace_range' ({start_line,start_col,end_line,end_col,text})"
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
            "description": "Apply standard Unified Diff (@@ -a,b +c,d @@) directly onto Piece-Table buffer with ±fuzz context tolerance and 𝕋=0 bracket check.",
            "inputSchema": {
                "type": "object",
                "required": ["diff"],
                "properties": {
                    "path": { "type": "string" },
                    "doc": { "type": "integer" },
                    "diff": { "type": "string", "description": "Unified Diff patch text" },
                    "fuzz": { "type": "integer", "default": 3 },
                    "check_syntax": { "type": "boolean", "default": true },
                    "expected_rev": { "type": "integer" },
                    "auto_save": { "type": "boolean", "default": true }
                }
            }
        }),
        json!({
            "name": "poler_piece_diff",
            "description": "Generate Unified Diff of unsaved buffer changes in O(pieces) without scanning untouched gigabytes of mmap.",
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
            "description": "Multi-threaded SIMD / Aho-Corasick search across any file size with ε-significance, projective density [N:D], and Q8 resonance. rank_by=\"epsilon\" (or \"resonance\") + min_epsilon prune boilerplate: 5000 trivial matches compressed to top semantic fragments — saves ~90% agent tokens.",
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
                        "description": "byte — file order; epsilon — descending significance ε (semantic matches first); resonance — Q8 resonance R"
                    },
                    "min_epsilon": {
                        "type": "number",
                        "description": "Filter threshold ε (e.g. 0.4): lower significance matches dropped"
                    }
                }
            }
        }),
        json!({
            "name": "poler_saccade",
            "description": "Virtual AI Eye: Saccade. One-step document topological map (outline, top energy peaks ε, AST anchors). Inspect full structure without loading entire file; jump to any node via poler_scroll(from_line=...) or poler_seek_energy.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to file on disk" },
                    "doc": { "type": "integer", "description": "Optional open buffer ID" },
                    "max_nodes": { "type": "integer", "default": 120, "description": "Maximum nodes and peaks in response" }
                }
            }
        }),
        json!({
            "name": "poler_scroll",
            "description": "Virtual AI Eye: Smooth reading window with cursor tracking. Reads N lines from active position (kernel maintains line_cursor across calls). Foveal reading window for arbitrary file sizes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to file on disk" },
                    "doc": { "type": "integer", "description": "Optional open buffer ID" },
                    "from_line": { "type": "integer", "description": "0-based start line; if omitted continues from current cursor" },
                    "lines": { "type": "integer", "default": 25 },
                    "direction": { "type": "string", "enum": ["forward", "backward"], "default": "forward" }
                }
            }
        }),
        json!({
            "name": "poler_seek_energy",
            "description": "Virtual AI Eye: Jump to semantic energy peak (function headers, classes, core algorithms, high-density zones). Returns 5-line foveal window around peak.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to file on disk" },
                    "doc": { "type": "integer", "description": "Optional open buffer ID" },
                    "direction": { "type": "string", "enum": ["next_peak", "prev_peak"], "default": "next_peak" },
                    "from_line": { "type": "integer", "description": "Optional starting line (defaults to active cursor)" }
                }
            }
        }),
        json!({
            "name": "poler_verify_syntax",
            "description": "Instant pre-compilation verification of balanced bracket tape ({-1, 0, +1}, pack5 vacuum = 121) with coordinates of first broken bracket.",
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
            "description": "Extract code symbol tree (fn, struct, class, impl, mod, enum) or Markdown headings with exact scope bounds (or breadcrumbs for a specific line).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "doc": { "type": "integer" },
                    "line": { "type": "integer", "description": "If specified, returns chain of enclosing breadcrumbs for this line" }
                }
            }
        }),
        json!({
            "name": "poler_project_impact",
            "description": "AIDDE project call-graph blast radius: upstream callers (+1), mutating/unsafe calls (-1), and risk level across repository.",
            "inputSchema": {
                "type": "object",
                "required": ["symbol"],
                "properties": {
                    "root": { "type": "string", "description": "Project root directory" },
                    "symbol": { "type": "string", "description": "Function, method, or struct name" },
                    "refresh": { "type": "boolean", "default": false }
                }
            }
        }),
        json!({
            "name": "poler_semantic_search",
            "description": "Fast semantic search over project scopes using 52-byte ternary signatures TritSig256 (SSSE3 psignb without multipliers + projective [N:D] ranking + LENS filter).",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "root": { "type": "string" },
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "default": 15 },
                    "require_flags": { "type": "integer", "default": 0, "description": "Bitmask: 1=pub, 2=fn, 4=type, 8=unsafe/mut, 16=test, 32=markdown" },
                    "forbid_flags": { "type": "integer", "default": 0 }
                }
            }
        }),
        json!({
            "name": "poler_fork_doc",
            "description": "Create instant zero-copy buffer fork in RAM in O(pieces) for speculative edits without touching disk.",
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
            "description": "Three-way merge of speculative forks (base_doc, ours_doc, theirs_doc) on Kleene ternary algebra 𝕋.",
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
            "description": "Two-phase transactional commit of all modified buffers with external disk conflict check (no files overwritten on conflict).",
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
            "description": "Extract structured Markdown text from PDF, DOCX, EPUB, FB2, MOBI, HTML.",
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
            "description": "List all open kernel buffers, CAS revisions rev, and CPUID hardware capabilities.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),

        json!({
            "name": "poler_vfs_mount",
            "description": "Mount any archive container (.poler, .zip, .tar, .tar.gz, .epub...) into VFS polygon without extraction: returns container kind and entry listing. Subsequent poler_scratch_put calls with provenance_path deduplicate drafts against archive contents in RAM.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "Path to archive/container on disk" }
                }
            }
        }),
        json!({
            "name": "poler_scratch_put",
            "description": "Save note/draft/hypothesis graph to RAM polygon vfs://ai_scratch/ — ZERO bytes on disk (git clean, SSD untouched). Content-defined chunking with FastCDC + BLAKE3 deduplication.",
            "inputSchema": {
                "type": "object",
                "required": ["uri", "text"],
                "properties": {
                    "uri": { "type": "string", "description": "Entry name, e.g. hypo/refactor.md or full vfs://ai_scratch/hypo/refactor.md" },
                    "text": { "type": "string", "description": "Text content of note/draft" },
                    "data_base64": { "type": "string", "description": "Alternative to text: base64-encoded binary data" },
                    "provenance": { "type": "string", "description": "Source URI for GUI display (e.g. file:///src/main.rs)" },
                    "provenance_path": { "type": "string", "description": "Source file path: chunks shared with draft for RAM deduplication" },
                    "provenance_entry": { "type": "string", "description": "Entry within archive provenance_path" }
                }
            }
        }),
        json!({
            "name": "poler_scratch_get",
            "description": "Reassemble RAM polygon entry bit-for-bit from chunks and return text (or base64 if text=false).",
            "inputSchema": {
                "type": "object",
                "required": ["uri"],
                "properties": {
                    "uri": { "type": "string", "description": "Entry name or full vfs://ai_scratch/... URI" },
                    "text": { "type": "boolean", "default": true, "description": "false to return data_base64" }
                }
            }
        }),
        json!({
            "name": "poler_scratch_list",
            "description": "List RAM polygon entries with deduplication stats: logical size, physical RAM footprint (unique chunks only), deduplication ratio. Disk usage always 0 bytes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Substring filter for names" }
                }
            }
        }),
        json!({
            "name": "poler_scratch_discard",
            "description": "Instant rollback: delete a single polygon entry (or all if all=true) in microseconds. Orphan chunks freed, RAM returned.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uri": { "type": "string", "description": "Entry name (or all=true)" },
                    "all": { "type": "boolean", "default": false, "description": "Clear entire polygon" }
                }
            }
        }),
        json!({
            "name": "poler_vfs_apply",
            "description": "Atomic materialization of draft from polygon: target+entry → CoW overlay of mounted archive (persisted via vfs_flush), or target file → atomic write (tmp+fsync+rename). No half-written files.",
            "inputSchema": {
                "type": "object",
                "required": ["uri", "target"],
                "properties": {
                    "uri": { "type": "string", "description": "Polygon entry to materialize" },
                    "target": { "type": "string", "description": "Disk file path OR archive path (entry required for archive)" },
                    "entry": { "type": "string", "description": "Entry inside target archive (CoW overlay mode)" }
                }
            }
        }),
        json!({
            "name": "poler_crystal_note",
            "description": "Embed note into Knowledge Crystal (agent associative memory): new words registered in O(1), neighbor pairs receive Hebbian ternary synapse strengthening {−1,0,+1}.",
            "inputSchema": {
                "type": "object",
                "required": ["text"],
                "properties": {
                    "text": { "type": "string", "description": "Note text (bug anatomy, plan, hypothesis)" }
                }
            }
        }),
        json!({
            "name": "poler_crystal_assoc",
            "description": "Resonant query to Knowledge Crystal: find associated concepts, semantic neighbors (CSE trits), and co-occurrence partners.",
            "inputSchema": {
                "type": "object",
                "required": ["word"],
                "properties": {
                    "word": { "type": "string", "description": "Seed word or concept" },
                    "sem_k": { "type": "integer", "default": 4, "description": "Number of semantic neighbors (1..32)" },
                    "bi_k": { "type": "integer", "default": 8, "description": "Number of co-occurrence partners (1..64)" }
                }
            }
        }),
        json!({
            "name": "poler_crystal_stats",
            "description": "Knowledge Crystal session passport: vocabulary size, corpus stats, bigram density, threshold θ_hi, format t5c/v1.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "poler_crystal_load",
            "description": "Mount Knowledge Crystal from .t5c file (Trit5 Crystal v1): SHA-256 integrity verified. Replaces session crystal.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "Path to .t5c" },
                    "dims": { "type": "integer", "default": 96, "description": "CSE embedding dimension (16..512)" }
                }
            }
        }),
        json!({
            "name": "poler_crystal_save",
            "description": "Persist session Knowledge Crystal as deterministic .t5c file (Trit5-packed bigrams, SHA-256 payload checksum). Fast reload in milliseconds.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "Destination .t5c path" }
                }
            }
        }),
        json!({
            "name": "poler_engine_status",
            "description": "Live Engine Gate: check status of poler-engine daemon binary location, version, and MCP gate connectivity (addr/pid/uptime).",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "poler_engine_search",
            "description": "Semantic ε-search via live poler-engine gate: full logical scenes, information density ε, resonance R, K-hop connectivity across repository.",
            "inputSchema": {
                "type": "object",
                "required": ["path", "query"],
                "properties": {
                    "path": { "type": "string", "description": "Target file or repository root" },
                    "query": { "type": "string", "description": "Query term or phrase" },
                    "top": { "type": "integer", "default": 5, "minimum": 1, "maximum": 20 }
                }
            }
        }),
        json!({
            "name": "poler_engine_grep",
            "description": "Exact grep via live engine: exhaustive matches, .gitignore traversal, -A/-B context, structured JSON report.",
            "inputSchema": {
                "type": "object",
                "required": ["pattern"],
                "properties": {
                    "path": { "type": "string", "default": ".", "description": "File or traversal root" },
                    "pattern": { "type": "string", "description": "Search pattern or regex" },
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
            "description": "Pack directory into compressed .poler archive (--pack): FastCDC + BLAKE3 deduplication + zstd, preserving permissions. Inter-file redundancies collapsed.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "Directory to pack" },
                    "out": { "type": "string", "description": "Output .poler path (defaults to <path>.poler)" },
                    "gitignore": { "type": "boolean", "default": false, "description": "Respect .gitignore and skip hidden files" },
                    "jobs": { "type": "integer", "default": 0, "description": "Compression threads (0 = all cores)" }
                }
            }
        }),
        json!({
            "name": "poler_engine_squeeze",
            "description": "Stream-compress container into .poler format with semantic deduplication: unpacks TAR/tar.gz/tar.zst on the fly with per-file table and section-aware chunk boundaries.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "Container path: tar / tar.gz / tar.zst / gz / zst / ELF / PE64 / ..." },
                    "out": { "type": "string", "description": "Output .poler path (defaults to <name>.poler)" }
                }
            }
        }),
        json!({
            "name": "poler_engine_archive_ls",
            "description": "Zero-copy inspection and listing of any archive without extraction: .poler, .zip, .tar, .tar.gz, .epub.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "Path to archive on disk" }
                }
            }
        }),
        json!({
            "name": "poler_engine_impact",
            "description": "AIDDE impact radar: call graph + upstream/downstream symbol dependencies across entire project.",
            "inputSchema": {
                "type": "object",
                "required": ["path", "symbol"],
                "properties": {
                    "path": { "type": "string", "description": "Project root directory" },
                    "symbol": { "type": "string", "description": "Target function, method, or struct name" },
                    "depth": { "type": "integer", "description": "BFS traversal depth" }
                }
            }
        }),
        json!({
            "name": "poler_engine_crystal_ingest",
            "description": "Crystallize text or code corpus into associative Knowledge Crystal .t5c (Trit5 ternary synapses) for persistent multi-turn agent memory.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "Text directory or .poler archive" },
                    "out": { "type": "string", "description": "Output .t5c path (defaults to <name>.t5c)" },
                    "vocab": { "type": "integer", "default": 0, "description": "Vocabulary size limit (0 = auto: 4096)" }
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
