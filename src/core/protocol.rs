//! poler-edit :: редакторский и агентный серверный протокол (JSON lines + TCP Live Bridge).
//!
//! `poler-edit-core`:
//! * stdin  — одна JSON-команда на строку: `{"id":N,"cmd":"open","path":...}`
//! * stdout — ответы `{"id":N,"ok":true,...}` и события `{"ev":"progress"|"hits"|"agent_edit",...}`
//! * `--listen 127.0.0.1:0` — поднимает локальный коллаборативный сокет для ИИ-агентов
//!   (выдаёт в stdout `{"ev":"listen_ready","addr":...,"token":...}`). Когда внешний агент
//!   мутирует открытую вкладку, GUI мгновенно получает `{"ev":"agent_edit",...}`.

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::agent_ops::{merge_three_way, BatchEditOp, MAX_EDIT_PAYLOAD_BYTES, MAX_READ_SPAN_BYTES};
use crate::buffer::PolerBuffer;
use crate::license;
use crate::search::{self, HitOut, LiveSpec, SearchEvents};
use crate::trit_silicon::{caps, TRIT5_ZERO_CODE};
use crate::trit_syntax::{
    breadcrumbs_for_line, extract_outline_from_bytes, verify_snapshot_syntax, TritProjectGraph,
};

/// Один процесс — много документов (вкладки GUI + спекулятивные форки ИИ-агентов).
pub struct Docs {
    map: HashMap<u64, PolerBuffer>,
    next: u64,
}

impl Docs {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            next: 1,
        }
    }

    pub fn create(&mut self, b: PolerBuffer) -> u64 {
        let id = self.next;
        self.next += 1;
        self.map.insert(id, b);
        id
    }

    /// Найти уже открытый документ по каноническому/нормализованному пути.
    pub fn find_by_path(&self, target: &Path) -> Option<u64> {
        let target_norm = normalize_path_key(target);
        for (&id, buf) in &self.map {
            if normalize_path_key(buf.path()) == target_norm {
                return Some(id);
            }
        }
        None
    }
}

impl Default for Docs {
    fn default() -> Self {
        Self::new()
    }
}

fn normalize_path_key(p: &Path) -> String {
    if let Ok(canon) = std::fs::canonicalize(p) {
        let s = canon.to_string_lossy().replace('\\', "/");
        #[cfg(windows)]
        {
            return s.to_lowercase();
        }
        #[cfg(not(windows))]
        {
            return s;
        }
    }
    let s = p.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    {
        s.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        s
    }
}

struct CachedGraph {
    root_key: String,
    built_at: Instant,
    graph: TritProjectGraph,
}

/// Единое состояние ядра `poler-edit-core`, используемое в stdio, TCP-мосте, MCP и `--exec`.
pub struct CoreSession {
    pub docs: Docs,
    pub cancel: Arc<AtomicBool>,
    sessions: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
    sid_counter: Arc<AtomicU64>,
    cached_graph: Option<CachedGraph>,
    // Виртуальные глаза ИИ (см. search::line_energy_profile): курсор «глаза»
    // на строку каждого документа + кэш построчного ε-профиля,
    // инвалидированный CAS-ревизией (любая правка пересчитывает профиль).
    eye_cursors: Mutex<HashMap<u64, u64>>,
    energy_cache: Mutex<Option<(u64, u64, Arc<Vec<f32>>, Arc<Vec<search::EnergyPeak>>)>>,
    // Фаза 3 «Редактирование в архиве»: CoW-оверлеи смонтированных
    // контейнеров (ключ — normalize_path_key архива) и привязка
    // vfs-вкладок (doc → архив + запись внутри).
    vfs_overlays: HashMap<String, crate::vfs_edit::VfsOverlay>,
    vfs_docs: HashMap<u64, (PathBuf, String)>,
    // Фаза 5 «MCP-глаза на VFS»: RAM-полигон агента vfs://ai_scratch/
    // (content-addressed стор чанков FastCDC+BLAKE3 — ноль байт на диске)
    // и Кристалл Знаний .t5c — ассоциативная память агента (ленивая
    // инициализация зашитым crystal_ru.t5c движка).
    scratch: crate::scratch::ScratchPad,
    crystal: Option<crate::crystal::Crystal>,
    crystal_source: Option<String>,
    // Фаза 5+ «Живой движок»: пол демона poler-engine (ленивый spawn
    // при первом engine_*-запросе; loopback + 128-битный токен; убивается
    // в Drop и через PR_SET_PDEATHSIG — не переживает ядро редактора).
    engine: Option<crate::engine_gate::EngineDaemon>,
    // Фаза 6.D «AI Chat»: реестр писателей подключённых агентов TCP-моста.
    // Проставляется в run_edit_server_with_listen; сам чат — это
    // chat_broadcast (GUI → все агенты) и chat_send (агент → GUI-событие).
    pub agent_hub: Option<Arc<AgentHub>>,
    // Фаза 7: UDS Event Plane для живого диффа и перехвата намерений шелла
    pub event_plane: Option<Arc<crate::event_plane::EventPlane>>,
}

/// Фаза 6.D «AI Chat»: хаб писателей подключённых ИИ-агентов.
/// Живой канал «Человек ↔ ИИ» в доке редактора: человеческие сообщения
/// транслируются ВСЕМ аутентифицированным агентам, ответы агентов
/// приходят GUI событием `agent_chat`. Ровно тот же loopback+token
/// мост, по которому работают agent_edit-мутации буферов.
pub struct AgentHub {
    next_id: AtomicU64,
    writers: Mutex<Vec<(u64, Arc<Mutex<TcpStream>>)>>,
}

impl AgentHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            next_id: AtomicU64::new(1),
            writers: Mutex::new(Vec::new()),
        })
    }

    /// Зарегистрировать соединение агента (клонируем поток для записи).
    fn register(&self, stream: &TcpStream) -> Option<u64> {
        let clone = stream.try_clone().ok()?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.writers.lock().unwrap().push((id, Arc::new(Mutex::new(clone))));
        Some(id)
    }

    /// Снять соединение с регистрации (агент отключился).
    fn unregister(&self, id: u64) {
        self.writers.lock().unwrap().retain(|(i, _)| *i != id);
    }

    /// Написать всем агентам JSON-объект; мёртвые соединения отбрасываются.
    /// Возвращает число доставленных.
    fn broadcast(&self, obj: &Value) -> usize {
        use std::io::Write as _;
        let payload = format!("{obj}\n");
        let mut ws = self.writers.lock().unwrap();
        ws.retain(|(_, w)| {
            let mut g = w.lock().unwrap();
            g.write_all(payload.as_bytes()).and_then(|_| g.flush()).is_ok()
        });
        ws.len()
    }

    /// Сколько агентов на связи (для индикатора в панели чата).
    fn count(&self) -> usize {
        self.writers.lock().unwrap().len()
    }
}


impl CoreSession {
    pub fn new() -> Self {
        Self {
            docs: Docs::new(),
            cancel: Arc::new(AtomicBool::new(false)),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            sid_counter: Arc::new(AtomicU64::new(0)),
            cached_graph: None,
            eye_cursors: Mutex::new(HashMap::new()),
            energy_cache: Mutex::new(None),
            vfs_overlays: HashMap::new(),
            vfs_docs: HashMap::new(),
            scratch: crate::scratch::ScratchPad::new(),
            crystal: None,
            agent_hub: None,
            crystal_source: None,
            engine: None,
            event_plane: None,
        }
    }

    /// Демон poler-engine (ленивый spawn). Первый engine_search/engine_grep
    /// поднимает `--mcp-http` на случайном loopback-порту; далее гейт живёт
    /// до конца сессии (Drop убивает ребёнка).
    fn engine_daemon_mut(&mut self) -> Result<&mut crate::engine_gate::EngineDaemon, String> {
        if self.engine.is_none() {
            let bin = crate::engine_gate::find_engine_bin().ok_or_else(|| {
                "poler-engine не найден: установите движок в PATH или ~/.local/bin, \
                 задайте POLER_ENGINE_BIN или соберите вендоренную копию \
                 poler-engine/target/release/poler-engine".to_string()
            })?;
            let d = crate::engine_gate::EngineDaemon::spawn(bin)
                .map_err(|e| format!("демон движка: {e}"))?;
            // ЛЕНИВЫЙ спавн — ТИХИЙ: событие up в stdout ДО ответа ломало бы
            // построчных клиентов ядра (smoke-скрипты, терминалы). События
            // engine_daemon летят только с явных engine_daemon_start/stop;
            // ленивый подъём панель видит опросом engine_status (≤ 2 с).
            self.engine = Some(d);
        }
        Ok(self.engine.as_mut().unwrap())
    }

    /// Кристалл сессии (ленивая инициализация зашитым crystal_ru.t5c
    /// движка — 22.8 КБ русской топологии, всегда под рукой).
    fn crystal_mut(&mut self) -> Result<&mut crate::crystal::Crystal, String> {
        if self.crystal.is_none() {
            self.crystal = Some(
                crate::crystal::Crystal::embedded()
                    .map_err(|e| format!("embedded crystal: {e}"))?,
            );
            self.crystal_source = Some("embedded:crystal_ru.t5c".into());
        }
        Ok(self.crystal.as_mut().unwrap())
    }

    /// Построчный ε-профиль документа с кэшированием по (doc, rev).
    /// O(N) пересчёт после любой правки (rev изменился); нетронутые буферы
    /// (mmap-кусок) читаются zero-copy. Профиль — основа poler_saccade
    /// (ε_high) и poler_seek_energy.
    fn eye_profile_and_peaks(
        &mut self,
        doc: u64,
    ) -> Result<(Arc<Vec<f32>>, Arc<Vec<search::EnergyPeak>>), String> {
        let (rev, snap) = {
            let b = self
                .docs
                .map
                .get(&doc)
                .ok_or_else(|| format!("doc_not_found: no open document doc={doc}"))?;
            (b.rev(), b.snapshot())
        };
        if let Some((cd, cr, prof, peaks)) = self.energy_cache.lock().unwrap().clone() {
            if cd == doc && cr == rev {
                return Ok((prof, peaks));
            }
        }
        let prof = Arc::new(crate::search::line_energy_profile(&snap));
        // Пики считаются ОДИН раз вместе с профилем: повторные saccade /
        // seek_energy на нетронутом документе — микросекунды, а не проход
        // по 37 млн строк.
        let peaks = Arc::new(crate::search::energy_peaks(&prof));
        *self.energy_cache.lock().unwrap() =
            Some((doc, rev, Arc::clone(&prof), Arc::clone(&peaks)));
        Ok((prof, peaks))
    }

    pub fn cancel_all(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Ok(s) = self.sessions.lock() {
            for tok in s.values() {
                tok.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Разрешить документ: либо по явному `doc`, либо по `path` (подключаясь к уже открытой
    /// вкладке или открывая файл с диска налету).
    fn resolve_or_open_doc(&mut self, v: &Value) -> Result<u64, String> {
        let explicit = doc_of(v);
        if explicit != 0 {
            if self.docs.map.contains_key(&explicit) {
                return Ok(explicit);
            }
            return Err(format!("doc_not_found: no open document doc={explicit}"));
        }
        if let Some(p) = req_path(v) {
            if !p.as_os_str().is_empty() {
                if let Some(existing) = self.docs.find_by_path(&p) {
                    return Ok(existing);
                }
                let buf = open_buffer_for_path(&p).map_err(|e| e.to_string())?;
                return Ok(self.docs.create(buf));
            }
        }
        // Авто-фокус: если документ не указан, берем последнюю открытую вкладку
        if let Some(&latest) = self.docs.map.keys().max() {
            return Ok(latest);
        }
        Err("missing_doc_or_path: no open documents and path not specified".to_string())
    }

    fn get_or_build_graph(&mut self, root: &Path, force_rebuild: bool) -> &TritProjectGraph {
        let key = normalize_path_key(root);
        let ttl_ok = self
            .cached_graph
            .as_ref()
            .map(|c| !force_rebuild && c.root_key == key && c.built_at.elapsed() < Duration::from_secs(15))
            .unwrap_or(false);
        if !ttl_ok {
            let g = TritProjectGraph::scan_workspace(root, 2000);
            self.cached_graph = Some(CachedGraph {
                root_key: key,
                built_at: Instant::now(),
                graph: g,
            });
        }
        &self.cached_graph.as_ref().unwrap().graph
    }

    /// Исполнить одну JSON-команду. Возвращает `None` только для `quit` (от главного клиента).
    pub fn execute(
        &mut self,
        v: &Value,
        from_agent: bool,
        gui_tx: Option<&mpsc::Sender<String>>,
    ) -> Option<Value> {
        let id = v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
        let cmd = v.get("cmd").or_else(|| v.get("method")).and_then(|x| x.as_str()).unwrap_or("");

        if cmd == "quit" {
            if from_agent {
                return Some(json!({"id": id, "ok": true, "bye": true}));
            }
            return None;
        }
        if cmd == "cancel" {
            self.cancel_all();
            return Some(json!({"id": id, "ok": true, "cancelled": true}));
        }

        self.cancel.store(false, Ordering::Relaxed);

        let resp = match cmd {
            "open" => {
                let p = req_path(v).unwrap_or_default();
                let reuse = v.get("reuse").and_then(|x| x.as_bool()).unwrap_or(from_agent);
                if reuse && !p.as_os_str().is_empty() {
                    if let Some(existing) = self.docs.find_by_path(&p) {
                        let b = &self.docs.map[&existing];
                        return Some(json!({
                            "id": id, "ok": true, "doc": existing, "reused": true,
                            "path": b.path(),
                            "bytes": b.total_len(),
                            "pieces": b.pieces_len(),
                            "lines": b.lines_if_known(),
                            "rev": b.rev(),
                            "edited": b.is_dirty(),
                        }));
                    }
                }
                match open_buffer_for_path(&p) {
                    Ok(b) => {
                        let d = self.docs.create(b);
                        let b = &self.docs.map[&d];
                        if let Some(ref ep) = self.event_plane {
                            ep.registry().register(d, &p, b.is_dirty(), String::new(), b.total_len());
                        }
                        json!({
                            "id": id, "ok": true, "doc": d,
                            "path": p,
                            "bytes": b.total_len(),
                            "pieces": b.pieces_len(),
                            "lines": b.lines_if_known(),
                            "rev": b.rev(),
                        })
                    }
                    Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                }
            }

            "list_docs" => {
                let mut items: Vec<(u64, Value)> = self
                    .docs
                    .map
                    .iter()
                    .map(|(&d, b)| {
                        let s = b.stats();
                        (
                            d,
                            json!({
                                "doc": d,
                                "path": s.path,
                                "bytes": s.bytes,
                                "pieces": s.pieces,
                                "lines": s.lines,
                                "edited": s.edited,
                                "rev": s.rev,
                                "disk_changed": s.disk_changed,
                            }),
                        )
                    })
                    .collect();
                items.sort_by_key(|(d, _)| *d);
                let docs_arr: Vec<Value> = items.into_iter().map(|(_, val)| val).collect();
                json!({"id": id, "ok": true, "docs": docs_arr})
            }

            "close" => {
                let doc = doc_of(v);
                if let Ok(mut s) = self.sessions.lock() {
                    if let Some(tok) = s.remove(&doc) {
                        tok.store(true, Ordering::Relaxed);
                    }
                }
                self.docs.map.remove(&doc);
                self.vfs_docs.remove(&doc); // Фаза 3: привязка vfs-вкладки
                json!({"id": id, "ok": true})
            }

            "stats" => match self.resolve_or_open_doc(v) {
                Ok(doc) => {
                    let s = self.docs.map[&doc].stats();
                    json!({
                        "id": id, "ok": true, "doc": doc,
                        "path": s.path,
                        "bytes": s.bytes,
                        "pieces": s.pieces,
                        "lines": s.lines,
                        "indexed_bytes": s.indexed_bytes,
                        "edited": s.edited,
                        "undo_depth": s.undo_depth,
                        "redo_depth": s.redo_depth,
                        "rev": s.rev,
                        "disk_changed": s.disk_changed,
                    })
                }
                Err(e) => json!({"id": id, "ok": false, "error": e}),
            },

            "viewport" => match self.resolve_or_open_doc(v) {
                Ok(doc) => {
                    let line = v.get("line").or_else(|| v.get("start_line")).and_then(|x| x.as_u64()).unwrap_or(0);
                    let count = v.get("count").or_else(|| v.get("line_count")).and_then(|x| x.as_u64()).unwrap_or(50).clamp(1, 4096) as usize;
                    let b = self.docs.map.get_mut(&doc).unwrap();
                    match b.viewport(line, count) {
                        Ok(vp) => json!({
                            "id": id, "ok": true, "doc": doc,
                            "rev": b.rev(),
                            "lines": vp.iter().map(|l| json!({
                                "line": l.line, "text": l.text, "truncated": l.truncated,
                            })).collect::<Vec<_>>(),
                        }),
                        Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                    }
                }
                Err(e) => json!({"id": id, "ok": false, "error": e}),
            },

            "read_span" => match self.resolve_or_open_doc(v) {
                Ok(doc) => {
                    let line = u64_of(v, "line");
                    let count = v
                        .get("count")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(200)
                        .clamp(1, 100_000) as usize;
                    let max_bytes = v
                        .get("max_bytes")
                        .and_then(|x| x.as_u64())
                        .map(|n| n as usize)
                        .unwrap_or(4 << 20)
                        .clamp(1, MAX_READ_SPAN_BYTES);
                    let b = self.docs.map.get_mut(&doc).unwrap();
                    match b.read_span(line, count, max_bytes) {
                        Ok(sp) => json!({
                            "id": id, "ok": true, "doc": doc,
                            "start_line": sp.start_line,
                            "end_line": sp.end_line,
                            "start_byte": sp.start_byte,
                            "end_byte": sp.end_byte,
                            "total_bytes": sp.total_bytes,
                            "byte_capped": sp.byte_capped,
                            "rev": sp.rev,
                            "text": sp.text,
                        }),
                        Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                    }
                }
                Err(e) => json!({"id": id, "ok": false, "error": e}),
            },

            "goto" => match self.resolve_or_open_doc(v) {
                Ok(doc) => {
                    let line = u64_of(v, "line");
                    let b = self.docs.map.get_mut(&doc).unwrap();
                    match b.offset_of_line(line) {
                        Ok(byte) => json!({"id": id, "ok": true, "byte": byte}),
                        Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                    }
                }
                Err(e) => json!({"id": id, "ok": false, "error": e}),
            },

            "linecol" => match self.resolve_or_open_doc(v) {
                Ok(doc) => {
                    let byte = u64_of(v, "byte");
                    let b = self.docs.map.get_mut(&doc).unwrap();
                    match b.line_col_of_byte_exact(byte) {
                        Ok((line, col)) => json!({"id": id, "ok": true, "line": line, "col": col}),
                        Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                    }
                }
                Err(e) => json!({"id": id, "ok": false, "error": e}),
            },

            "insert_at" => {
                let Some(text) = v.get("text").and_then(|x| x.as_str()) else {
                    return Some(json!({"id": id, "ok": false, "error": "insert_at: text field is required"}));
                };
                if text.len() > MAX_EDIT_PAYLOAD_BYTES {
                    return Some(json!({
                        "id": id,
                        "ok": false,
                        "error": format!("payload_too_large: text length ({}) exceeds limit {} bytes", text.len(), MAX_EDIT_PAYLOAD_BYTES)
                    }));
                }
                match self.resolve_or_open_doc(v) {
                    Ok(doc) => {
                        let line = u64_of(v, "line");
                        let col = u64_of(v, "col");
                        let byte_opt = v.get("byte").or_else(|| v.get("insert_at")).and_then(|x| x.as_u64());
                        let b = self.docs.map.get_mut(&doc).unwrap();
                        if let Some(cas_err) = check_cas(id, v, b) {
                            return Some(cas_err);
                        }
                        let r = if let Some(off) = byte_opt {
                            b.insert(off, text)
                        } else {
                            b.byte_of_line_col_exact(line, col)
                                .and_then(|off| b.insert(off, text))
                        };
                        match r {
                            Ok(()) => {
                                if let Err(se) = maybe_auto_save(v, b) {
                                    return Some(json!({"id": id, "ok": false, "error": se}));
                                }
                                notify_gui_agent_edit(from_agent, gui_tx, doc, "insert_at", b, line);
                                if let Some(ref ep) = self.event_plane {
                                    ep.registry().set_dirty(b.path(), true);
                                    ep.broadcast(crate::event_plane::Event::EditProposed {
                                        buffer_id: doc,
                                        path: b.path().to_string_lossy().to_string(),
                                        at: line,
                                        mode: "insert".to_string(),
                                        old_len: 0,
                                        new_text: text.to_string(),
                                    });
                                }
                                json!({
                                    "id": id, "ok": true, "doc": doc,
                                    "bytes": b.total_len(),
                                    "lines": b.lines_if_known(),
                                    "rev": b.rev(),
                                })
                            }
                            Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                        }
                    }
                    Err(e) => json!({"id": id, "ok": false, "error": e}),
                }
            }

            // Прямой потоковый Markdown-пайплайн для ИИ-агентов:
            // дописывает чанк токенов/текста в конец Piece-Table буфера с CAS-защитой
            "append" | "stream" => {
                let Some(text) = v.get("text").and_then(|x| x.as_str()) else {
                    return Some(json!({"id": id, "ok": false, "error": "append: text field is required"}));
                };
                match self.resolve_or_open_doc(v) {
                    Ok(doc) => {
                        let b = self.docs.map.get_mut(&doc).unwrap();
                        if let Some(cas_err) = check_cas(id, v, b) {
                            return Some(cas_err);
                        }
                        let off = b.total_len();
                        match b.insert(off, text) {
                            Ok(()) => {
                                if let Err(se) = maybe_auto_save(v, b) {
                                    return Some(json!({"id": id, "ok": false, "error": se}));
                                }
                                let lines = b.lines_if_known();
                                notify_gui_agent_edit(from_agent, gui_tx, doc, "stream", b, lines.unwrap_or(0).saturating_sub(1));
                                json!({
                                    "id": id,
                                    "ok": true,
                                    "doc": doc,
                                    "bytes": b.total_len(),
                                    "lines": lines,
                                    "rev": b.rev(),
                                })
                            }
                            Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                        }
                    }
                    Err(e) => json!({"id": id, "ok": false, "error": e}),
                }
            }

            "delete" => match self.resolve_or_open_doc(v) {
                Ok(doc) => {
                    let sl = u64_of(v, "start_line");
                    let sc = u64_of(v, "start_col");
                    let el = u64_of(v, "end_line");
                    let ec = u64_of(v, "end_col");
                    let b = self.docs.map.get_mut(&doc).unwrap();
                    if let Some(cas_err) = check_cas(id, v, b) {
                        return Some(cas_err);
                    }
                    let r = b.byte_of_line_col_exact(sl, sc).and_then(|s| {
                        b.byte_of_line_col_exact(el, ec).and_then(|e| b.delete(s, e))
                    });
                    match r {
                        Ok(()) => {
                            if let Err(se) = maybe_auto_save(v, b) {
                                return Some(json!({"id": id, "ok": false, "error": se}));
                            }
                            notify_gui_agent_edit(from_agent, gui_tx, doc, "delete", b, sl);
                            json!({
                                "id": id, "ok": true, "doc": doc,
                                "bytes": b.total_len(),
                                "lines": b.lines_if_known(),
                                "rev": b.rev(),
                            })
                        }
                        Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                    }
                }
                Err(e) => json!({"id": id, "ok": false, "error": e}),
            },

            "replace_match" => {
                let Some(old_text) = v.get("old_text").and_then(|x| x.as_str()) else {
                    return Some(json!({"id": id, "ok": false, "error": "replace_match: old_text field is required"}));
                };
                let Some(new_text) = v.get("new_text").and_then(|x| x.as_str()) else {
                    return Some(json!({"id": id, "ok": false, "error": "replace_match: new_text field is required"}));
                };
                let near_line = v.get("near_line").and_then(|x| x.as_u64());
                let occurrence = v.get("occurrence").and_then(|x| x.as_u64()).map(|n| n as usize);
                let replace_all = v.get("replace_all").and_then(|x| x.as_bool()).unwrap_or(false);
                let check_syntax = v.get("check_syntax").and_then(|x| x.as_bool()).unwrap_or(false);

                match self.resolve_or_open_doc(v) {
                    Ok(doc) => {
                        let b = self.docs.map.get_mut(&doc).unwrap();
                        if let Some(cas_err) = check_cas(id, v, b) {
                            return Some(cas_err);
                        }
                        match b.replace_match(
                            old_text,
                            new_text,
                            near_line,
                            occurrence,
                            replace_all,
                            check_syntax,
                        ) {
                            Ok(rec) => {
                                if let Err(se) = maybe_auto_save(v, b) {
                                    return Some(json!({"id": id, "ok": false, "error": se}));
                                }
                                notify_gui_agent_edit(
                                    from_agent,
                                    gui_tx,
                                    doc,
                                    "replace_match",
                                    b,
                                    rec.focus_line,
                                );
                                json!({
                                    "id": id, "ok": true, "doc": doc,
                                    "rev": rec.rev,
                                    "bytes": rec.bytes,
                                    "lines": rec.lines,
                                    "replacements": rec.replacements,
                                    "focus_line": rec.focus_line,
                                    "syntax_balanced": rec.syntax_balanced,
                                    "net_trit_sum": rec.net_trit_sum,
                                })
                            }
                            Err(e) => json!({"id": id, "ok": false, "error": e, "rev": b.rev()}),
                        }
                    }
                    Err(e) => json!({"id": id, "ok": false, "error": e}),
                }
            }

            "apply_batch" => {
                let Some(ops_val) = v.get("ops") else {
                    return Some(json!({"id": id, "ok": false, "error": "apply_batch: ops field is required"}));
                };
                let ops: Vec<BatchEditOp> = match serde_json::from_value(ops_val.clone()) {
                    Ok(o) => o,
                    Err(e) => {
                        return Some(json!({"id": id, "ok": false, "error": format!("invalid_ops: {e}")}));
                    }
                };
                let check_syntax = v.get("check_syntax").and_then(|x| x.as_bool()).unwrap_or(false);

                match self.resolve_or_open_doc(v) {
                    Ok(doc) => {
                        let b = self.docs.map.get_mut(&doc).unwrap();
                        if let Some(cas_err) = check_cas(id, v, b) {
                            return Some(cas_err);
                        }
                        match b.apply_batch(&ops, check_syntax) {
                            Ok(rec) => {
                                if let Err(se) = maybe_auto_save(v, b) {
                                    return Some(json!({"id": id, "ok": false, "error": se}));
                                }
                                notify_gui_agent_edit(
                                    from_agent,
                                    gui_tx,
                                    doc,
                                    "apply_batch",
                                    b,
                                    rec.focus_line,
                                );
                                json!({
                                    "id": id, "ok": true, "doc": doc,
                                    "rev": rec.rev,
                                    "bytes": rec.bytes,
                                    "lines": rec.lines,
                                    "replacements": rec.replacements,
                                    "focus_line": rec.focus_line,
                                    "syntax_balanced": rec.syntax_balanced,
                                    "net_trit_sum": rec.net_trit_sum,
                                })
                            }
                            Err(e) => json!({"id": id, "ok": false, "error": e, "rev": b.rev()}),
                        }
                    }
                    Err(e) => json!({"id": id, "ok": false, "error": e}),
                }
            }

            "apply_diff" => {
                let Some(diff_text) = v.get("diff").and_then(|x| x.as_str()) else {
                    return Some(json!({"id": id, "ok": false, "error": "apply_diff: diff field is required"}));
                };
                let fuzz = v.get("fuzz").and_then(|x| x.as_u64()).unwrap_or(3) as usize;
                let check_syntax = v.get("check_syntax").and_then(|x| x.as_bool()).unwrap_or(false);

                match self.resolve_or_open_doc(v) {
                    Ok(doc) => {
                        let b = self.docs.map.get_mut(&doc).unwrap();
                        if let Some(cas_err) = check_cas(id, v, b) {
                            return Some(cas_err);
                        }
                        match b.apply_unified_diff(diff_text, fuzz, check_syntax) {
                            Ok(rec) => {
                                if let Err(se) = maybe_auto_save(v, b) {
                                    return Some(json!({"id": id, "ok": false, "error": se}));
                                }
                                notify_gui_agent_edit(
                                    from_agent,
                                    gui_tx,
                                    doc,
                                    "apply_diff",
                                    b,
                                    rec.focus_line,
                                );
                                json!({
                                    "id": id, "ok": true, "doc": doc,
                                    "rev": rec.rev,
                                    "bytes": rec.bytes,
                                    "lines": rec.lines,
                                    "hunks_applied": rec.replacements,
                                    "focus_line": rec.focus_line,
                                    "syntax_balanced": rec.syntax_balanced,
                                    "net_trit_sum": rec.net_trit_sum,
                                })
                            }
                            Err(e) => json!({"id": id, "ok": false, "error": e, "rev": b.rev()}),
                        }
                    }
                    Err(e) => json!({"id": id, "ok": false, "error": e}),
                }
            }

            "piece_diff" => match self.resolve_or_open_doc(v) {
                Ok(doc) => {
                    let ctx = v
                        .get("context_lines")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(3)
                        .clamp(0, 50) as usize;
                    let b = self.docs.map.get_mut(&doc).unwrap();
                    match b.piece_diff(ctx) {
                        Ok(diff) => json!({
                            "id": id, "ok": true, "doc": doc,
                            "rev": b.rev(),
                            "edited": b.is_dirty(),
                            "diff": diff,
                        }),
                        Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                    }
                }
                Err(e) => json!({"id": id, "ok": false, "error": e}),
            },

            "fork_doc" => match self.resolve_or_open_doc(v) {
                Ok(src_doc) => {
                    let forked = self.docs.map[&src_doc].fork();
                    let rev = forked.rev();
                    let bytes = forked.total_len();
                    let fork_id = self.docs.create(forked);
                    json!({
                        "id": id, "ok": true,
                        "base_doc": src_doc,
                        "fork_doc": fork_id,
                        "bytes": bytes,
                        "rev": rev,
                    })
                }
                Err(e) => json!({"id": id, "ok": false, "error": e}),
            },

            "merge_fork" => {
                let base_id = u64_of(v, "base_doc");
                let ours_id = u64_of(v, "ours_doc");
                let theirs_id = u64_of(v, "theirs_doc");
                let target_id = v.get("target_doc").and_then(|x| x.as_u64());
                let insert_markers = v
                    .get("insert_markers")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);

                let (Some(base), Some(ours), Some(theirs)) = (
                    self.docs.map.get(&base_id),
                    self.docs.map.get(&ours_id),
                    self.docs.map.get(&theirs_id),
                ) else {
                    return Some(json!({
                        "id": id, "ok": false,
                        "error": "doc_not_found: specify existing base_doc, ours_doc, theirs_doc"
                    }));
                };

                if let Some(tid) = target_id {
                    let Some(tb) = self.docs.map.get(&tid) else {
                        return Some(json!({"id": id, "ok": false, "error": "target_doc not found"}));
                    };
                    if let Some(cas_err) = check_cas(id, v, tb) {
                        return Some(cas_err);
                    }
                }

                match merge_three_way(base, ours, theirs, insert_markers) {
                    Ok(res) => {
                        let clean = res.clean;
                        let conflicts = res.conflicts;
                        let applied_ours = res.applied_ours;
                        let applied_theirs = res.applied_theirs;
                        let out_doc = if let Some(tid) = target_id {
                            if clean || insert_markers {
                                self.docs.map.insert(tid, res.buffer);
                                let b = &self.docs.map[&tid];
                                notify_gui_agent_edit(from_agent, gui_tx, tid, "merge_fork", b, 0);
                            }
                            tid
                        } else {
                            self.docs.create(res.buffer)
                        };
                        let b = &self.docs.map[&out_doc];
                        json!({
                            "id": id, "ok": true,
                            "merged_doc": out_doc,
                            "clean": clean,
                            "applied_ours": applied_ours,
                            "applied_theirs": applied_theirs,
                            "conflicts": conflicts,
                            "rev": b.rev(),
                            "bytes": b.total_len(),
                        })
                    }
                    Err(e) => json!({"id": id, "ok": false, "error": e}),
                }
            }

            "verify_syntax" => match self.resolve_or_open_doc(v) {
                Ok(doc) => {
                    let b = &self.docs.map[&doc];
                    let is_md = b
                        .path()
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| matches!(e.to_ascii_lowercase().as_str(), "md" | "markdown" | "txt"))
                        .unwrap_or(false);
                    let verdict = verify_snapshot_syntax(&b.snapshot(), is_md);
                    json!({
                        "id": id, "ok": true, "doc": doc, "rev": b.rev(),
                        "balanced": verdict.balanced,
                        "net_trit_sum": verdict.net_trit_sum,
                        "min_dip": verdict.min_dip,
                        "max_depth": verdict.max_depth,
                        "packed_bytes": verdict.packed_bytes,
                        "vacuum_ratio": verdict.vacuum_ratio,
                        "issues": verdict.issues,
                    })
                }
                Err(e) => json!({"id": id, "ok": false, "error": e}),
            },

            "outline" => match self.resolve_or_open_doc(v) {
                Ok(doc) => {
                    let b = &self.docs.map[&doc];
                    let cap = (b.total_len() as usize).min(16 << 20);
                    let raw = b.read_range(0, cap);
                    let items = extract_outline_from_bytes(&raw, Some(b.path()));
                    json!({
                        "id": id, "ok": true, "doc": doc, "rev": b.rev(),
                        "items": items,
                    })
                }
                Err(e) => json!({"id": id, "ok": false, "error": e}),
            },

            "breadcrumbs" => match self.resolve_or_open_doc(v) {
                Ok(doc) => {
                    let line = u64_of(v, "line");
                    let b = &self.docs.map[&doc];
                    let cap = (b.total_len() as usize).min(16 << 20);
                    let raw = b.read_range(0, cap);
                    let items = extract_outline_from_bytes(&raw, Some(b.path()));
                    let chain = breadcrumbs_for_line(&items, line);
                    json!({
                        "id": id, "ok": true, "doc": doc,
                        "line": line, "breadcrumbs": chain,
                    })
                }
                Err(e) => json!({"id": id, "ok": false, "error": e}),
            },

            "project_impact" => {
                let Some(symbol) = v.get("symbol").and_then(|x| x.as_str()) else {
                    return Some(json!({"id": id, "ok": false, "error": "project_impact: symbol field is required"}));
                };
                let root = req_root(v);
                let refresh = v.get("refresh").and_then(|x| x.as_bool()).unwrap_or(false);
                let g = self.get_or_build_graph(&root, refresh);
                let rep = g.impact(symbol);
                json!({
                    "id": id, "ok": true,
                    "report": rep,
                })
            }

            "semantic_search" => {
                let query = v.get("query").and_then(|x| x.as_str()).unwrap_or("");
                let root = req_root(v);
                let limit = v
                    .get("limit")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(20)
                    .clamp(1, 200) as usize;
                let req_flags = u64_of(v, "require_flags");
                let forbid_flags = u64_of(v, "forbid_flags");
                let refresh = v.get("refresh").and_then(|x| x.as_bool()).unwrap_or(false);
                let g = self.get_or_build_graph(&root, refresh);
                let hits = g.semantic_search(query, limit, req_flags, forbid_flags);
                json!({
                    "id": id, "ok": true,
                    "graph_nodes": g.scopes.len(),
                    "hits": hits,
                })
            }

            "pdf_replace" => {
                let Some(p) = req_path(v) else {
                    return Some(json!({"id": id, "ok": false, "error": "pdf_replace: path field is required"}));
                };
                let search = v.get("search").and_then(|x| x.as_str()).unwrap_or("");
                let replace = v.get("replace").and_then(|x| x.as_str()).unwrap_or("");
                match crate::pdf_edit::pdf_replace_in_file(&p, search, replace) {
                    Ok(res) => json!({
                        "id": id, "ok": res.ok, "matches": res.matches,
                        "bytes_appended": res.bytes_appended, "path": res.path,
                        "error": res.error,
                    }),
                    Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                }
            }

            "pdf_save" => {
                let Some(p) = req_path(v) else {
                    return Some(json!({"id": id, "ok": false, "error": "pdf_save: path field is required"}));
                };
                let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("");
                match crate::pdf_edit::save_pdf_buffer(&p, text) {
                    Ok(n) => json!({"id": id, "ok": true, "path": p, "bytes": n}),
                    Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                }
            }

            "extract_doc" => {
                let Some(p) = req_path(v) else {
                    return Some(json!({"id": id, "ok": false, "error": "extract_doc: path field is required"}));
                };
                let max_chars = v
                    .get("max_chars")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(500_000) as usize;
                // Фаза 1: keep_images=true — читалка получает Markdown с
                // ссылками на иллюстрации (EPUB: пути внутри архива;
                // FB2: data:-URI из binary-элементов), loadResource в
                // ReaderView подтягивает их через vfs_read без распаковки.
                let keep_images = v
                    .get("keep_images")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);
                match crate::extractor::extract_text_opts(&p, keep_images) {
                    Ok(Some(text)) => {
                        let total_chars = text.chars().count();
                        let truncated = total_chars > max_chars;
                        let out_text: String = if truncated {
                            text.chars().take(max_chars).collect()
                        } else {
                            text
                        };
                        json!({
                            "id": id, "ok": true, "path": p,
                            "chars": total_chars,
                            "truncated": truncated,
                            "text": out_text,
                        })
                    }
                    Ok(None) => match std::fs::read_to_string(&p) {
                        Ok(raw) => {
                            let total_chars = raw.chars().count();
                            let truncated = total_chars > max_chars;
                            let out_text: String = if truncated {
                                raw.chars().take(max_chars).collect()
                            } else {
                                raw
                            };
                            json!({
                                "id": id, "ok": true, "path": p,
                                "chars": total_chars,
                                "truncated": truncated,
                                "text": out_text,
                            })
                        }
                        Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                    },
                    Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                }
            }

            "vfs_list" => {
                // Фаза 5: листинг RAM-полигона vfs://ai_scratch/ — записи
                // агента в памяти (ноль байт на диске) + статистика дедупа.
                if let Some(sp) = v.get("path").and_then(|x| x.as_str()) {
                    if crate::scratch::is_scratch_path(sp) {
                        let entries: Vec<serde_json::Value> = self
                            .scratch
                            .list()
                            .into_iter()
                            .map(|s| {
                                json!({
                                    "name": s.name,
                                    "bytes": s.bytes,
                                    "chunks": s.chunks,
                                    "shared_chunks": s.shared_chunks,
                                    "provenance": s.provenance,
                                    "updated_ms": s.updated_ms,
                                })
                            })
                            .collect();
                        let st = self.scratch.stats();
                        return Some(json!({
                            "id": id, "ok": true, "path": sp,
                            "kind": "ai_scratch",
                            "entries": entries,
                            "stats": {
                                "entries": st.entries,
                                "logical_bytes": st.logical_bytes,
                                "physical_bytes": st.physical_bytes,
                                "unique_chunks": st.unique_chunks,
                                "dedup_ratio": st.dedup_ratio(),
                            },
                        }));
                    }
                }
                let Some(p) = req_path(v) else {
                    return Some(json!({"id": id, "ok": false, "error": "vfs_list: path field is required"}));
                };
                // Фаза 3: листинг с учётом CoW-оверлея (статусы
                // ok/modified/added, удалённые скрыты, новые дописаны).
                let key = normalize_path_key(&p);
                match crate::vfs::vfs_list(&p) {
                    Ok((kind, entries)) => {
                        let entries = match self.vfs_overlays.get(&key) {
                            Some(ov) => crate::vfs_edit::merge_listing(entries, ov),
                            None => entries,
                        };
                        json!({
                            "id": id, "ok": true, "path": p,
                            "kind": kind.as_str(),
                            "entries": entries,
                        })
                    }
                    Err(e) => json!({"id": id, "ok": false, "error": e}),
                }
            }

            "vfs_read" => {
                // Фаза 5: поддерево полигона vfs://ai_scratch/ читается из
                // RAM-стора (имя — хвост пути после схемы либо поле entry).
                if let Some(sp) = v.get("path").and_then(|x| x.as_str()) {
                    if crate::scratch::is_scratch_path(sp) {
                        let name = sp
                            .strip_prefix(crate::scratch::SCRATCH_SCHEME)
                            .map(|s| s.to_string())
                            .filter(|s| !s.is_empty())
                            .or_else(|| {
                                v.get("entry").and_then(|x| x.as_str()).map(|s| s.to_string())
                            })
                            .unwrap_or_default();
                        if name.is_empty() {
                            return Some(json!({"id": id, "ok": false,
                                "error": "vfs_read: for scratchpad vfs://ai_scratch/ specify entry name in path or entry"}));
                        }
                        return Some(match self.scratch.get(&name) {
                            Ok(bytes)
                                if v.get("text").and_then(|x| x.as_bool()).unwrap_or(false) =>
                            {
                                json!({
                                    "id": id, "ok": true, "path": sp,
                                    "bytes": bytes.len() as u64,
                                    "text": String::from_utf8_lossy(&bytes),
                                })
                            }
                            Ok(bytes) => json!({
                                "id": id, "ok": true, "path": sp,
                                "bytes": bytes.len() as u64,
                                "data_base64": crate::vfs::base64_encode(&bytes),
                            }),
                            Err(e) => json!({"id": id, "ok": false, "error": e}),
                        });
                    }
                }
                let Some(p) = req_path(v) else {
                    return Some(json!({"id": id, "ok": false, "error": "vfs_read: path field is required"}));
                };
                let Some(entry) = v.get("entry").and_then(|x| x.as_str()) else {
                    return Some(json!({"id": id, "ok": false, "error": "vfs_read: entry field is required"}));
                };
                let as_text = v.get("text").and_then(|x| x.as_bool()).unwrap_or(false);
                // Лимит записи (защита от zip-бомб): по умолчанию 64 МиБ,
                // как у архивного слоя poler-engine::archive::ReadLimits.
                let max_entry_bytes = v
                    .get("max_bytes")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(crate::vfs::DEFAULT_MAX_ENTRY_BYTES)
                    .clamp(1, crate::vfs::DEFAULT_MAX_ENTRY_BYTES);
                // Фаза 3: изменённые записи читаются из CoW-оверлея,
                // удалённые — ошибка, нетронутые — с диска (Фаза 1).
                let key = normalize_path_key(&p);
                match crate::vfs_edit::overlay_read(
                    &p,
                    entry,
                    max_entry_bytes,
                    self.vfs_overlays.get(&key),
                ) {
                    Ok(bytes) => {
                        if as_text {
                            json!({
                                "id": id, "ok": true, "path": p, "entry": entry,
                                "bytes": bytes.len() as u64,
                                "text": String::from_utf8_lossy(&bytes),
                            })
                        } else {
                            json!({
                                "id": id, "ok": true, "path": p, "entry": entry,
                                "bytes": bytes.len() as u64,
                                "data_base64": crate::vfs::base64_encode(&bytes),
                            })
                        }
                    }
                    Err(e) => json!({"id": id, "ok": false, "error": e}),
                }
            }

            // ------------- Фаза 3 «Редактирование в архиве» (vfs_write + CoW) -------------

            "vfs_open" => {
                // Открыть запись архива как редактируемый документ: буфер
                // получает виртуальный путь vfs://<архив>::<запись>, правки
                // живут в piece-table, а Ctrl+S (vfs_save) уходит в оверлей.
                let Some(p) = req_path(v) else {
                    return Some(json!({"id": id, "ok": false, "error": "vfs_open: path field is required"}));
                };
                let Some(entry) = v.get("entry").and_then(|x| x.as_str()) else {
                    return Some(json!({"id": id, "ok": false, "error": "vfs_open: entry field is required"}));
                };
                let norm = crate::vfs::norm_name(entry);
                // Повторное открытие уже открытой записи — тот же doc
                // (как reuse у "open": вкладка GUI подключается к буферу).
                if let Some((&doc, _)) = self.vfs_docs.iter().find(|(_, (a, e))| {
                    normalize_path_key(a) == normalize_path_key(&p) && e == &norm
                }) {
                    let b = &self.docs.map[&doc];
                    json!({
                        "id": id, "ok": true, "doc": doc, "reused": true,
                        "path": b.path(), "bytes": b.total_len(),
                        "pieces": b.pieces_len(), "lines": b.lines_if_known(),
                        "rev": b.rev(), "edited": b.is_dirty(),
                        "vfs": true, "archive": p, "entry": norm,
                    })
                } else {
                    let key = normalize_path_key(&p);
                    match crate::vfs_edit::overlay_read(
                        &p,
                        &norm,
                        crate::vfs::DEFAULT_MAX_ENTRY_BYTES,
                        self.vfs_overlays.get(&key),
                    ) {
                        Ok(bytes) => match String::from_utf8(bytes) {
                            Ok(text) => {
                                let virtual_path =
                                    PathBuf::from(format!("vfs://{}::{}", p.display(), norm));
                                let buf = PolerBuffer::from_text(virtual_path, &text);
                                let doc = self.docs.create(buf);
                                self.vfs_docs.insert(doc, (p.clone(), norm.clone()));
                                let b = &self.docs.map[&doc];
                                json!({
                                    "id": id, "ok": true, "doc": doc,
                                    "path": b.path(),
                                    "bytes": b.total_len(),
                                    "pieces": b.pieces_len(),
                                    "lines": b.lines_if_known(),
                                    "rev": b.rev(),
                                    "edited": b.is_dirty(),
                                    "vfs": true, "archive": p, "entry": norm,
                                })
                            }
                            Err(_) => json!({"id": id, "ok": false, "error":
                                format!("vfs_open: entry '{norm}' contains binary data (non UTF-8), editor accepts text only")}),
                        },
                        Err(e) => json!({"id": id, "ok": false, "error": e}),
                    }
                }
            }

            "vfs_save" => {
                // Ctrl+S VFS-вкладки: содержимое буфера → CoW-оверлей,
                // диск не трогается (copy-on-write). Материализация —
                // отдельная команда vfs_flush.
                let doc = doc_of(v);
                let Some((archive, entry)) = self.vfs_docs.get(&doc).cloned() else {
                    return Some(json!({"id": id, "ok": false, "error":
                        format!("vfs_save: doc={doc} is not a VFS tab (open entry via vfs_open)")}));
                };
                let Some(b) = self.docs.map.get_mut(&doc) else {
                    return Some(json!({"id": id, "ok": false, "error": "vfs_save: no such doc"}));
                };
                let len = b.total_len();
                if len > crate::vfs_edit::DEFAULT_MAX_WRITE_BYTES {
                    return Some(json!({"id": id, "ok": false, "error":
                        format!("vfs_save: document {len} bytes > limit {} bytes", crate::vfs_edit::DEFAULT_MAX_WRITE_BYTES)}));
                }
                let bytes = b.read_range(0, len as usize);
                let key = normalize_path_key(&archive);
                let ov = self.vfs_overlays.entry(key).or_default();
                ov.write(&entry, bytes);
                let dirty = ov.raw_changes().count();
                b.mark_clean();
                notify_gui_agent_edit(from_agent, gui_tx, doc, "vfs_save", b, 0);
                json!({
                    "id": id, "ok": true, "doc": doc,
                    "archive": archive, "entry": entry,
                    "bytes": len, "dirty_entries": dirty, "cow": true,
                })
            }

            "vfs_write" => {
                // Прямая запись в оверлей (GUI/агенты): data_base64 или text.
                let Some(p) = req_path(v) else {
                    return Some(json!({"id": id, "ok": false, "error": "vfs_write: path field is required"}));
                };
                let Some(entry) = v.get("entry").and_then(|x| x.as_str()) else {
                    return Some(json!({"id": id, "ok": false, "error": "vfs_write: entry field is required"}));
                };
                let bytes: Vec<u8> = if let Some(b64) = v.get("data_base64").and_then(|x| x.as_str()) {
                    match crate::vfs::base64_decode(b64) {
                        Ok(b) => b,
                        Err(e) => {
                            return Some(json!({"id": id, "ok": false,
                                "error": format!("vfs_write: base64: {e}")}));
                        }
                    }
                } else if let Some(text) = v.get("text").and_then(|x| x.as_str()) {
                    text.as_bytes().to_vec()
                } else {
                    return Some(json!({"id": id, "ok": false,
                        "error": "vfs_write: data_base64 or text field required"}));
                };
                let n = bytes.len();
                if n as u64 > crate::vfs_edit::DEFAULT_MAX_WRITE_BYTES {
                    return Some(json!({"id": id, "ok": false, "error":
                        format!("vfs_write: {n} bytes > limit {} bytes", crate::vfs_edit::DEFAULT_MAX_WRITE_BYTES)}));
                }
                let key = normalize_path_key(&p);
                let ov = self.vfs_overlays.entry(key).or_default();
                ov.write(entry, bytes);
                let dirty = ov.raw_changes().count();
                json!({
                    "id": id, "ok": true, "path": p,
                    "entry": crate::vfs::norm_name(entry),
                    "bytes": n, "dirty_entries": dirty, "cow": true,
                })
            }

            "vfs_delete" => {
                // Пометить запись удалённой в оверлее (материализация — flush).
                let Some(p) = req_path(v) else {
                    return Some(json!({"id": id, "ok": false, "error": "vfs_delete: path field is required"}));
                };
                let Some(entry) = v.get("entry").and_then(|x| x.as_str()) else {
                    return Some(json!({"id": id, "ok": false, "error": "vfs_delete: entry field is required"}));
                };
                let key = normalize_path_key(&p);
                let ov = self.vfs_overlays.entry(key).or_default();
                let had = ov.delete(entry);
                let dirty = ov.raw_changes().count();
                json!({
                    "id": id, "ok": true, "path": p,
                    "entry": crate::vfs::norm_name(entry),
                    "was_in_overlay": had, "dirty_entries": dirty, "cow": true,
                })
            }

            "vfs_dirty" => {
                // Что изменено относительно диска: modified/added/deleted.
                let Some(p) = req_path(v) else {
                    return Some(json!({"id": id, "ok": false, "error": "vfs_dirty: path field is required"}));
                };
                let key = normalize_path_key(&p);
                let entries = match self.vfs_overlays.get(&key) {
                    Some(ov) if !ov.is_empty() => {
                        let base_names: std::collections::HashSet<String> =
                            crate::vfs::vfs_list(&p)
                                .map(|(_, es)| es.into_iter().map(|e| e.name).collect())
                                .unwrap_or_default();
                        crate::vfs_edit::overlay_dirty(ov, &base_names)
                    }
                    _ => Vec::new(),
                };
                let count = entries.len() as u64;
                json!({
                    "id": id, "ok": true, "path": p,
                    "count": count, "entries": entries,
                })
            }

            "vfs_flush" => {
                // Материализация CoW: пересборка архива атомарно
                // (tmp + fsync + rename). «to» — сохранить копию, оригинал
                // остаётся нетронутым, оверлей уходит в копию.
                //
                // Фаза 4 (Shannon Bypass v3.0): `method` — метод сжатия чанков
                // `.poler` ("zstd" по умолчанию | "symbol" — символьная фаза);
                // `format: "poler"` — принудительная упаковка результата в
                // родной `.poler` (ремукс любого смонтированного контейнера
                // с дотисканием; пустой CoW-оверлей в этом режиме допустим —
                // это конверсия/перенос записей, а не материализация правок).
                let Some(p) = req_path(v) else {
                    return Some(json!({"id": id, "ok": false, "error": "vfs_flush: path field is required"}));
                };
                let to = v
                    .get("to")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from);
                let poler_method = match v.get("method").and_then(|x| x.as_str()) {
                    None => crate::vfs_edit::PolerCompress::Zstd,
                    Some("zstd") => crate::vfs_edit::PolerCompress::Zstd,
                    Some("symbol") => crate::vfs_edit::PolerCompress::Symbol,
                    Some(other) => {
                        return Some(json!({"id": id, "ok": false,
                            "error": format!("vfs_flush: method must be \"zstd\" or \"symbol\", got {other:?}")}))
                    }
                };
                let force_kind = match v.get("format").and_then(|x| x.as_str()) {
                    None => None,
                    Some("poler") => Some(crate::vfs::VfsKind::Poler),
                    Some(other) => {
                        return Some(json!({"id": id, "ok": false,
                            "error": format!("vfs_flush: unknown format {other:?} (only \"poler\" is supported)")}))
                    }
                };
                let key = normalize_path_key(&p);
                let empty = self.vfs_overlays.get(&key).map(|ov| ov.is_empty()).unwrap_or(true);
                if empty && force_kind.is_none() {
                    return Some(json!({"id": id, "ok": false,
                        "error": "vfs_flush: archive has no changes (CoW overlay is empty)"}));
                }
                // Пустой локальный оверлей для режима ремукса (формат принуждён,
                // правок может не быть — переносим всё как есть).
                let empty_ov = crate::vfs_edit::VfsOverlay::new();
                let ov = self.vfs_overlays.get(&key).unwrap_or(&empty_ov);
                let dest = to.unwrap_or_else(|| p.clone());
                let opts = crate::vfs_edit::FlushOptions {
                    poler_method,
                    force_kind,
                };
                match crate::vfs_edit::flush_archive_opts(
                    &p,
                    ov,
                    &dest,
                    crate::vfs::DEFAULT_MAX_ENTRY_BYTES,
                    &opts,
                ) {
                    Ok(stats) => {
                        // Оверлей израсходован (при save-as — тоже: правки
                        // теперь живут в копии, оригинал чист).
                        self.vfs_overlays.remove(&key);
                        let ratio = if stats.raw > 0 {
                            stats.bytes as f64 / stats.raw as f64
                        } else {
                            1.0
                        };
                        json!({
                            "id": id, "ok": true,
                            "source": p, "path": dest,
                            "kind": stats.kind, "entries": stats.entries,
                            "bytes": stats.bytes, "raw": stats.raw,
                            "method": stats.method,
                            "ratio": (ratio * 10_000.0).round() / 10_000.0,
                        })
                    }
                    Err(e) => json!({"id": id, "ok": false, "error": e}),
                }
            }

            "symbol_bench" => {
                // Фаза 4 (Shannon Bypass v3.0): бенчмарк символьной фазы —
                // коэффициент сжатия против zstd-3 + проверка бит-в-бит
                // round-trip. Данные: data_base64 | text | path (файл ≤ 64 МиБ).
                let data: Vec<u8> = if let Some(b64) = v.get("data_base64").and_then(|x| x.as_str()) {
                    match crate::vfs::base64_decode(b64) {
                        Ok(b) => b,
                        Err(e) => {
                            return Some(json!({"id": id, "ok": false,
                                "error": format!("symbol_bench: base64: {e}")}))
                        }
                    }
                } else if let Some(text) = v.get("text").and_then(|x| x.as_str()) {
                    text.as_bytes().to_vec()
                } else if let Some(p) = req_path(v) {
                    match std::fs::metadata(&p) {
                        Ok(m) if m.len() <= crate::vfs::DEFAULT_MAX_ENTRY_BYTES => {}
                        Ok(m) => {
                            return Some(json!({"id": id, "ok": false,
                                "error": format!("symbol_bench: {} B > limit {} B", m.len(), crate::vfs::DEFAULT_MAX_ENTRY_BYTES)}))
                        }
                        Err(e) => {
                            return Some(json!({"id": id, "ok": false,
                                "error": format!("symbol_bench: {}: {e}", p.display())}))
                        }
                    }
                    match std::fs::read(&p) {
                        Ok(b) => b,
                        Err(e) => {
                            return Some(json!({"id": id, "ok": false,
                                "error": format!("symbol_bench: {}: {e}", p.display())}))
                        }
                    }
                } else {
                    return Some(json!({"id": id, "ok": false,
                        "error": "symbol_bench: data_base64, text, or path field is required"}));
                };
                json!({
                    "id": id, "ok": false,
                    "error": "Shannon Bypass symbolic phase is offloaded to sovereign poler-engine",
                })
            }
            "vfs_discard" => {
                // Откатить CoW-правки архива к состоянию на диске.
                let Some(p) = req_path(v) else {
                    return Some(json!({"id": id, "ok": false, "error": "vfs_discard: path field is required"}));
                };
                let key = normalize_path_key(&p);
                let discarded = self.vfs_overlays.remove(&key).is_some();
                json!({"id": id, "ok": true, "path": p, "discarded": discarded})
            }


            // ------------- Фаза 5 «MCP-глаза на VFS»: RAM-полигон + Кристалл -------------

            "scratch_put" => {
                // Сохранить заметку/черновик/граф гипотезы в RAM-полигон
                // vfs://ai_scratch/ — ноль байт на диске. provenance_path
                // (+ provenance_entry для записи архива) регистрирует
                // базу-источник: черновик, дублирующий источник, разделяет
                // с ним чанки FastCDC+BLAKE3 и не занимает лишнюю RAM.
                let Some(name) = v
                    .get("uri")
                    .or_else(|| v.get("name"))
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| {
                        v.get("path")
                            .and_then(|x| x.as_str())
                            .filter(|s| crate::scratch::is_scratch_path(s))
                            .map(|s| s.to_string())
                    })
                else {
                    return Some(json!({"id": id, "ok": false,
                        "error": "scratch_put: uri (or name) field is required"}));
                };
                let data: Vec<u8> = if let Some(b64) = v.get("data_base64").and_then(|x| x.as_str()) {
                    match crate::vfs::base64_decode(b64) {
                        Ok(b) => b,
                        Err(e) => {
                            return Some(json!({"id": id, "ok": false,
                                "error": format!("scratch_put: base64: {e}")}))
                        }
                    }
                } else if let Some(text) = v.get("text").and_then(|x| x.as_str()) {
                    text.as_bytes().to_vec()
                } else {
                    return Some(json!({"id": id, "ok": false,
                        "error": "scratch_put: data_base64 or text field required"}));
                };
                let provenance = v.get("provenance").and_then(|x| x.as_str()).map(|s| s.to_string());
                // База-источник: файл на диске либо запись смонтированного
                // архива (с учётом CoW-оверлея) — её чанки становятся общими.
                let mut base_registered = 0u64;
                if let Some(bp) = v.get("provenance_path").and_then(|x| x.as_str()) {
                    let src: Vec<u8> =
                        if let Some(entry) = v.get("provenance_entry").and_then(|x| x.as_str()) {
                            let bpp = PathBuf::from(bp);
                            let key = normalize_path_key(&bpp);
                            match crate::vfs_edit::overlay_read(
                                &bpp,
                                entry,
                                crate::vfs::DEFAULT_MAX_ENTRY_BYTES,
                                self.vfs_overlays.get(&key),
                            ) {
                                Ok(b) => b,
                                Err(e) => {
                                    return Some(json!({"id": id, "ok": false,
                                        "error": format!("scratch_put: provenance {bp}/{entry}: {e}")}))
                                }
                            }
                        } else {
                            match std::fs::read(bp) {
                                Ok(b) => b,
                                Err(e) => {
                                    return Some(json!({"id": id, "ok": false,
                                        "error": format!("scratch_put: provenance {bp}: {e}")}))
                                }
                            }
                        };
                    match self.scratch.register_base(&src) {
                        Ok(n) => base_registered = n,
                        Err(e) => {
                            return Some(json!({"id": id, "ok": false,
                                "error": format!("scratch_put: provenance: {e}")}))
                        }
                    }
                }
                let canon = match crate::scratch::normalize_name(&name) {
                    Ok(n) => n,
                    Err(e) => {
                        return Some(json!({"id": id, "ok": false,
                            "error": format!("scratch_put: {e}")}))
                    }
                };
                match self.scratch.put(&name, &data, provenance.as_deref()) {
                    Ok(st) => json!({
                        "id": id, "ok": true,
                        "uri": crate::scratch::scratch_uri(&canon),
                        "bytes": st.bytes, "chunks": st.chunks,
                        "new_chunks": st.new_chunks, "shared_chunks": st.shared_chunks,
                        "physical_delta": st.physical_delta,
                        "base_registered": base_registered,
                        "ram": true, "disk": 0,
                    }),
                    Err(e) => json!({"id": id, "ok": false, "error": e}),
                }
            }

            "scratch_get" => {
                // Пересобрать запись полигона из чанков (бит-в-бит).
                let Some(name) = v
                    .get("uri")
                    .or_else(|| v.get("name"))
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| {
                        v.get("path")
                            .and_then(|x| x.as_str())
                            .filter(|s| crate::scratch::is_scratch_path(s))
                            .map(|s| s.to_string())
                    })
                else {
                    return Some(json!({"id": id, "ok": false,
                        "error": "scratch_get: uri (or name) field is required"}));
                };
                let as_text = v.get("text").and_then(|x| x.as_bool()).unwrap_or(false);
                match self.scratch.get(&name) {
                    Ok(bytes) => {
                        let meta = self.scratch.list().into_iter().find(|s| {
                            crate::scratch::normalize_name(&name)
                                .map(|n| n == s.name)
                                .unwrap_or(false)
                        });
                        let base = json!({
                            "id": id, "ok": true,
                            "uri": crate::scratch::scratch_uri(
                                &crate::scratch::normalize_name(&name).unwrap_or_default()),
                            "bytes": bytes.len() as u64,
                            "provenance": meta.as_ref().and_then(|m| m.provenance.clone()),
                            "updated_ms": meta.as_ref().map(|m| m.updated_ms).unwrap_or(0),
                        });
                        if as_text {
                            let mut r = base;
                            r["text"] = json!(String::from_utf8_lossy(&bytes));
                            r
                        } else {
                            let mut r = base;
                            r["data_base64"] = json!(crate::vfs::base64_encode(&bytes));
                            r
                        }
                    }
                    Err(e) => json!({"id": id, "ok": false, "error": e}),
                }
            }

            "scratch_list" => {
                // Список записей полигона (+ фильтр-подстрока) и статистика.
                let pattern = v.get("pattern").and_then(|x| x.as_str()).unwrap_or("");
                let entries: Vec<serde_json::Value> = self
                    .scratch
                    .list()
                    .into_iter()
                    .filter(|s| pattern.is_empty() || s.name.contains(pattern))
                    .map(|s| {
                        json!({
                            "uri": s.uri,
                            "name": s.name,
                            "bytes": s.bytes,
                            "chunks": s.chunks,
                            "shared_chunks": s.shared_chunks,
                            "provenance": s.provenance,
                            "created_ms": s.created_ms,
                            "updated_ms": s.updated_ms,
                        })
                    })
                    .collect();
                let count = entries.len() as u64;
                let st = self.scratch.stats();
                json!({
                    "id": id, "ok": true, "count": count, "entries": entries,
                    "stats": {
                        "entries": st.entries,
                        "logical_bytes": st.logical_bytes,
                        "physical_bytes": st.physical_bytes,
                        "unique_chunks": st.unique_chunks,
                        "total_refs": st.total_refs,
                        "dedup_ratio": st.dedup_ratio(),
                        "disk": 0,
                    },
                })
            }

            "scratch_discard" => {
                // Микросекундный откат идеи: одна запись или весь полигон.
                if v.get("all").and_then(|x| x.as_bool()).unwrap_or(false) {
                    let n = self.scratch.discard_all();
                    json!({"id": id, "ok": true, "all": true, "discarded": n as u64})
                } else {
                    let Some(name) = v
                        .get("uri")
                        .or_else(|| v.get("name"))
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string())
                        .or_else(|| {
                            v.get("path")
                                .and_then(|x| x.as_str())
                                .filter(|s| crate::scratch::is_scratch_path(s))
                                .map(|s| s.to_string())
                        })
                    else {
                        return Some(json!({"id": id, "ok": false,
                            "error": "scratch_discard: uri (or name) field, or all: true is required"}));
                    };
                    match self.scratch.discard(&name) {
                        Ok(true) => json!({"id": id, "ok": true, "uri": name, "discarded": true}),
                        Ok(false) => json!({"id": id, "ok": false,
                            "error": format!("scratch_discard: entry {name:?} not found in scratchpad")}),
                        Err(e) => json!({"id": id, "ok": false, "error": e}),
                    }
                }
            }

            "scratch_apply" => {
                // Атомарная материализация выверенной идеи из полигона:
                //  * target + entry → CoW-оверлей смонтированного архива
                //    (материализация на диск — существующий vfs_flush);
                //  * target (файл) → atomic write (tmp + fsync + rename).
                let Some(name) = v
                    .get("uri")
                    .or_else(|| v.get("name"))
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| {
                        v.get("path")
                            .and_then(|x| x.as_str())
                            .filter(|s| crate::scratch::is_scratch_path(s))
                            .map(|s| s.to_string())
                    })
                else {
                    return Some(json!({"id": id, "ok": false,
                        "error": "scratch_apply: uri (or name) field is required"}));
                };
                let Some(target) = v
                    .get("target")
                    .or_else(|| v.get("to"))
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                else {
                    return Some(json!({"id": id, "ok": false,
                        "error": "scratch_apply: target field (file or archive path) is required"}));
                };
                let data = match self.scratch.get(&name) {
                    Ok(d) => d,
                    Err(e) => {
                        return Some(json!({"id": id, "ok": false, "error": e}))
                    }
                };
                let n = data.len() as u64;
                if let Some(entry) = v.get("entry").and_then(|x| x.as_str()).filter(|s| !s.is_empty()) {
                    let p = PathBuf::from(target);
                    let key = normalize_path_key(&p);
                    let ov = self.vfs_overlays.entry(key).or_default();
                    ov.write(entry, data);
                    let dirty = ov.raw_changes().count();
                    json!({
                        "id": id, "ok": true, "uri": name,
                        "target": p, "entry": crate::vfs::norm_name(entry),
                        "bytes": n, "cow": true, "dirty_entries": dirty,
                        "materialized": false,
                        "note": "запись в CoW-оверлее; материализация — vfs_flush {path}",
                    })
                } else {
                    let p = PathBuf::from(target);
                    match crate::scratch::atomic_write(&p, &data) {
                        Ok(()) => json!({
                            "id": id, "ok": true, "uri": name,
                            "target": p, "bytes": n,
                            "materialized": true, "atomic": true,
                        }),
                        Err(e) => json!({"id": id, "ok": false, "error": e}),
                    }
                }
            }

            "crystal_note" => {
                // Прошить заметку в Кристалл Знаний: новые слова получают
                // id за O(1) (Dynamic Vocab Expansion), пары соседей —
                // Хеббовское укрепление синапса (+1, насыщение {−1,0,+1}).
                let Some(text) = v.get("text").and_then(|x| x.as_str()) else {
                    return Some(json!({"id": id, "ok": false,
                        "error": "crystal_note: text field is required"}));
                };
                let words = crate::crystal::tokenize(text);
                if words.is_empty() {
                    return Some(json!({"id": id, "ok": false,
                        "error": "crystal_note: empty note (no words found)"}));
                }
                let crystal = match self.crystal_mut() {
                    Ok(c) => c,
                    Err(e) => {
                        return Some(json!({"id": id, "ok": false, "error": e}))
                    }
                };
                let mut new_words = 0u64;
                let mut ids: Vec<u32> = Vec::with_capacity(words.len());
                for w in &words {
                    if let Some(id) = crystal.id_of(w) {
                        ids.push(id);
                    } else if let Some(id) = crystal.expand_vocab(w) {
                        new_words += 1;
                        ids.push(id);
                    }
                }
                let mut bumped = 0u64;
                for pair in ids.windows(2) {
                    let t = crystal.bigram_bump(pair[0], pair[1], 1);
                    if t != 0 {
                        bumped += 1;
                    }
                }
                json!({
                    "id": id, "ok": true,
                    "words": words.len() as u64,
                    "new_words": new_words,
                    "pairs_bumped": bumped,
                    "vocab": crystal.vocab() as u64,
                    "hebbian": true,
                })
            }

            "crystal_assoc" => {
                // Резонансный запрос к Кристаллу: семантические соседи
                // (CSE-триты) + партнёры совместной встречаемости (биграммы
                // ±1 в обе стороны) — ассоциативная память агента.
                let Some(word) = v.get("word").and_then(|x| x.as_str()) else {
                    return Some(json!({"id": id, "ok": false,
                        "error": "crystal_assoc: word field is required"}));
                };
                let sem_k = v
                    .get("sem_k")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(4)
                    .clamp(1, 32) as usize;
                let bi_k = v
                    .get("bi_k")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(8)
                    .clamp(1, 64) as usize;
                let crystal = match self.crystal_mut() {
                    Ok(c) => c,
                    Err(e) => {
                        return Some(json!({"id": id, "ok": false, "error": e}))
                    }
                };
                // id: прямое попадание либо subword-фолбек (жадное
                // разложение слова на известные подтокены).
                let idx = match crystal.id_of(word) {
                    Some(i) => i,
                    None => match crystal.subword_ids(word).first() {
                        Some(&i) => i,
                        None => {
                            return Some(json!({"id": id, "ok": false,
                                "error": format!("crystal_assoc: word {word:?} not found in crystal"),
                                "hint": "index it first via crystal_note"}))
                        }
                    },
                };
                let token_of = |i: u32| crystal.tokens[i as usize].clone();
                let semantic: Vec<String> = crystal
                    .semantic_neighbors(idx, sem_k)
                    .into_iter()
                    .map(token_of)
                    .collect();
                // Со-встречаемость: прямые (+1 в строке) и обратные (+1 в
                // столбце) партнёры — «срабатывали вместе — связаны».
                let v_sz = crystal.vocab() as u32;
                let mut cooccurrence: Vec<String> = Vec::new();
                for j in 0..v_sz {
                    if j != idx && crystal.bigram_trit(idx, j) == 1 {
                        cooccurrence.push(token_of(j));
                    }
                }
                for j in 0..v_sz {
                    if j != idx
                        && crystal.bigram_trit(j, idx) == 1
                        && !cooccurrence.contains(&crystal.tokens[j as usize].clone())
                    {
                        cooccurrence.push(token_of(j));
                    }
                }
                cooccurrence.truncate(bi_k.max(sem_k));
                let assoc: Vec<String> = crystal
                    .assoc_neighbors(idx, sem_k, bi_k)
                    .into_iter()
                    .map(token_of)
                    .collect();
                json!({
                    "id": id, "ok": true, "word": word,
                    "token": crystal.tokens[idx as usize],
                    "anchor": crystal.anchor(idx),
                    "semantic": semantic,
                    "cooccurrence": cooccurrence,
                    "assoc": assoc,
                    "vocab": crystal.vocab() as u64,
                })
            }

            "crystal_stats" => {
                // Паспорт Кристалла Знаний сессии (.t5c v1).
                let crystal = match self.crystal_mut() {
                    Ok(c) => c,
                    Err(e) => {
                        return Some(json!({"id": id, "ok": false, "error": e}))
                    }
                };
                json!({
                    "id": id, "ok": true,
                    "format": "t5c/v1", "magic": "T5CRYS",
                    "vocab": crystal.vocab() as u64,
                    "corpus_words": crystal.corpus_words,
                    "corpus_chars": crystal.corpus_chars,
                    "bigram_nonzeros": crystal.bigram_nonzeros,
                    "dims": crystal.dims as u64,
                    "theta_hi": crystal.theta_hi(),
                    "archetypes": crate::crystal::ARCHETYPES as u64,
                    "trits_per_byte": 5,
                    "source": self.crystal_source,
                })
            }

            "crystal_load" => {
                // Смонтировать .t5c с диска (SHA-256 тела проверяется
                // загрузчиком формата; повреждённый кристалл отвергается).
                let Some(p) = req_path(v) else {
                    return Some(json!({"id": id, "ok": false,
                        "error": "crystal_load: path field is required"}));
                };
                let dims = v
                    .get("dims")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(crate::crystal::DEFAULT_DIMS as u64)
                    .clamp(16, 512) as usize;
                let bytes = match std::fs::read(&p) {
                    Ok(b) => b,
                    Err(e) => {
                        return Some(json!({"id": id, "ok": false,
                            "error": format!("crystal_load: {}: {e}", p.display())}))
                    }
                };
                match crate::crystal::Crystal::load(&bytes, dims) {
                    Ok(c) => {
                        let stats = json!({
                            "vocab": c.vocab() as u64,
                            "corpus_words": c.corpus_words,
                            "corpus_chars": c.corpus_chars,
                            "bigram_nonzeros": c.bigram_nonzeros,
                            "dims": c.dims as u64,
                            "theta_hi": c.theta_hi(),
                        });
                        self.crystal = Some(c);
                        self.crystal_source = Some(p.display().to_string());
                        json!({"id": id, "ok": true, "path": p,
                            "bytes": bytes.len() as u64, "stats": stats})
                    }
                    Err(e) => json!({"id": id, "ok": false,
                        "error": format!("crystal_load: {e}")}),
                }
            }

            "crystal_save" => {
                // Сохранить Кристалл сессии как .t5c (детерминированная
                // сериализация + SHA-256 тела; файл 20 КБ – 2 МБ).
                let Some(p) = req_path(v) else {
                    return Some(json!({"id": id, "ok": false,
                        "error": "crystal_save: path field is required"}));
                };
                let crystal = match self.crystal_mut() {
                    Ok(c) => c,
                    Err(e) => {
                        return Some(json!({"id": id, "ok": false, "error": e}))
                    }
                };
                let n = crystal.to_bytes().len() as u64;
                match crystal.save(&p) {
                    Ok(()) => json!({
                        "id": id, "ok": true, "path": p,
                        "bytes": n, "format": "t5c/v1",
                    }),
                    Err(e) => json!({"id": id, "ok": false, "error": e}),
                }
            }

            // ── Фаза 5+ «Живой движок»: пол демона poler-engine ──
            // Редактор как тонкий терминал: ε-поиск/grep — через живой MCP-гейт
            // демона; дотискание форматов (--pack/--stream-file), листинг архивов,
            // AIDDE impact и кристаллизация корпусов — one-shot CLI движка.
            "engine_status" => {
                match crate::engine_gate::find_engine_bin() {
                    None => json!({"id": id, "ok": false, "error":
                        "poler-engine не найден: установите движок в PATH / ~/.local/bin, задайте POLER_ENGINE_BIN или соберите poler-engine/target/release"}),
                    Some(bin) => {
                        let version = crate::engine_gate::engine_version(&bin)
                            .unwrap_or_else(|_| "unknown".into());
                        let mut resp = json!({
                            "id": id, "ok": true,
                            "bin": bin.display().to_string(),
                            "version": version,
                            "core_version": env!("CARGO_PKG_VERSION"),
                        });
                        // Phase 5++.A: engine_status — единственный источник
                        // правды о демоне (rss/port/tools/счётчики/log_tail —
                        // аддитивные поля, старые клиенты их игнорируют).
                        // Мёртвый демон (крах/kill -9) виден сразу: событие
                        // down + сброс, следующий engine_* поднимет заново.
                        if let Some(dm) = self.engine.as_mut() {
                            if dm.is_alive() {
                                resp["daemon"] = dm.status_json();
                            } else {
                                // смерть демона БЕЗ события (строка до ответа ломает
                                // построчных клиентов): честный running:false в ответе;
                                // панель гасит индикатор опросом (≤ 2 с)
                                self.engine = None;
                                resp["daemon"] = json!({"running": false,
                                    "hint": "демон умер — поднимется при следующем engine_*"});
                            }
                        } else {
                            resp["daemon"] = json!({"running": false,
                                "hint": "демон поднимается лениво при первом engine_search / engine_grep (или engine_daemon_start)"});
                        }
                        resp
                    }
                }
            }

            // ── Phase 5++.A: управление демоном из панели DaemonPanel ──
            // Событие up летит ТОЛЬКО отсюда (и с engine_daemon_stop — down):
            // клиент явно запросил управление и ожидает событий; ленивый
            // спавн от engine_search/engine_grep — тихий (совместимость
            // построчных клиентов: событие до ответа ломает их round-trip).
            "engine_daemon_start" => {
                let was_down = self.engine.is_none();
                match self.engine_daemon_mut() {
                    Err(e) => json!({"id": id, "ok": false, "error": e}),
                    Ok(dm) => {
                        if was_down {
                            if let Some(tx) = gui_tx {
                                let _ = tx.send(
                                    json!({
                                        "ev": "engine_daemon",
                                        "state": "up",
                                        "pid": dm.pid(),
                                        "addr": dm.addr(),
                                    })
                                    .to_string(),
                                );
                            }
                        }
                        let mut resp = json!({"id": id, "ok": true, "started": true});
                        resp["daemon"] = dm.status_json();
                        resp
                    }
                }
            }

            "engine_daemon_stop" => {
                let was_running = self.engine.is_some();
                if let Some(dm) = self.engine.as_mut() {
                    dm.stop();
                }
                self.engine = None;
                if was_running {
                    // Phase 5++.A: событие гибели гасит индикатор в статус-баре.
                    if let Some(tx) = gui_tx {
                        let _ = tx.send(
                            json!({"ev": "engine_daemon", "state": "down"})
                                .to_string(),
                        );
                    }
                    json!({"id": id, "ok": true, "stopped": true})
                } else {
                    json!({"id": id, "ok": true, "stopped": false,
                        "hint": "демон и так не поднят"})
                }
            }

            "engine_search" => {
                let Some(path) = req_path(v) else {
                    return Some(json!({"id": id, "ok": false,
                        "error": "engine_search: path field is required (file or repository root)"}));
                };
                let Some(query) = v.get("query").and_then(|x| x.as_str())
                    .filter(|s| !s.trim().is_empty()) else {
                    return Some(json!({"id": id, "ok": false,
                        "error": "engine_search: query field is required"}));
                };
                let top = u64_of(v, "top").clamp(1, 20);
                match self.engine_daemon_mut() {
                    Err(e) => json!({"id": id, "ok": false, "error": e}),
                    Ok(d) => match d.tool("poler_search",
                        json!({"path": path.display().to_string(), "query": query, "top": top}))
                    {
                        Err(e) => json!({"id": id, "ok": false, "error": e}),
                        Ok(r) => {
                            let is_err = r.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false);
                            if is_err {
                                let msg = r.get("text").and_then(|t| t.as_str())
                                    .unwrap_or("engine search error");
                                json!({"id": id, "ok": false, "error": msg})
                            } else {
                                let text = r.get("text").and_then(|t| t.as_str()).unwrap_or("");
                                let (hits, shown) = crate::engine_gate::parse_search_hits(text)
                                    .unwrap_or((0, 0));
                                json!({"id": id, "ok": true,
                                    "total_hits": hits, "shown": shown,
                                    "engine": "poler-engine", "text": text})
                            }
                        }
                    },
                }
            }

            "engine_grep" => {
                let Some(pattern) = v.get("pattern").and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty()) else {
                    return Some(json!({"id": id, "ok": false,
                        "error": "engine_grep: pattern field is required"}));
                };
                let path = v.get("path").and_then(|x| x.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| ".".to_string());
                let output = match v.get("output").and_then(|x| x.as_str()) {
                    Some("count") | Some("list") | Some("list_nonmatching") => {
                        v.get("output").and_then(|x| x.as_str()).unwrap()
                    }
                    _ => "content",
                };
                let mut args = json!({
                    "path": path, "pattern": pattern, "output": output, "json": true,
                    "ignore_case": v.get("ignore_case").and_then(|x| x.as_bool()).unwrap_or(false),
                    "regex": v.get("regex").and_then(|x| x.as_bool()).unwrap_or(false),
                    "after": u64_of(v, "after").clamp(0, 50),
                    "before": u64_of(v, "before").clamp(0, 50),
                });
                let mc = u64_of(v, "max_count");
                if mc > 0 {
                    args["max_count"] = json!(mc);
                }
                match self.engine_daemon_mut() {
                    Err(e) => json!({"id": id, "ok": false, "error": e}),
                    Ok(d) => match d.tool("poler_grep", args) {
                        Err(e) => json!({"id": id, "ok": false, "error": e}),
                        Ok(r) => {
                            let is_err = r.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false);
                            if is_err {
                                json!({"id": id, "ok": false,
                                    "error": r.get("text").cloned()
                                        .unwrap_or(json!("grep error"))})
                            } else {
                                let mut resp = r;
                                if let Some(obj) = resp.as_object_mut() {
                                    obj.remove("is_error");
                                }
                                resp["id"] = json!(id);
                                resp["ok"] = json!(true);
                                resp
                            }
                        }
                    },
                }
            }

            "engine_pack" => {
                let Some(dir) = req_path(v).filter(|p| p.is_dir()) else {
                    return Some(json!({"id": id, "ok": false,
                        "error": "engine_pack: path field is required (directory to pack into .poler)"}));
                };
                let out = match v.get("out").and_then(|x| x.as_str())
                    .filter(|s| !s.trim().is_empty()) {
                    Some(s) => PathBuf::from(s),
                    None => PathBuf::from(format!("{}.poler", dir.display())),
                };
                let gitignore = v.get("gitignore").and_then(|x| x.as_bool()).unwrap_or(false);
                let jobs = u64_of(v, "jobs");
                match crate::engine_gate::find_engine_bin() {
                    None => json!({"id": id, "ok": false, "error": "poler-engine not found in PATH or ~/.local/bin"}),
                    Some(bin) => match crate::engine_gate::pack_dir(&bin, &dir, &out, gitignore, jobs) {
                        Ok(stats) => json!({"id": id, "ok": true, "stats": stats}),
                        Err(e) => json!({"id": id, "ok": false, "error": e}),
                    },
                }
            }

            "engine_squeeze" => {
                let Some(file) = req_path(v).filter(|p| p.is_file()) else {
                    return Some(json!({"id": id, "ok": false,
                        "error": "engine_squeeze: path field is required (container file: tar / tar.gz / tar.zst / gz / zst / ...)"}));
                };
                let out = match v.get("out").and_then(|x| x.as_str())
                    .filter(|s| !s.trim().is_empty()) {
                    Some(s) => PathBuf::from(s),
                    None => file.with_extension("poler"),
                };
                match crate::engine_gate::find_engine_bin() {
                    None => json!({"id": id, "ok": false, "error": "poler-engine not found in PATH or ~/.local/bin"}),
                    Some(bin) => match crate::engine_gate::stream_file(&bin, &file, &out) {
                        Ok(stats) => json!({"id": id, "ok": true, "stats": stats}),
                        Err(e) => json!({"id": id, "ok": false, "error": e}),
                    },
                }
            }

            "engine_archive_ls" => {
                let Some(path) = req_path(v).filter(|p| p.is_file()) else {
                    return Some(json!({"id": id, "ok": false,
                        "error": "engine_archive_ls: path field is required (archive: .poler / .zip / .tar / .epub / ...)"}));
                };
                match crate::engine_gate::find_engine_bin() {
                    None => json!({"id": id, "ok": false, "error": "poler-engine not found in PATH or ~/.local/bin"}),
                    Some(bin) => match crate::engine_gate::archive_list(&bin, &path) {
                        Ok(listing) => json!({"id": id, "ok": true, "listing": listing}),
                        Err(e) => json!({"id": id, "ok": false, "error": e}),
                    },
                }
            }

            "engine_impact" => {
                let Some(root) = req_path(v).filter(|p| p.is_dir()) else {
                    return Some(json!({"id": id, "ok": false,
                        "error": "engine_impact: path field is required (project root for AIDDE call-graph)"}));
                };
                let Some(symbol) = v.get("symbol").and_then(|x| x.as_str())
                    .filter(|s| !s.trim().is_empty()) else {
                    return Some(json!({"id": id, "ok": false,
                        "error": "engine_impact: symbol field is required (function or struct name)"}));
                };
                let depth = u64_of(v, "depth");
                match crate::engine_gate::find_engine_bin() {
                    None => json!({"id": id, "ok": false, "error": "poler-engine not found in PATH or ~/.local/bin"}),
                    Some(bin) => match crate::engine_gate::impact(&bin, &root, symbol, depth) {
                        Ok(imp) => json!({"id": id, "ok": true, "impact": imp}),
                        Err(e) => json!({"id": id, "ok": false, "error": e}),
                    },
                }
            }

            "engine_crystal_ingest" => {
                let Some(path) = req_path(v).filter(|p| p.exists()) else {
                    return Some(json!({"id": id, "ok": false,
                        "error": "engine_crystal_ingest: path field is required (text directory or .poler archive)"}));
                };
                let out = match v.get("out").and_then(|x| x.as_str())
                    .filter(|s| !s.trim().is_empty()) {
                    Some(s) => PathBuf::from(s),
                    None => path.with_extension("t5c"),
                };
                let vocab = u64_of(v, "vocab");
                match crate::engine_gate::find_engine_bin() {
                    None => json!({"id": id, "ok": false, "error": "poler-engine not found in PATH or ~/.local/bin"}),
                    Some(bin) => match crate::engine_gate::crystal_ingest(&bin, &path, &out, vocab) {
                        Ok(mut c) => {
                            c["id"] = json!(id);
                            c
                        }
                        Err(e) => json!({"id": id, "ok": false, "error": e}),
                    },
                }
            }

            "silicon_info" => {
                json!({
                    "id": id,
                    "ok": true,
                    "version": env!("CARGO_PKG_VERSION"),
                    "trit_vacuum_byte": TRIT5_ZERO_CODE,
                    "silicon": caps(),
                })
            }

            "search" => match self.resolve_or_open_doc(v) {
                Ok(doc) => {
                    let query: String = v
                        .get("query")
                        .or_else(|| v.get("pattern"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .chars()
                        .take(4096)
                        .collect();
                    let case_sensitive = v
                        .get("case_sensitive")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(true);
                    // Ранжирование (MCP-слой ИИ): rank_by != byte и/или порог
                    // min_epsilon → собираем расширенный пул кандидатов,
                    // фильтруем по ε, сортируем и только потом режем по limit.
                    let rank_by = v
                        .get("rank_by")
                        .and_then(|x| x.as_str())
                        .unwrap_or("byte")
                        .to_ascii_lowercase();
                    let min_epsilon = v.get("min_epsilon").and_then(|x| x.as_f64());
                    let limit = u64_of(v, "limit").clamp(1, 100_000) as usize;
                    let effective_limit = if rank_by != "byte" || min_epsilon.is_some() {
                        limit.max(2000)
                    } else {
                        limit
                    };
                    let b = self.docs.map.get_mut(&doc).unwrap();
                    match b.search(&query, case_sensitive, effective_limit, &self.cancel) {
                        Ok(st) => {
                            let mut hits = st.hits;
                            let pool = hits.len() as u64;
                            let dropped_low_eps = match min_epsilon {
                                Some(m) => {
                                    let before = hits.len();
                                    hits.retain(|h| h.eps >= m);
                                    (before - hits.len()) as u64
                                }
                                None => 0,
                            };
                            match rank_by.as_str() {
                                "epsilon" => hits.sort_by(|a, b| b.eps.total_cmp(&a.eps)),
                                "resonance" => hits.sort_by(|a, b| b.r_q8.cmp(&a.r_q8)),
                                _ => {} // "byte": выдача уже в байтовом порядке
                            }
                            let rank_truncated = hits.len() > limit;
                            hits.truncate(limit);
                            json!({
                                "id": id, "ok": true, "doc": doc,
                                "rev": b.rev(),
                                "hits": hits.iter().map(hit_json_buffer).collect::<Vec<_>>(),
                                "ranked_by": rank_by,
                                "epsilon_filter": min_epsilon,
                                "dropped_low_epsilon": dropped_low_eps,
                                "pool": pool,
                                "rank_truncated": rank_truncated,
                                "truncated": st.truncated,
                                "cancelled": st.cancelled,
                                "scanned_bytes": st.scanned_bytes,
                                "elapsed_ms": st.elapsed.as_millis() as u64,
                            })
                        }
                        Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                    }
                }
                Err(e) => json!({"id": id, "ok": false, "error": e}),
            },

            // =============================================================
            // Виртуальные глаза ИИ: ε_high (saccade) / ε_low (scroll) +
            // удержание курсора «глаза» между вызовами (seek_energy).
            // =============================================================
            "saccade" => match self.resolve_or_open_doc(v) {
                Ok(doc) => {
                    let max_nodes = v
                        .get("max_nodes")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(120)
                        .clamp(1, 400) as usize;
                    // Скелет символов: тот же extractor, что и "outline"
                    // (ограничение 16 МиБ унаследовано честно; ε-пики ниже
                    // считаются по ВСЕМУ документу без ограничения).
                    let (rev, outline) = {
                        let b = &self.docs.map[&doc];
                        let cap = (b.total_len() as usize).min(16 << 20);
                        let raw = b.read_range(0, cap);
                        (b.rev(), extract_outline_from_bytes(&raw, Some(b.path())))
                    };
                    let (prof, cached_peaks) = match self.eye_profile_and_peaks(doc) {
                        Ok(p) => p,
                        Err(e) => return Some(json!({"id": id, "ok": false, "error": e})),
                    };
                    let total_lines = prof.len() as u64;
                    let nodes: Vec<Value> = outline
                        .iter()
                        .take(max_nodes)
                        .map(|it| {
                            let eps = prof.get(it.line as usize).map(|&e| e as f64).unwrap_or(0.0);
                            json!({
                                "line": it.line, "end_line": it.end_line,
                                "name": it.name, "kind": it.kind, "level": it.level,
                                "eps": round2(eps),
                            })
                        })
                        .collect();
                    let peaks: Vec<Value> = cached_peaks
                        .iter()
                        .take(max_nodes)
                        .map(|p| json!({ "line": p.line, "eps": round2(p.eps) }))
                        .collect();
                    let eye = self.eye_cursors.lock().unwrap().get(&doc).copied().unwrap_or(0);
                    json!({
                        "id": id, "ok": true, "doc": doc, "rev": rev,
                        "total_lines": total_lines,
                        "nodes": nodes,
                        "peaks": peaks,
                        "eye": eye,
                        "hint": "ε_high готов: 1 шаг = карта документа. Дальше poler_scroll(path, from_line=<line>) — фовеальное окно, или poler_seek_energy(direction=\"next_peak\") — прыжок к пику.",
                    })
                }
                Err(e) => json!({"id": id, "ok": false, "error": e}),
            },

            "scroll" => match self.resolve_or_open_doc(v) {
                Ok(doc) => {
                    let lines_n = v
                        .get("lines")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(25)
                        .clamp(1, 1000) as usize;
                    let backward = v
                        .get("direction")
                        .and_then(|x| x.as_str())
                        .map(|d| d.eq_ignore_ascii_case("backward"))
                        .unwrap_or(false);
                    let from_line_opt = v.get("from_line").and_then(|x| x.as_u64());
                    let cur = self.eye_cursors.lock().unwrap().get(&doc).copied().unwrap_or(0);
                    let anchor = from_line_opt.unwrap_or(cur);
                    let start = if backward {
                        anchor.saturating_sub(lines_n as u64)
                    } else {
                        anchor
                    };
                    let b = self.docs.map.get_mut(&doc).unwrap();
                    match b.read_span(start, lines_n, 4 << 20) {
                        Ok(sp) => {
                            // Курсор «глаза»: forward — первая НЕпрочитанная строка;
                            // backward — верх окна (повторный forward ПЕРЕЧИТАЕТ окно:
                            // перечитывание строки остановки — сознательная фича).
                            let new_cursor = if backward { sp.start_line } else { sp.end_line };
                            let offset = b.offset_of_line(new_cursor).unwrap_or(sp.end_byte);
                            let prev = self
                                .eye_cursors
                                .lock()
                                .unwrap()
                                .insert(doc, new_cursor)
                                .unwrap_or(start);
                            let mut parts: Vec<&str> = sp.text.split('\n').collect();
                            if sp.text.ends_with('\n') {
                                parts.pop();
                            }
                            json!({
                                "id": id, "ok": true, "doc": doc, "rev": sp.rev,
                                "lines": parts.iter().enumerate().map(|(i, t)| json!({
                                    "line": sp.start_line + i as u64,
                                    "text": t.trim_end_matches('\r'),
                                })).collect::<Vec<_>>(),
                                "cursor": { "line": new_cursor, "offset": offset },
                                "prev_cursor": prev,
                                "start_line": sp.start_line,
                                "end_line_read": sp.end_line,
                                "direction": if backward { "backward" } else { "forward" },
                                "eof": sp.total_bytes > 0 && sp.end_byte >= sp.total_bytes,
                            })
                        }
                        Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                    }
                }
                Err(e) => json!({"id": id, "ok": false, "error": e}),
            },

            "seek_energy" => match self.resolve_or_open_doc(v) {
                Ok(doc) => {
                    let prev_dir = v
                        .get("direction")
                        .and_then(|x| x.as_str())
                        .map(|d| {
                            d.eq_ignore_ascii_case("prev_peak")
                                || d.eq_ignore_ascii_case("previous")
                                || d.eq_ignore_ascii_case("backward")
                        })
                        .unwrap_or(false);
                    let from_line_opt = v.get("from_line").and_then(|x| x.as_u64());
                    let cur = self.eye_cursors.lock().unwrap().get(&doc).copied().unwrap_or(0);
                    let from = from_line_opt.unwrap_or(cur);
                    let (prof, peaks) = match self.eye_profile_and_peaks(doc) {
                        Ok(p) => p,
                        Err(e) => return Some(json!({"id": id, "ok": false, "error": e})),
                    };
                    let found = if prev_dir {
                        peaks.iter().rev().find(|p| p.line < from).copied()
                    } else {
                        peaks.iter().find(|p| p.line > from).copied()
                    };
                    let Some(peak) = found else {
                        return Some(json!({
                            "id": id, "ok": true, "doc": doc, "found": false, "cursor": from,
                            "hint": if prev_dir {
                                "пиков ниже текущей позиции нет — вызовите poler_saccade для карты документа"
                            } else {
                                "пиков выше текущей позиции нет — достигнут конец; вызовите poler_scroll для чтения хвоста"
                            },
                        }));
                    };
                    // Фовеальное окно 5 строк на пике + перенос курсора «глаза».
                    let b = self.docs.map.get_mut(&doc).unwrap();
                    match b.read_span(peak.line, 5, 1 << 20) {
                        Ok(sp) => {
                            let new_cursor = sp.end_line;
                            let offset = b.offset_of_line(new_cursor).unwrap_or(sp.end_byte);
                            self.eye_cursors.lock().unwrap().insert(doc, new_cursor);
                            let mut parts: Vec<&str> = sp.text.split('\n').collect();
                            if sp.text.ends_with('\n') {
                                parts.pop();
                            }
                            json!({
                                "id": id, "ok": true, "doc": doc, "found": true,
                                "peak": { "line": peak.line, "eps": round2(peak.eps) },
                                "cursor": { "line": new_cursor, "offset": offset },
                                "lines": parts.iter().enumerate().map(|(i, t)| json!({
                                    "line": sp.start_line + i as u64,
                                    "text": t.trim_end_matches('\r'),
                                })).collect::<Vec<_>>(),
                                "hint": "пик ε достигнут; продолжайте poler_scroll(direction=\"forward\") или ищите следующий пик",
                            })
                        }
                        Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                    }
                }
                Err(e) => json!({"id": id, "ok": false, "error": e}),
            },

            "search_live" => {
                let doc = doc_of(v);
                let query: String = v
                    .get("query")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .chars()
                    .take(4096)
                    .collect();
                let case_sensitive = v
                    .get("case_sensitive")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(true);
                let limit = u64_of(v, "limit").clamp(1, 100_000) as usize;
                let vp = v.get("viewport").and_then(|x| x.as_array()).map(|a| {
                    let from = a.first().and_then(|x| x.as_u64()).unwrap_or(0);
                    let to = a.get(1).and_then(|x| x.as_u64()).unwrap_or(0);
                    (from, to)
                });
                let (vp_from, vp_to) = vp.unwrap_or((0, 0));

                match self.docs.map.get_mut(&doc) {
                    Some(b) => {
                        if query.is_empty() {
                            if let Ok(mut s) = self.sessions.lock() {
                                if let Some(tok) = s.remove(&doc) {
                                    tok.store(true, Ordering::Relaxed);
                                }
                            }
                            return Some(json!({
                                "id": id, "ok": true, "sid": 0, "started": false, "hits": 0
                            }));
                        }
                        let Some(tx) = gui_tx.cloned() else {
                            return Some(json!({
                                "id": id, "ok": false,
                                "error": "search_live requires a streaming event channel (use search in stateless mode)"
                            }));
                        };
                        let total = b.total_len();
                        let vp_bytes = if vp_to > vp_from {
                            let lo = b.offset_of_line(vp_from).unwrap_or(0).min(total);
                            let hi = b.offset_of_line(vp_to).unwrap_or(total).min(total);
                            if hi > lo {
                                Some((lo, hi, vp_from, lo))
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        let snap = b.snapshot();
                        let token = Arc::new(AtomicBool::new(false));
                        if let Ok(mut s) = self.sessions.lock() {
                            if let Some(old) = s.insert(doc, Arc::clone(&token)) {
                                old.store(true, Ordering::Relaxed);
                            }
                        }
                        let sid = self.sid_counter.fetch_add(1, Ordering::Relaxed) + 1;
                        thread::spawn(move || {
                            let spec = LiveSpec {
                                needle: query,
                                case_sensitive,
                                limit,
                                viewport: vp_bytes,
                            };
                            let mut sink = JsonEvents {
                                tx: tx.clone(),
                                sid,
                                doc,
                            };
                            let sum = search::run_live(&snap, &spec, &token, &mut sink);
                            let gbps = if sum.elapsed.as_secs_f64() > 0.0 {
                                sum.scanned as f64 / 1_073_741_824.0 / sum.elapsed.as_secs_f64()
                            } else {
                                0.0
                            };
                            let _ = tx.send(
                                json!({
                                    "ev": "search_done", "id": id, "sid": sid, "doc": doc,
                                    "hits": sum.hits, "truncated": sum.truncated,
                                    "cancelled": sum.cancelled, "scanned": sum.scanned,
                                    "elapsed_ms": sum.elapsed.as_millis() as u64,
                                    "gbps": round2(gbps),
                                    "R": round2(sum.resonance),
                                    "R_q8": sum.resonance_q8,
                                    "viewport_ms": round2(sum.viewport_ms),
                                })
                                .to_string(),
                            );
                        });
                        json!({
                            "id": id, "ok": true, "sid": sid, "started": true,
                            "limit": limit, "total_bytes": total,
                        })
                    }
                    None => json!({"id": id, "ok": false, "error": "doc not found: no open document with this id"}),
                }
            }

            "license_activate" => match v.get("key").and_then(|x| x.as_str()) {
                Some(k) => {
                    let info = license::validate_key(k);
                    json!({
                        "id": id, "ok": true,
                        "valid": info.valid,
                        "name": info.name,
                        "licensee": info.name,
                        "edition": info.edition,
                        "expires": info.expires,
                        "perpetual": info.perpetual,
                        "reason": info.reason,
                    })
                }
                None => json!({"id": id, "ok": false, "error": "license_activate: key field is required"}),
            },

            "index" => match self.resolve_or_open_doc(v) {
                Ok(doc) => {
                    let tx_opt = gui_tx.cloned();
                    let started = Instant::now();
                    let mut last = 0u128;
                    let b = self.docs.map.get_mut(&doc).unwrap();
                    let r = b.index_all(&self.cancel, move |done, total| {
                        if let Some(ref tx2) = tx_opt {
                            let now = started.elapsed().as_millis();
                            if now.saturating_sub(last) >= 120 || done == total {
                                last = now;
                                let gbps = if now > 0 {
                                    (done as f64 / 1024.0 / 1024.0 / 1024.0) / (now as f64 / 1000.0)
                                } else {
                                    0.0
                                };
                                let _ = tx2.send(
                                    json!({
                                        "ev": "progress", "op": "index", "doc": doc,
                                        "done_bytes": done, "total_bytes": total,
                                        "gbps": (gbps * 10.0).round() / 10.0,
                                    })
                                    .to_string(),
                                );
                            }
                        }
                    });
                    match r {
                        Ok((lines, done)) => {
                            json!({"id": id, "ok": true, "lines": lines, "completed": done})
                        }
                        Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                    }
                }
                Err(e) => json!({"id": id, "ok": false, "error": e}),
            },

            "reload" => match self.resolve_or_open_doc(v) {
                Ok(doc) => {
                    let b = self.docs.map.get_mut(&doc).unwrap();
                    if let Some(cas_err) = check_cas(id, v, b) {
                        return Some(cas_err);
                    }
                    match b.reload() {
                        Ok(()) => {
                            notify_gui_agent_edit(from_agent, gui_tx, doc, "reload", b, 0);
                            json!({
                                "id": id, "ok": true, "doc": doc,
                                "bytes": b.total_len(),
                                "lines": b.lines_if_known(),
                                "rev": b.rev(),
                            })
                        }
                        Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                    }
                }
                Err(e) => json!({"id": id, "ok": false, "error": e}),
            },

            "save" => match self.resolve_or_open_doc(v) {
                Ok(doc) => {
                    if self.vfs_docs.contains_key(&doc) {
                        let rev = self.docs.map.get(&doc).map(|b| b.rev()).unwrap_or(0);
                        return Some(json!({
                            "id": id, "ok": false, "doc": doc, "rev": rev,
                            "error": "save: this is a VFS buffer inside a container; use vfs_save + vfs_flush"
                        }));
                    }
                    let force = v.get("force").and_then(|x| x.as_bool()).unwrap_or(false);
                    let b = self.docs.map.get_mut(&doc).unwrap();
                    if let Some(cas_err) = check_cas(id, v, b) {
                        return Some(cas_err);
                    }
                    let is_pdf = b.path().extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("pdf")).unwrap_or(false);
                    if !is_pdf && !force && crate::extractor::is_extractable_format(b.path()) {
                        return Some(json!({
                            "id": id, "ok": false, "doc": doc, "rev": b.rev(),
                            "error": "save blocked: document extracted from binary container (DOCX/EPUB/ODT/FB2) in read-only mode; for in-place archive editing use vfs_open -> vfs_save -> vfs_flush (or save_as to text file)"
                        }));
                    }
                    if is_pdf {
                        let raw_bytes = b.read_range(0, b.total_len() as usize);
                        let text = String::from_utf8_lossy(&raw_bytes);
                        match crate::pdf_edit::save_pdf_buffer(b.path(), &text) {
                            Ok(pdf_n) => {
                                b.mark_clean();
                                b.refresh_disk_stamp();
                                notify_gui_agent_edit(from_agent, gui_tx, doc, "save", b, 0);
                                json!({
                                    "id": id, "ok": true, "doc": doc,
                                    "bytes": pdf_n, "path": b.path(), "rev": b.rev(),
                                })
                            }
                            Err(e) => json!({"id": id, "ok": false, "error": e.to_string(), "rev": b.rev()}),
                        }
                    } else {
                        match b.save_checked(force) {
                            Ok(n) => {
                                notify_gui_agent_edit(from_agent, gui_tx, doc, "save", b, 0);
                                if let Some(ref ep) = self.event_plane {
                                    ep.registry().set_dirty(b.path(), false);
                                    ep.broadcast(crate::event_plane::Event::EditApplied {
                                        buffer_id: doc,
                                        path: b.path().to_string_lossy().to_string(),
                                        revision: b.rev(),
                                        hash: String::new(),
                                    });
                                }
                                json!({
                                    "id": id, "ok": true, "doc": doc,
                                    "bytes": n, "path": b.path(), "rev": b.rev(),
                                })
                            }
                            Err(e) => json!({"id": id, "ok": false, "error": e.to_string(), "rev": b.rev()}),
                        }
                    }
                }
                Err(e) => json!({"id": id, "ok": false, "error": e}),
            },

            "save_as" => {
                let Some(p) = req_path(v) else {
                    return Some(json!({"id": id, "ok": false, "error": "save_as: path field is required"}));
                };
                let doc = doc_of(v);
                match self.docs.map.get_mut(&doc) {
                    Some(b) => {
                        if let Some(cas_err) = check_cas(id, v, b) {
                            return Some(cas_err);
                        }
                        let is_pdf = p.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("pdf")).unwrap_or(false);
                        if is_pdf {
                            let raw_bytes = b.read_range(0, b.total_len() as usize);
                            let text = String::from_utf8_lossy(&raw_bytes);
                            match crate::pdf_edit::save_pdf_buffer(&p, &text) {
                                Ok(pdf_n) => {
                                    b.set_path(&p);
                                    b.mark_clean();
                                    b.refresh_disk_stamp();
                                    notify_gui_agent_edit(from_agent, gui_tx, doc, "save_as", b, 0);
                                    json!({
                                        "id": id, "ok": true, "doc": doc,
                                        "bytes": pdf_n, "path": p, "rev": b.rev(),
                                    })
                                }
                                Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                            }
                        } else {
                            match b.save_as(&p) {
                                Ok(n) => {
                                    notify_gui_agent_edit(from_agent, gui_tx, doc, "save_as", b, 0);
                                    json!({
                                        "id": id, "ok": true, "doc": doc,
                                        "bytes": n, "path": p, "rev": b.rev(),
                                    })
                                }
                                Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                            }
                        }
                    }
                    None => json!({"id": id, "ok": false, "error": "doc not found: no open document with this id"}),
                }
            }

            "tx_save_all" => {
                let force = v.get("force").and_then(|x| x.as_bool()).unwrap_or(false);
                let target_ids: Vec<u64> = if let Some(arr) = v.get("docs").and_then(|x| x.as_array()) {
                    arr.iter().filter_map(|x| x.as_u64()).collect()
                } else {
                    let mut ids: Vec<u64> = self
                        .docs
                        .map
                        .iter()
                        .filter(|(_, b)| b.is_dirty())
                        .map(|(&d, _)| d)
                        .collect();
                    ids.sort_unstable();
                    ids
                };

                // Фаза 1 (Preflight): проверяем существование и отсутствие конфликтов диска по ВСЕМ файлам до записи!
                if !force {
                    let mut conflicts = Vec::new();
                    for &d in &target_ids {
                        let Some(b) = self.docs.map.get(&d) else {
                            return Some(json!({
                                "id": id, "ok": false,
                                "error": format!("doc_not_found: doc={d}")
                            }));
                        };
                        if b.disk_changed_externally() {
                            conflicts.push(json!({"doc": d, "path": b.path()}));
                        }
                    }
                    if !conflicts.is_empty() {
                        return Some(json!({
                            "id": id, "ok": false,
                            "error": "disk_conflict: one or more files modified externally (no files overwritten)",
                            "conflicts": conflicts,
                        }));
                    }
                }

                // Фаза 2 (Commit): атомарная запись каждого документа.
                let mut saved = Vec::new();
                for d in target_ids {
                    if let Some(b) = self.docs.map.get_mut(&d) {
                        match b.save_checked(true) {
                            Ok(bytes) => {
                                notify_gui_agent_edit(from_agent, gui_tx, d, "tx_save_all", b, 0);
                                saved.push(json!({
                                    "doc": d,
                                    "path": b.path(),
                                    "bytes": bytes,
                                    "rev": b.rev(),
                                }));
                            }
                            Err(e) => {
                                return Some(json!({
                                    "id": id, "ok": false,
                                    "error": format!("save_failed on doc {d}: {e}"),
                                    "saved_before_error": saved,
                                }));
                            }
                        }
                    }
                }
                json!({"id": id, "ok": true, "saved": saved})
            }

            "undo" => match self.resolve_or_open_doc(v) {
                Ok(doc) => {
                    let b = self.docs.map.get_mut(&doc).unwrap();
                    if let Some(cas_err) = check_cas(id, v, b) {
                        return Some(cas_err);
                    }
                    let applied = b.undo();
                    if applied {
                        notify_gui_agent_edit(from_agent, gui_tx, doc, "undo", b, 0);
                    }
                    json!({"id": id, "ok": true, "doc": doc, "applied": applied, "rev": b.rev()})
                }
                Err(e) => json!({"id": id, "ok": false, "error": e}),
            },

            "redo" => match self.resolve_or_open_doc(v) {
                Ok(doc) => {
                    let b = self.docs.map.get_mut(&doc).unwrap();
                    if let Some(cas_err) = check_cas(id, v, b) {
                        return Some(cas_err);
                    }
                    let applied = b.redo();
                    if applied {
                        notify_gui_agent_edit(from_agent, gui_tx, doc, "redo", b, 0);
                    }
                    json!({"id": id, "ok": true, "doc": doc, "applied": applied, "rev": b.rev()})
                }
                Err(e) => json!({"id": id, "ok": false, "error": e}),
            },

            "calc" => {
                let expr = v
                    .get("expr")
                    .and_then(|x| x.as_str())
                    .or_else(|| v.get("expression").and_then(|x| x.as_str()))
                    .unwrap_or("");
                let t0 = Instant::now();
                if let Some(engine_bin) = crate::engine_gate::find_engine_bin() {
                    match std::process::Command::new(&engine_bin)
                        .args(&["calc", expr])
                        .output()
                    {
                        Ok(output) => {
                            let ms = t0.elapsed().as_millis() as u64;
                            let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
                            if output.status.success() {
                                json!({
                                    "id": id,
                                    "ok": true,
                                    "output": out,
                                    "result": out,
                                    "duration_ms": ms,
                                })
                            } else {
                                let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
                                json!({
                                    "id": id,
                                    "ok": false,
                                    "error": if err.is_empty() { out } else { err },
                                    "duration_ms": ms,
                                })
                            }
                        }
                        Err(e) => json!({
                            "id": id,
                            "ok": false,
                            "error": format!("Failed to invoke poler-engine: {e}"),
                            "duration_ms": t0.elapsed().as_millis() as u64,
                        }),
                    }
                } else {
                    json!({
                        "id": id,
                        "ok": false,
                        "error": "Calculator is offloaded to sovereign poler-engine. Install poler-engine or use: poler-engine calc",
                        "duration_ms": t0.elapsed().as_millis() as u64,
                    })
                }
            },
            // ---------------- Фаза 6.D «AI Chat» -------------------------
            // Живой канал «Человек ↔ ИИ» в доке редактора. Человек пишет в
            // панель AI Chat → chat_broadcast транслирует текст ВСЕМ
            // подключённым агентам TCP-моста; агент отвечает chat_send →
            // GUI получает событие agent_chat (как agent_edit для правок).
            "chat_broadcast" => {
                let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let role = v.get("role").and_then(|x| x.as_str()).unwrap_or("human").to_string();
                if from_agent {
                    json!({"id": id, "ok": false,
                        "error": "chat_broadcast: GUI (human) command, not for agents"})
                } else if let Some(hub) = self.agent_hub.clone() {
                    let delivered = hub.broadcast(&json!({
                        "ev": "agent_chat", "role": role, "text": text
                    }));
                    json!({"id": id, "ok": true, "delivered": delivered})
                } else {
                    json!({"id": id, "ok": false,
                        "error": "listen socket is not running (start editor with --listen)"})
                }
            }
            "chat_send" => {
                let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if !from_agent {
                    json!({"id": id, "ok": false,
                        "error": "chat_send: agent command (TCP+token); for GUI use chat_broadcast"})
                } else {
                    if let Some(tx) = gui_tx {
                        let _ = tx.send(
                            json!({"ev": "agent_chat", "role": "agent", "text": text}).to_string(),
                        );
                    }
                    json!({"id": id, "ok": true})
                }
            }
            "chat_status" => {
                let agents = self.agent_hub.as_ref().map(|h| h.count()).unwrap_or(0);
                json!({"id": id, "ok": true, "agents": agents})
            }
            other => json!({"id": id, "ok": false, "error": format!("unknown command: {other}")}),
        };

        Some(resp)
    }
}

impl Default for CoreSession {
    fn default() -> Self {
        Self::new()
    }
}

fn check_cas(id: u64, v: &Value, b: &PolerBuffer) -> Option<Value> {
    if let Some(exp) = v.get("expected_rev").and_then(|x| x.as_u64()) {
        if exp != b.rev() {
            return Some(json!({
                "id": id,
                "ok": false,
                "error": format!("stale_revision: expected_rev={exp}, but current buffer rev={}", b.rev()),
                "current_rev": b.rev(),
                "expected_rev": exp,
            }));
        }
    }
    None
}

fn maybe_auto_save(v: &Value, b: &mut PolerBuffer) -> Result<(), String> {
    let auto_save = v.get("auto_save").and_then(|x| x.as_bool()).unwrap_or(false);
    if !auto_save {
        return Ok(());
    }
    let force = v.get("force").and_then(|x| x.as_bool()).unwrap_or(false);
    b.save_checked(force).map(|_| ()).map_err(|e| e.to_string())
}

fn notify_gui_agent_edit(
    from_agent: bool,
    gui_tx: Option<&mpsc::Sender<String>>,
    doc: u64,
    op: &str,
    b: &PolerBuffer,
    focus_line: u64,
) {
    if !from_agent {
        return;
    }
    if let Some(tx) = gui_tx {
        let _ = tx.send(
            json!({
                "ev": "agent_edit",
                "doc": doc,
                "path": b.path(),
                "op": op,
                "rev": b.rev(),
                "bytes": b.total_len(),
                "lines": b.lines_if_known(),
                "dirty": b.is_dirty(),
                "focus_line": focus_line,
            })
            .to_string(),
        );
    }
}

fn open_buffer_for_path(path: &Path) -> io::Result<PolerBuffer> {
    if path.as_os_str().is_empty() {
        return Ok(PolerBuffer::new_empty(path));
    }
    if crate::extractor::is_extractable_format(path) {
        match crate::extractor::extract_text(path) {
            Ok(Some(clean_text)) => {
                let text = if clean_text.trim().is_empty() {
                    format!(
                        "[{}: Документ не містить текстового шару або порожній]",
                        path.extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("")
                            .to_ascii_uppercase()
                    )
                } else {
                    clean_text
                };
                Ok(PolerBuffer::from_text(path, &text))
            }
            Ok(None) => PolerBuffer::open(path),
            Err(e) => {
                let msg = format!(
                    "[Помилка читання документа {}: {}]",
                    path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                    e
                );
                Ok(PolerBuffer::from_text(path, &msg))
            }
        }
    } else {
        match PolerBuffer::open(path) {
            Ok(b) => Ok(b),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(PolerBuffer::new_empty(path)),
            Err(e) => Err(e),
        }
    }
}

struct QueuedRequest {
    req: Value,
    from_agent: bool,
    reply_tx: Option<mpsc::Sender<String>>,
}

/// Сериализатор событий живого поиска в JSON-строки (в writer-поток).
struct JsonEvents {
    tx: mpsc::Sender<String>,
    sid: u64,
    doc: u64,
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn hit_json(h: &HitOut) -> Value {
    json!({
        "byte": h.byte,
        "line": h.line,
        "col": h.col,
        "col_bytes": h.col_bytes,
        "len": h.len,
        "len_chars": h.len_chars,
        "eps": round2(h.eps),
        "R": round2(h.r),
        "proj": h.proj,
        "R_q8": h.r_q8,
    })
}

fn hit_json_buffer(h: &crate::buffer::SearchHit) -> Value {
    json!({
        "byte": h.byte,
        "line": h.line,
        "col": h.col,
        "col_bytes": h.col_bytes,
        "len": h.len,
        "len_chars": h.len_chars,
        "eps": round2(h.eps),
        "proj": h.proj,
        "R_q8": h.r_q8,
    })
}

impl SearchEvents for JsonEvents {
    fn on_hits(&mut self, phase: &str, hits: &[HitOut], scanned: u64, total: u64) {
        let arr: Vec<Value> = hits.iter().map(hit_json).collect();
        let _ = self.tx.send(
            json!({
                "ev": "hits", "sid": self.sid, "doc": self.doc, "phase": phase,
                "hits": arr, "scanned": scanned, "total": total,
            })
            .to_string(),
        );
    }
    fn on_progress(&mut self, scanned: u64, total: u64) {
        let _ = self.tx.send(
            json!({
                "ev": "progress", "op": "search", "sid": self.sid, "doc": self.doc,
                "done_bytes": scanned, "total_bytes": total,
            })
            .to_string(),
        );
    }
    fn on_viewport_done(&mut self, ms: f64) {
        let _ = self.tx.send(
            json!({
                "ev": "viewport_done", "sid": self.sid, "doc": self.doc, "ms": round2(ms),
            })
            .to_string(),
        );
    }
}

fn generate_session_token() -> String {
    let s1 = RandomState::new();
    let s2 = RandomState::new();
    let mut h1 = s1.build_hasher();
    h1.write_u32(std::process::id());
    let mut h2 = s2.build_hasher();
    h2.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    format!("{:016x}{:016x}", h1.finish(), h2.finish())
}

/// Отправить единичную JSON-команду в живой GUI-сервер по TCP (`POLER_EDIT_ADDR` + `POLER_EDIT_TOKEN`).
pub fn send_tcp_command(addr: &str, token: &str, mut cmd: Value) -> Result<Value, String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("tcp_connect({addr}): {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    if let Some(obj) = cmd.as_object_mut() {
        obj.insert("token".to_string(), Value::String(token.to_string()));
    }
    let payload = format!("{}\n", cmd);
    stream
        .write_all(payload.as_bytes())
        .and_then(|_| stream.flush())
        .map_err(|e| format!("tcp_write: {e}"))?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("tcp_read: {e}"))?;
    if line.trim().is_empty() {
        return Err("tcp_eof: empty response from live engine".to_string());
    }
    serde_json::from_str(line.trim()).map_err(|e| format!("tcp_bad_json: {e}"))
}

/// Точка входа классического stdio-сервера.
pub fn run_edit_server() -> i32 {
    run_edit_server_with_listen(None)
}

/// Точка входа stdio-сервера с опциональным локальным TCP-шлюзом `--listen 127.0.0.1:0` для ИИ-агентов.
pub fn run_edit_server_with_listen(listen_addr: Option<&str>) -> i32 {
    let mut session = CoreSession::new();
    let cancel_fast = Arc::clone(&session.cancel);
    let sessions_fast = Arc::clone(&session.sessions);

    // Фаза 6.D «AI Chat»: хаб писателей агентов — общая шина broadcast
    // (GUI → все агенты) для живого канала «Человек ↔ ИИ».
    let hub = AgentHub::new();
    session.agent_hub = Some(Arc::clone(&hub));

    // Фаза 7: UDS Event Plane для живого диффа и перехвата намерений шелла (Causal Nexus)
    let event_plane = Arc::new(crate::event_plane::EventPlane::new(crate::event_plane::DEFAULT_SOCKET_PATH));
    if let Err(e) = Arc::clone(&event_plane).start() {
        eprintln!("[poler-edit-core] UDS Event Plane note: {e}");
    }
    session.event_plane = Some(Arc::clone(&event_plane));

    let (tx_out, rx_out) = mpsc::channel::<String>();
    let (tx_req, rx_req) = mpsc::channel::<QueuedRequest>();

    // Писатель: единственный владелец stdout, flush после каждой строки.
    let writer = thread::spawn(move || -> io::Result<()> {
        let stdout = io::stdout();
        let mut lock = stdout.lock();
        for line in rx_out {
            let _ = writeln!(lock, "{line}");
            let _ = lock.flush();
        }
        Ok(())
    });

    // Опциональный локальный TCP-мост для живой совместной работы ИИ-агентов и GUI.
    if let Some(bind_spec) = listen_addr {
        if let Ok(listener) = TcpListener::bind(bind_spec) {
            if let Ok(local_addr) = listener.local_addr() {
                let token = generate_session_token();
                let _ = tx_out.send(
                    json!({
                        "ev": "listen_ready",
                        "addr": local_addr.to_string(),
                        "token": token,
                    })
                    .to_string(),
                );
                let tx_req_tcp = tx_req.clone();
                thread::spawn(move || {
                    for incoming in listener.incoming().flatten() {
                        // Принимаем ТОЛЬКО loopback-соединения.
                        if let Ok(peer) = incoming.peer_addr() {
                            if !peer.ip().is_loopback() {
                                continue;
                            }
                        }
                        let tx_job = tx_req_tcp.clone();
                        let expected_tok = token.clone();
                        let hub_tcp = Arc::clone(&hub);
                        thread::spawn(move || {
                            handle_tcp_agent(incoming, tx_job, &expected_tok, hub_tcp)
                        });
                    }
                });
            }
        }
    }

    // Воркер: владеет всеми документами, исполняет операции строго последовательно.
    let tx_out_w = tx_out.clone();
    let worker = thread::spawn(move || {
        while let Ok(q) = rx_req.recv() {
            match session.execute(&q.req, q.from_agent, Some(&tx_out_w)) {
                Some(resp) => {
                    let s = resp.to_string();
                    if let Some(client_tx) = q.reply_tx {
                        let _ = client_tx.send(s);
                    } else {
                        let _ = tx_out_w.send(s);
                    }
                }
                None => break, // Quit от главного stdio-клиента
            }
        }
    });

    // Главный поток: stdin -> задания; cancel обслуживается мгновенно вне очереди!
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    while let Some(Ok(line)) = lines.next() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let _ = tx_out.send(
                    json!({"id": 0, "ok": false, "error": format!("bad json: {e}")}).to_string(),
                );
                continue;
            }
        };
        let id = v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
        let cmd = v.get("cmd").or_else(|| v.get("method")).and_then(|x| x.as_str()).unwrap_or("");
        if cmd == "cancel" {
            cancel_fast.store(true, Ordering::Relaxed);
            if let Ok(s) = sessions_fast.lock() {
                for tok in s.values() {
                    tok.store(true, Ordering::Relaxed);
                }
            }
            let _ = tx_out.send(json!({"id": id, "ok": true, "cancelled": true}).to_string());
            continue;
        }
        let is_quit = cmd == "quit";
        if tx_req
            .send(QueuedRequest {
                req: v,
                from_agent: false,
                reply_tx: None,
            })
            .is_err()
        {
            break;
        }
        if is_quit {
            break;
        }
    }

    let _ = tx_req.send(QueuedRequest {
        req: json!({"cmd": "quit"}),
        from_agent: false,
        reply_tx: None,
    });
    let _ = worker.join();
    drop(tx_out);
    let _ = writer.join();
    0
}

fn handle_tcp_agent(
    stream: TcpStream,
    tx_req: mpsc::Sender<QueuedRequest>,
    expected_token: &str,
    hub: Arc<AgentHub>,
) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    // Фаза 6.D: агент регистрируется в хабе СРАЗУ после аутентификации —
    // с этой секунды он получает chat_broadcast'ы человека.
    let mut agent_hub_id: Option<u64> = None;
    let reader = BufReader::new(stream);
    let mut authenticated = false;

    for line_res in reader.lines() {
        let Ok(line) = line_res else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let _ = writeln!(
                    writer,
                    "{}",
                    json!({"id": 0, "ok": false, "error": format!("bad json: {e}")})
                );
                let _ = writer.flush();
                continue;
            }
        };
        let id = v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
        let supplied = v.get("token").and_then(|x| x.as_str()).unwrap_or("");
        if !authenticated {
            if constant_time_eq(supplied.as_bytes(), expected_token.as_bytes()) {
                authenticated = true;
                // Фаза 6.D: регистрируем клон writer'а (тот же сокет агента);
                // reader уже владеет исходным потоком — его не трогаем.
                agent_hub_id = hub.register(&writer);
            } else {
                let _ = writeln!(
                    writer,
                    "{}",
                    json!({"id": id, "ok": false, "error": "unauthorized: invalid POLER_EDIT_TOKEN"})
                );
                let _ = writer.flush();
                break;
            }
        }
        let (reply_tx, reply_rx) = mpsc::channel::<String>();
        let is_quit = v.get("cmd").or_else(|| v.get("method")).and_then(|x| x.as_str()) == Some("quit");
        if tx_req
            .send(QueuedRequest {
                req: v,
                from_agent: true,
                reply_tx: Some(reply_tx),
            })
            .is_err()
        {
            break;
        }
        if let Ok(resp_line) = reply_rx.recv() {
            if writeln!(writer, "{resp_line}").is_err() || writer.flush().is_err() {
                break;
            }
        } else {
            break;
        }
        if is_quit {
            break;
        }
    }
    // Фаза 6.D: агент ушёл — снимаем с регистрации (broadcast больше
    // не пытается писать в мёртвый поток, delivered считает честно).
    if let Some(aid) = agent_hub_id {
        hub.unregister(aid);
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn doc_of(v: &Value) -> u64 {
    v.get("doc").and_then(|x| x.as_u64()).unwrap_or(0)
}

fn u64_of(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

fn req_root(v: &Value) -> PathBuf {
    v.get("root")
        .or_else(|| v.get("path"))
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn url_decode(s: &str) -> String {
    let mut res = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(val) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                res.push(val as char);
                i += 3;
                continue;
            }
        }
        res.push(bytes[i] as char);
        i += 1;
    }
    res
}

pub fn req_path(v: &Value) -> Option<PathBuf> {
    let raw = v.get("path").and_then(|x| x.as_str())?;
    if raw.starts_with("content://") || raw.contains("raw%3A") || raw.contains("primary%3A") {
        let decoded = url_decode(raw);
        if let Some(pos) = decoded.find("raw:") {
            let sub = &decoded[pos + 4..];
            if Path::new(sub).exists() {
                return Some(PathBuf::from(sub));
            }
        }
        if let Some(pos) = decoded.find("primary:") {
            let sub = format!("/storage/emulated/0/{}", &decoded[pos + 8..]);
            if Path::new(&sub).exists() {
                return Some(PathBuf::from(sub));
            }
        }
        if let Some(slash) = decoded.rfind('/') {
            let filename = &decoded[slash + 1..];
            let cand1 = format!("/storage/emulated/0/Download/{filename}");
            if Path::new(&cand1).exists() {
                return Some(PathBuf::from(cand1));
            }
            let cand2 = format!("/sdcard/Download/{filename}");
            if Path::new(&cand2).exists() {
                return Some(PathBuf::from(cand2));
            }
        }
    }
    Some(PathBuf::from(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cas_expected_rev_and_auto_save_and_outline_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let fp = dir.path().join("mod.rs");
        std::fs::write(
            &fp,
            "pub struct Node {\n    pub id: u64,\n}\n\npub fn step(n: &Node) -> u64 {\n    n.id + 1\n}\n",
        )
        .unwrap();

        let mut sess = CoreSession::new();
        let open_r = sess
            .execute(&json!({"id": 1, "cmd": "open", "path": fp}), false, None)
            .unwrap();
        assert_eq!(open_r["ok"], true);
        let doc = open_r["doc"].as_u64().unwrap();
        let rev1 = open_r["rev"].as_u64().unwrap();

        // CAS mismatch блокируется со статусом stale_revision:
        let stale = sess
            .execute(
                &json!({
                    "id": 2, "cmd": "replace_match", "doc": doc,
                    "old_text": "n.id + 1", "new_text": "n.id + 2",
                    "expected_rev": rev1 + 99
                }),
                true,
                None,
            )
            .unwrap();
        assert_eq!(stale["ok"], false);
        assert!(stale["error"].as_str().unwrap().starts_with("stale_revision:"));

        // Точный CAS + синтаксический барьер + auto_save:
        let ok_edit = sess
            .execute(
                &json!({
                    "id": 3, "cmd": "replace_match", "doc": doc,
                    "old_text": "n.id + 1", "new_text": "n.id + 42",
                    "expected_rev": rev1, "check_syntax": true, "auto_save": true
                }),
                true,
                None,
            )
            .unwrap();
        assert_eq!(ok_edit["ok"], true);
        assert_eq!(ok_edit["syntax_balanced"], true);
        assert!(std::fs::read_to_string(&fp).unwrap().contains("n.id + 42"));

        // Outline отдаёт и struct Node, и fn step:
        let out_r = sess
            .execute(&json!({"id": 4, "cmd": "outline", "doc": doc}), false, None)
            .unwrap();
        let items = out_r["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["name"], "Node");
        assert_eq!(items[1]["name"], "step");
    }

    #[test]
    fn vfs_cow_open_save_flush_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("book.epub");
        {
            use std::io::Write as _;
            let f = std::fs::File::create(&p).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts: zip::write::SimpleFileOptions = Default::default();
            w.start_file("OEBPS/chapter1.xhtml", opts).unwrap();
            w.write_all(b"<html><body><p>Hello</p></body></html>").unwrap();
            w.start_file("toc.ncx", opts).unwrap();
            w.write_all(b"<ncx/>").unwrap();
            w.finish().unwrap();
        }
        let disk_before = std::fs::read(&p).unwrap();

        let mut sess = CoreSession::new();

        // 1) vfs_open: запись становится документом.
        let r = sess
            .execute(
                &json!({"id": 1, "cmd": "vfs_open", "path": p, "entry": "OEBPS/chapter1.xhtml"}),
                false,
                None,
            )
            .unwrap();
        assert_eq!(r["ok"], true);
        assert_eq!(r["vfs"], true);
        assert_eq!(r["entry"], "OEBPS/chapter1.xhtml");
        let doc = r["doc"].as_u64().unwrap();
        let rev1 = r["rev"].as_u64().unwrap();

        // 2) Повторное открытие — тот же doc (reuse).
        let r2 = sess
            .execute(
                &json!({"id": 2, "cmd": "vfs_open", "path": p, "entry": "OEBPS/chapter1.xhtml"}),
                false,
                None,
            )
            .unwrap();
        assert_eq!(r2["reused"], true);
        assert_eq!(r2["doc"].as_u64().unwrap(), doc);

        // 3) Правка в буфере (обычный replace_match).
        let r3 = sess
            .execute(
                &json!({"id": 3, "cmd": "replace_match", "doc": doc,
                        "old_text": "Hello", "new_text": "Привіт, світе", "expected_rev": rev1}),
                false,
                None,
            )
            .unwrap();
        assert_eq!(r3["ok"], true);

        // 4) CoW: диск не тронут и после vfs_save.
        let r4 = sess
            .execute(&json!({"id": 4, "cmd": "vfs_save", "doc": doc}), false, None)
            .unwrap();
        assert_eq!(r4["ok"], true);
        assert_eq!(r4["cow"], true);
        assert_eq!(r4["dirty_entries"], 1);
        assert_eq!(disk_before, std::fs::read(&p).unwrap());

        // 5) vfs_read видит новую версию, vfs_list — статус modified.
        let r5 = sess
            .execute(
                &json!({"id": 5, "cmd": "vfs_read", "path": p,
                        "entry": "OEBPS/chapter1.xhtml", "text": true}),
                false,
                None,
            )
            .unwrap();
        assert_eq!(r5["ok"], true);
        assert!(r5["text"].as_str().unwrap().contains("Привіт, світе"));

        let r6 = sess
            .execute(&json!({"id": 6, "cmd": "vfs_list", "path": p}), false, None)
            .unwrap();
        let entries = r6["entries"].as_array().unwrap();
        let ch = entries.iter().find(|e| e["name"] == "OEBPS/chapter1.xhtml").unwrap();
        assert_eq!(ch["status"], "modified");

        // 6) vfs_delete + vfs_dirty.
        let r7 = sess
            .execute(
                &json!({"id": 7, "cmd": "vfs_delete", "path": p, "entry": "toc.ncx"}),
                false,
                None,
            )
            .unwrap();
        assert_eq!(r7["ok"], true);
        let r8 = sess
            .execute(&json!({"id": 8, "cmd": "vfs_dirty", "path": p}), false, None)
            .unwrap();
        assert_eq!(r8["count"], 2);

        // 7) Прямая запись vfs_write (агентский путь) — новая запись.
        let r9 = sess
            .execute(
                &json!({"id": 9, "cmd": "vfs_write", "path": p,
                        "entry": "META-INF/new.xml", "text": "<new/>"}),
                true,
                None,
            )
            .unwrap();
        assert_eq!(r9["ok"], true);

        // 8) flush: материализация на диск.
        let r10 = sess
            .execute(&json!({"id": 10, "cmd": "vfs_flush", "path": p}), false, None)
            .unwrap();
        assert_eq!(r10["ok"], true);
        assert_eq!(r10["kind"], "zip");
        assert_eq!(r10["entries"], 2); // chapter + new.xml (toc удалён)
        assert_ne!(disk_before, std::fs::read(&p).unwrap());

        // 9) Оверлей израсходован: новые чтения — с диска.
        let r11 = sess
            .execute(
                &json!({"id": 11, "cmd": "vfs_read", "path": p,
                        "entry": "OEBPS/chapter1.xhtml", "text": true}),
                false,
                None,
            )
            .unwrap();
        assert!(r11["text"].as_str().unwrap().contains("Привіт, світе"));
        let r12 = sess
            .execute(&json!({"id": 12, "cmd": "vfs_dirty", "path": p}), false, None)
            .unwrap();
        assert_eq!(r12["count"], 0);
        // toc.ncx действительно удалён из нового архива.
        let r13 = sess
            .execute(
                &json!({"id": 13, "cmd": "vfs_read", "path": p, "entry": "toc.ncx"}),
                false,
                None,
            )
            .unwrap();
        assert_eq!(r13["ok"], false);

        // 10) vfs_flush на чистом архиве — вежливая ошибка.
        let r14 = sess
            .execute(&json!({"id": 14, "cmd": "vfs_flush", "path": p}), false, None)
            .unwrap();
        assert_eq!(r14["ok"], false);
    }

    #[test]
    fn vfs_discard_restores_disk_view() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("book.epub");
        {
            use std::io::Write as _;
            let f = std::fs::File::create(&p).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts: zip::write::SimpleFileOptions = Default::default();
            w.start_file("toc.ncx", opts).unwrap();
            w.write_all(b"<ncx/>").unwrap();
            w.finish().unwrap();
        }
        let disk_before = std::fs::read(&p).unwrap();

        let mut sess = CoreSession::new();
        sess.execute(
            &json!({"id": 1, "cmd": "vfs_write", "path": p, "entry": "toc.ncx", "text": "<ncx v2/>"}),
            false,
            None,
        )
        .unwrap();
        // discard: оверлей сброшен, диск не изменился ни на байт.
        let r = sess
            .execute(&json!({"id": 2, "cmd": "vfs_discard", "path": p}), false, None)
            .unwrap();
        assert_eq!(r["discarded"], true);
        assert_eq!(disk_before, std::fs::read(&p).unwrap());
        let r2 = sess
            .execute(
                &json!({"id": 3, "cmd": "vfs_read", "path": p, "entry": "toc.ncx", "text": true}),
                false,
                None,
            )
            .unwrap();
        assert_eq!(r2["text"], "<ncx/>");
        // Повторный discard — discarded=false (нечего откатывать).
        let r3 = sess
            .execute(&json!({"id": 4, "cmd": "vfs_discard", "path": p}), false, None)
            .unwrap();
        assert_eq!(r3["discarded"], false);
    }
    // ------------------------- Фаза 4: Shannon Bypass v3.0 -------------------------

    #[test]
    fn vfs_flush_symbol_method_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("capsule.poler");
        // .poler с анизотропной записью (символьная фаза окупается).
        {
            let payload: Vec<u8> = (0..300_000u32)
                .map(|i| {
                    if (i / 500) % 3 == 0 { 0xEF } else { (i % 251) as u8 }
                })
                .collect();
            crate::vfs_edit::poler_write_files(
                &p,
                vec![("data/aniso.bin".to_string(), payload)],
            )
            .unwrap();
        }
        let disk_before = std::fs::read(&p).unwrap();

        let mut sess = CoreSession::new();
        // 1) Некорректный метод — вежливая ошибка.
        let r0 = sess
            .execute(&json!({"id": 1, "cmd": "vfs_flush", "path": p, "method": "brotli"}), false, None)
            .unwrap();
        assert_eq!(r0["ok"], false);
        assert!(r0["error"].as_str().unwrap().contains("method"));

        // 2) Некорректный формат — вежливая ошибка.
        let r0b = sess
            .execute(&json!({"id": 2, "cmd": "vfs_flush", "path": p, "format": "rar"}), false, None)
            .unwrap();
        assert_eq!(r0b["ok"], false);
        assert!(r0b["error"].as_str().unwrap().to_lowercase().contains("format"));

        // 3) Правка через оверлей + flush с method=symbol на месте.
        sess.execute(
            &json!({"id": 3, "cmd": "vfs_write", "path": p, "entry": "data/aniso.bin",
                    "text": "Нова анізотропна версія"}),
            false,
            None,
        )
        .unwrap();
        assert_eq!(disk_before, std::fs::read(&p).unwrap()); // CoW
        let r = sess
            .execute(&json!({"id": 4, "cmd": "vfs_flush", "path": p, "method": "symbol"}), false, None)
            .unwrap();
        assert_eq!(r["ok"], true);
        assert_eq!(r["kind"], "poler");
        assert_eq!(r["method"], "symbol");
        assert!(r["raw"].as_u64().unwrap() > 0);
        // ratio = bytes/raw: для крошечных архивов включает накладные расходы
        // контейнера (заголовки/трейлер), поэтому верхняя граница не Assert-ится.
        assert!(r["ratio"].as_f64().unwrap() > 0.0);
        assert_ne!(disk_before, std::fs::read(&p).unwrap());

        // 4) Чтение обратно — новая версия.
        let r2 = sess
            .execute(&json!({"id": 5, "cmd": "vfs_read", "path": p, "entry": "data/aniso.bin", "text": true}), false, None)
            .unwrap();
        assert_eq!(r2["text"], "Нова анізотропна версія");

        // 5) Пустой оверлей без format — вежливая ошибка (как в Фазе 3).
        let r3 = sess
            .execute(&json!({"id": 6, "cmd": "vfs_flush", "path": p}), false, None)
            .unwrap();
        assert_eq!(r3["ok"], false);
    }

    #[test]
    fn vfs_flush_remux_to_poler_symbol_protocol() {
        // Ремукс: пустой оверлей + format=poler + method=symbol — конверсия
        // смонтированного zip в .poler с дотисканием (действие GUI).
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("book.epub");
        {
            use std::io::Write as _;
            let f = std::fs::File::create(&src).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts: zip::write::SimpleFileOptions = Default::default();
            w.start_file("OEBPS/ch1.xhtml", opts).unwrap();
            w.write_all(b"<html><body><p>Hello</p></body></html>").unwrap();
            w.start_file("OEBPS/images/cover.jpg", opts).unwrap();
            w.write_all(&(0..5000u32).map(|i| (i % 251) as u8).collect::<Vec<u8>>()).unwrap();
            w.finish().unwrap();
        }
        let zip_before = std::fs::read(&src).unwrap();
        let dest = dir.path().join("book.poler");

        let mut sess = CoreSession::new();
        let r = sess
            .execute(&json!({"id": 1, "cmd": "vfs_flush", "path": src,
                    "to": dest, "method": "symbol", "format": "poler"}), false, None)
            .unwrap();
        assert_eq!(r["ok"], true);
        assert_eq!(r["kind"], "poler");
        assert_eq!(r["method"], "symbol");
        assert_eq!(r["entries"], 2);
        assert_eq!(zip_before, std::fs::read(&src).unwrap()); // оригинал нетронут

        // Записи читаются из нового .poler бит-в-бит.
        let r2 = sess
            .execute(&json!({"id": 2, "cmd": "vfs_read", "path": dest, "entry": "OEBPS/ch1.xhtml", "text": true}), false, None)
            .unwrap();
        assert_eq!(r2["text"], "<html><body><p>Hello</p></body></html>");
        let r3 = sess
            .execute(&json!({"id": 3, "cmd": "vfs_read", "path": dest, "entry": "OEBPS/images/cover.jpg"}), false, None)
            .unwrap();
        assert_eq!(r3["ok"], true);

        // Пустой оверлей + format=poler допустим (ремукс), но тот же вызов
        // без format на чистом zip — ошибка (совместимость Фазы 3).
        let r4 = sess
            .execute(&json!({"id": 4, "cmd": "vfs_flush", "path": src}), false, None)
            .unwrap();
        assert_eq!(r4["ok"], false);
    }

    #[test]
    fn symbol_bench_protocol() {
        let mut sess = CoreSession::new();
        let text = "Анізотропія: два символи покривають 55% потоку. ".repeat(200);
        let r = sess
            .execute(&json!({"id": 1, "cmd": "symbol_bench", "text": text}), false, None)
            .unwrap();
        assert_eq!(r["ok"], false);
        assert!(r["error"].as_str().unwrap().contains("poler-engine"));
    }

    // ------------------------- Фаза 5 «MCP-глаза на VFS» -------------------------

    #[test]
    fn scratch_ram_playground_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let mut sess = CoreSession::new();

        // 1) Полигон пуст.
        let r0 = sess
            .execute(&json!({"id": 1, "cmd": "scratch_list"}), false, None)
            .unwrap();
        assert_eq!(r0["ok"], true);
        assert_eq!(r0["count"], 0);
        assert_eq!(r0["stats"]["disk"], 0);

        // 2) Ошибки валидации имени — вежливые.
        let re1 = sess
            .execute(&json!({"id": 2, "cmd": "scratch_put", "uri": "", "text": "x"}), false, None)
            .unwrap();
        assert_eq!(re1["ok"], false);
        let re2 = sess
            .execute(&json!({"id": 3, "cmd": "scratch_put", "uri": "../escape.md", "text": "x"}), false, None)
            .unwrap();
        assert_eq!(re2["ok"], false);
        let re3 = sess
            .execute(&json!({"id": 4, "cmd": "scratch_put", "uri": "note.md"}), false, None)
            .unwrap();
        assert_eq!(re3["ok"], false, "нет text/data_base64");

        // 3) Заметка агента в RAM: ноль байт на диске.
        let plan = "# План атаки\n1. Прочитать vfs\n2. Собрать кристалл\n3. Применить патч\n".repeat(40);
        let r1 = sess
            .execute(&json!({"id": 5, "cmd": "scratch_put", "uri": "plan.md",
                    "text": plan, "provenance": "file:///project/README.md"}), false, None)
            .unwrap();
        assert_eq!(r1["ok"], true);
        assert_eq!(r1["ram"], true);
        assert_eq!(r1["disk"], 0);
        assert_eq!(r1["bytes"].as_u64().unwrap(), plan.len() as u64);
        assert!(r1["chunks"].as_u64().unwrap() >= 1);

        // 4) Чтение записи: scratch_get и маршрутизация vfs_read.
        let r2 = sess
            .execute(&json!({"id": 6, "cmd": "scratch_get", "uri": "plan.md", "text": true}), false, None)
            .unwrap();
        assert_eq!(r2["ok"], true);
        assert_eq!(r2["text"].as_str().unwrap(), plan);
        assert_eq!(r2["provenance"], "file:///project/README.md");
        let r2b = sess
            .execute(&json!({"id": 7, "cmd": "vfs_read", "path": "vfs://ai_scratch/plan.md", "text": true}), false, None)
            .unwrap();
        assert_eq!(r2b["ok"], true, "vfs_read маршрутизируется в полигон");
        assert_eq!(r2b["text"].as_str().unwrap(), plan);
        let r2c = sess
            .execute(&json!({"id": 8, "cmd": "vfs_read", "path": "vfs://ai_scratch", "entry": "plan.md", "text": true}), false, None)
            .unwrap();
        assert_eq!(r2c["ok"], true);
        assert_eq!(r2c["text"].as_str().unwrap(), plan);

        // 5) База-источник + черновик поверх неё: дедуп в RAM.
        let src_path = dir.path().join("main.rs");
        let source: Vec<u8> = (0..150_000u32).map(|i| (i % 251) as u8 ^ 0x5A).collect();
        std::fs::write(&src_path, &source).unwrap();
        let mut draft = source.clone();
        draft.extend_from_slice("\n// TODO: final patch from the playground\n".as_bytes());
        let r3 = sess
            .execute(&json!({"id": 9, "cmd": "scratch_put", "uri": "drafts/main.rs",
                    "data_base64": crate::vfs::base64_encode(&draft),
                    "provenance": "file:///main.rs",
                    "provenance_path": src_path.display().to_string()}), false, None)
            .unwrap();
        assert_eq!(r3["ok"], true);
        assert!(r3["base_registered"].as_u64().unwrap() > 0, "база зарегистрирована");
        let (sh, ch) = (
            r3["shared_chunks"].as_u64().unwrap(),
            r3["chunks"].as_u64().unwrap(),
        );
        assert!(sh as f64 / ch as f64 > 0.8, "черновик ~90% дублирует базу: {sh}/{ch}");

        // 5b) Второй черновик поверх той же базы: те же чанки — RAM не растёт.
        let unique_before = {
            let l = sess.execute(&json!({"id": 9, "cmd": "scratch_list"}), false, None).unwrap();
            l["stats"]["unique_chunks"].as_u64().unwrap()
        };
        let r3b = sess
            .execute(&json!({"id": 10, "cmd": "scratch_put", "uri": "drafts/main-v2.rs",
                    "data_base64": crate::vfs::base64_encode(&draft),
                    "provenance": "file:///main.rs"}), false, None)
            .unwrap();
        assert_eq!(r3b["ok"], true);
        assert_eq!(r3b["new_chunks"], 0, "второй черновик целиком из общих чанков");
        assert_eq!(r3b["physical_delta"], 0, "RAM не растёт");
        let unique_after = {
            let l = sess.execute(&json!({"id": 11, "cmd": "scratch_list"}), false, None).unwrap();
            l["stats"]["unique_chunks"].as_u64().unwrap()
        };
        assert_eq!(unique_before, unique_after, "уникальных чанков столько же");

        // 6) Листинги: scratch_list и vfs_list полигона.
        let r4 = sess
            .execute(&json!({"id": 10, "cmd": "scratch_list"}), false, None)
            .unwrap();
        assert_eq!(r4["count"], 3);
        assert_eq!(r4["stats"]["disk"], 0);
        assert!(r4["stats"]["dedup_ratio"].as_f64().unwrap() > 1.0,
            "логический объём больше физического (дедуп работает)");
        let r4b = sess
            .execute(&json!({"id": 11, "cmd": "vfs_list", "path": "vfs://ai_scratch"}), false, None)
            .unwrap();
        assert_eq!(r4b["ok"], true);
        assert_eq!(r4b["kind"], "ai_scratch");
        assert_eq!(r4b["entries"].as_array().unwrap().len(), 3);
        let r4c = sess
            .execute(&json!({"id": 12, "cmd": "scratch_list", "pattern": "plan"}), false, None)
            .unwrap();
        assert_eq!(r4c["count"], 1);

        // 7) Материализация в файл — атомарно.
        let out_file = dir.path().join("materialized_plan.md");
        let r5 = sess
            .execute(&json!({"id": 13, "cmd": "scratch_apply", "uri": "plan.md",
                    "target": out_file.display().to_string()}), false, None)
            .unwrap();
        assert_eq!(r5["ok"], true);
        assert_eq!(r5["materialized"], true);
        assert_eq!(r5["atomic"], true);
        assert_eq!(std::fs::read(&out_file).unwrap(), plan.as_bytes());

        // 8) Материализация в архив — CoW + vfs_flush.
        let arch = dir.path().join("proj.poler");
        crate::vfs_edit::poler_write_files(
            &arch,
            vec![("src/main.rs".to_string(), source.clone())],
        )
        .unwrap();
        let disk_before = std::fs::read(&arch).unwrap();
        let r6 = sess
            .execute(&json!({"id": 14, "cmd": "scratch_apply", "uri": "drafts/main.rs",
                    "target": arch.display().to_string(), "entry": "src/main.rs"}), false, None)
            .unwrap();
        assert_eq!(r6["ok"], true);
        assert_eq!(r6["cow"], true);
        assert_eq!(r6["materialized"], false, "сначала только оверлей");
        assert_eq!(disk_before, std::fs::read(&arch).unwrap(), "CoW: диск не тронут");
        let r7 = sess
            .execute(&json!({"id": 15, "cmd": "vfs_read", "path": arch.display().to_string(),
                    "entry": "src/main.rs", "text": false}), false, None)
            .unwrap();
        assert_eq!(r7["ok"], true);
        assert_eq!(crate::vfs::base64_decode(r7["data_base64"].as_str().unwrap()).unwrap(), draft,
            "чтение через оверлей даёт черновик");
        let r8 = sess
            .execute(&json!({"id": 16, "cmd": "vfs_flush", "path": arch.display().to_string()}), false, None)
            .unwrap();
        assert_eq!(r8["ok"], true);
        assert_ne!(disk_before, std::fs::read(&arch).unwrap(), "flush материализовал");

        // 9) Откат: одна запись и весь полигон.
        let r9 = sess
            .execute(&json!({"id": 17, "cmd": "scratch_discard", "uri": "plan.md"}), false, None)
            .unwrap();
        assert_eq!(r9["ok"], true);
        assert_eq!(r9["discarded"], true);
        let r9b = sess
            .execute(&json!({"id": 18, "cmd": "scratch_get", "uri": "plan.md", "text": true}), false, None)
            .unwrap();
        assert_eq!(r9b["ok"], false);
        let r9c = sess
            .execute(&json!({"id": 19, "cmd": "scratch_discard", "all": true}), false, None)
            .unwrap();
        assert_eq!(r9c["ok"], true);
        assert_eq!(r9c["discarded"], 2);
        let r9d = sess
            .execute(&json!({"id": 20, "cmd": "scratch_list"}), false, None)
            .unwrap();
        assert_eq!(r9d["count"], 0);
        assert_eq!(r9d["stats"]["physical_bytes"], 0, "полигон пуст — RAM возвращена");
    }

    #[test]
    fn crystal_memory_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let mut sess = CoreSession::new();

        // 1) Паспорт: лениво монтируется зашитый crystal_ru.t5c движка.
        let r0 = sess
            .execute(&json!({"id": 1, "cmd": "crystal_stats"}), false, None)
            .unwrap();
        assert_eq!(r0["ok"], true);
        assert_eq!(r0["format"], "t5c/v1");
        assert_eq!(r0["magic"], "T5CRYS");
        assert!(r0["vocab"].as_u64().unwrap() >= 32, "зашитый кристалл жив");
        assert!(r0["bigram_nonzeros"].as_u64().unwrap() > 0);
        assert_eq!(r0["trits_per_byte"], 5);
        assert_eq!(r0["source"], "embedded:crystal_ru.t5c");

        // 2) Прошивка заметки: новые слова + Хеббовские сдвиги.
        let note = "Квантигон резонансує з автотрофом. Автотроф жде квантигон в резонансі. \
                    Полігон пам\'ятає зв\'язок квантигон автотроф.";
        let r1 = sess
            .execute(&json!({"id": 2, "cmd": "crystal_note", "text": note}), false, None)
            .unwrap();
        assert_eq!(r1["ok"], true);
        assert!(r1["words"].as_u64().unwrap() >= 8);
        assert!(r1["new_words"].as_u64().unwrap() >= 1, "квантигон — новое слово");
        assert!(r1["pairs_bumped"].as_u64().unwrap() >= 1);
        let vocab_before = r1["vocab"].as_u64().unwrap();

        // 3) Резонансный запрос: ассоциаты нового слова.
        let r2 = sess
            .execute(&json!({"id": 3, "cmd": "crystal_assoc", "word": "квантигон", "sem_k": 4, "bi_k": 8}), false, None)
            .unwrap();
        assert_eq!(r2["ok"], true);
        assert_eq!(r2["word"], "квантигон");
        let assoc = r2["assoc"].as_array().unwrap();
        assert!(assoc.contains(&json!("автотроф")),
            "партнёр по совместной встречаемости найден: {:?}", assoc);
        assert!(r2["semantic"].as_array().unwrap().len() <= 4);

        // 4) Неизвестное слово — вежливая ошибка с подсказкой.
        let r3 = sess
            .execute(&json!({"id": 4, "cmd": "crystal_assoc", "word": "ъъъъъ"}), false, None)
            .unwrap();
        assert_eq!(r3["ok"], false);
        assert!(r3["error"].as_str().unwrap().contains("crystal"));
        assert!(r3["hint"].as_str().unwrap().contains("crystal_note"));

        // 5) Пустая заметка — вежливая ошибка.
        let r3b = sess
            .execute(&json!({"id": 5, "cmd": "crystal_note", "text": "   "}), false, None)
            .unwrap();
        assert_eq!(r3b["ok"], false);

        // 6) Сохранение .t5c: магия формата + детерминизм тела.
        let t5c = dir.path().join("memory.t5c");
        let r4 = sess
            .execute(&json!({"id": 6, "cmd": "crystal_save", "path": t5c.display().to_string()}), false, None)
            .unwrap();
        assert_eq!(r4["ok"], true);
        assert!(r4["bytes"].as_u64().unwrap() > 0x50);
        let bytes = std::fs::read(&t5c).unwrap();
        assert_eq!(&bytes[0..8], b"T5CRYS\0\0", "магия формата на месте");
        assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 1, "version = 1");
        let vocab_saved = u32::from_le_bytes(bytes[0x0C..0x10].try_into().unwrap());
        assert_eq!(vocab_saved as u64, vocab_before, "расширенный словарь сохранён");

        // 7) Загрузка: кристалл сессии заменяется, память переживает рестарт.
        let mut sess2 = CoreSession::new();
        let r5 = sess2
            .execute(&json!({"id": 7, "cmd": "crystal_load", "path": t5c.display().to_string()}), false, None)
            .unwrap();
        assert_eq!(r5["ok"], true);
        assert_eq!(r5["stats"]["vocab"].as_u64().unwrap(), vocab_before);
        let r6 = sess2
            .execute(&json!({"id": 8, "cmd": "crystal_assoc", "word": "квантигон"}), false, None)
            .unwrap();
        assert_eq!(r6["ok"], true, "память пережила перезапуск сессии");
        assert!(r6["assoc"].as_array().unwrap().contains(&json!("автотроф")));
        // Повреждённый файл отвергается SHA-256.
        let mut corrupt = bytes.clone();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0x01;
        let corrupt_path = dir.path().join("corrupt.t5c");
        std::fs::write(&corrupt_path, &corrupt).unwrap();
        let r7 = sess2
            .execute(&json!({"id": 9, "cmd": "crystal_load", "path": corrupt_path.display().to_string()}), false, None)
            .unwrap();
        assert_eq!(r7["ok"], false, "порча тела ловится SHA-256");
    }

    /// Phase 5++.A (глава 4 плана v2.0): engine_daemon_start/stop —
    /// управление демоном из панели; события engine_daemon up/down летят
    /// в канал GUI; engine_status отдаёт расширенный блок (port, rss_bytes,
    /// tools_count, requests, last_latency_ms, log_tail).
    #[test]
    fn engine_daemon_start_stop_events_and_status() {
        if crate::engine_gate::find_engine_bin().is_none() {
            eprintln!("SKIP: poler-engine не найден (PATH/POLER_ENGINE_BIN)");
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let mut sess = CoreSession::new();

        // статус до старта: демон спит
        let r0 = sess
            .execute(&json!({"id": 1, "cmd": "engine_status"}), false, None)
            .unwrap();
        assert_eq!(r0["ok"], true);
        assert_eq!(r0["daemon"]["running"], false);

        // старт из панели: событие up + полный блок
        let r1 = sess
            .execute(&json!({"id": 2, "cmd": "engine_daemon_start"}), false, Some(&tx))
            .unwrap();
        assert_eq!(r1["ok"], true, "{r1}");
        assert_eq!(r1["started"], true);
        let d = &r1["daemon"];
        assert_eq!(d["running"], true);
        for k in ["addr", "port", "pid", "uptime_secs", "daemon_version",
                  "rss_bytes", "tools_count", "requests", "last_latency_ms",
                  "last_error", "log_tail"] {
            assert!(d.get(k).is_some(), "нет поля {k}: {d}");
        }
        assert!(d["tools_count"].as_u64().unwrap() >= 30, "каталог MCP: {d}");
        let pid = d["pid"].as_u64().unwrap();
        assert!(pid > 0);
        let ev_up: Value =
            serde_json::from_str(&rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap())
                .unwrap();
        assert_eq!(ev_up["ev"], "engine_daemon");
        assert_eq!(ev_up["state"], "up");
        assert_eq!(ev_up["pid"].as_u64().unwrap(), pid);

        // engine_status теперь видит живого демона (расширенные поля)
        let r2 = sess
            .execute(&json!({"id": 3, "cmd": "engine_status"}), false, None)
            .unwrap();
        assert_eq!(r2["daemon"]["running"], true);
        assert!(r2["daemon"]["rss_bytes"].as_u64().unwrap() > 1024 * 1024, "RSS живого: {}", r2["daemon"]);
        assert!(r2["daemon"]["port"].as_u64().unwrap() > 0);

        // повторный старт — идемпотентен (тот же демон)
        let r3 = sess
            .execute(&json!({"id": 4, "cmd": "engine_daemon_start"}), false, Some(&tx))
            .unwrap();
        assert_eq!(r3["ok"], true);
        assert_eq!(r3["started"], true);
        assert_eq!(r3["daemon"]["pid"].as_u64().unwrap(), pid);

        // stop: событие down, демон мёртв, статус это отражает
        let r4 = sess
            .execute(&json!({"id": 5, "cmd": "engine_daemon_stop"}), false, Some(&tx))
            .unwrap();
        assert_eq!(r4["ok"], true, "{r4}");
        assert_eq!(r4["stopped"], true);
        let ev_down: Value =
            serde_json::from_str(&rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap())
                .unwrap();
        assert_eq!(ev_down["ev"], "engine_daemon");
        assert_eq!(ev_down["state"], "down");
        let r5 = sess
            .execute(&json!({"id": 6, "cmd": "engine_status"}), false, None)
            .unwrap();
        assert_eq!(r5["daemon"]["running"], false, "после stop: {r5}");

        // процесс действительно мёртв
        let alive = std::path::Path::new(&format!("/proc/{pid}")).exists();
        assert!(!alive, "демон {pid} обязан быть мёртв после stop");

        // повторный stop — не ошибка
        let r6 = sess
            .execute(&json!({"id": 7, "cmd": "engine_daemon_stop"}), false, None)
            .unwrap();
        assert_eq!(r6["ok"], true);
        assert_eq!(r6["stopped"], false);
    }

    // ---------------- Фаза 6.D «AI Chat» -------------------------------

    /// Агент шлёт chat_send → GUI-канал получает событие agent_chat;
    /// GUI-штат chat_send запрещён (роль дисциплинируется транспортом).
    #[test]
    fn chat_send_from_agent_emits_event_and_rejects_gui() {
        let mut sess = CoreSession::new();
        let (tx, rx) = mpsc::channel::<String>();

        // агент (from_agent=true) пишет человеку:
        let r = sess
            .execute(&json!({"id": 1, "cmd": "chat_send", "text": "готово: функция собрана"}), true, Some(&tx))
            .unwrap();
        assert_eq!(r["ok"], true);

        let ev: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(2)).unwrap()).unwrap();
        assert_eq!(ev["ev"], "agent_chat");
        assert_eq!(ev["role"], "agent");
        assert_eq!(ev["text"], "готово: функция собрана");

        // GUI не имеет права зваться агентом:
        let bad = sess
            .execute(&json!({"id": 2, "cmd": "chat_send", "text": "подделка"}), false, Some(&tx))
            .unwrap();
        assert_eq!(bad["ok"], false);
    }

    /// chat_broadcast без listen-сокета честно отказывает;
    /// с реальным TCP-агентом в хабе — доставляет и считает delivered.
    #[test]
    fn chat_broadcast_delivery_and_role_guard() {
        let mut sess = CoreSession::new();
        let (tx, _rx) = mpsc::channel::<String>();

        // listen не поднят:
        let none = sess
            .execute(&json!({"id": 1, "cmd": "chat_broadcast", "text": "привет"}), false, Some(&tx))
            .unwrap();
        assert_eq!(none["ok"], false);

        // агент не может транслировать человеческие сообщения:
        let hub = AgentHub::new();
        sess.agent_hub = Some(Arc::clone(&hub));
        let not_agent = sess
            .execute(&json!({"id": 2, "cmd": "chat_broadcast", "text": "привет"}), true, Some(&tx))
            .unwrap();
        assert_eq!(not_agent["ok"], false);

        // живой агент: слушаем, регистрируем клон потока в хабе, broadcasтим
        use std::io::BufRead;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let agent_thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            line
        });
        // даём accept'у подняться
        std::thread::sleep(Duration::from_millis(150));
        let client = TcpStream::connect(addr).unwrap();
        let agent_id = hub.register(&client).unwrap();
        assert_eq!(hub.count(), 1);

        let delivered = sess
            .execute(&json!({"id": 3, "cmd": "chat_broadcast", "text": "сделай рефакторинг"}), false, Some(&tx))
            .unwrap();
        assert_eq!(delivered["ok"], true);
        assert_eq!(delivered["delivered"], 1);

        let line = agent_thread.join().unwrap();
        let ev: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(ev["ev"], "agent_chat");
        assert_eq!(ev["role"], "human");
        assert_eq!(ev["text"], "сделай рефакторинг");

        // chat_status видит присутствие:
        let st = sess
            .execute(&json!({"id": 4, "cmd": "chat_status"}), false, None)
            .unwrap();
        assert_eq!(st["ok"], true);
        assert_eq!(st["agents"], 1);

        // отключение агента снимает его с регистрации:
        drop(client);
        hub.unregister(agent_id);
        let st2 = sess
            .execute(&json!({"id": 5, "cmd": "chat_status"}), false, None)
            .unwrap();
        assert_eq!(st2["agents"], 0);
        let after = sess
            .execute(&json!({"id": 6, "cmd": "chat_broadcast", "text": "нет никого"}), false, Some(&tx))
            .unwrap();
        assert_eq!(after["ok"], true);
        assert_eq!(after["delivered"], 0);
    }
}
