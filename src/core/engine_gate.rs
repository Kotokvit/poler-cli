//! poler-edit :: Фаза 5+ «Живой движок» — пол демона poler-engine под ядром.
//!
//! Редактор становится **тонким терминалом**: тяжёлые операции, которым не
//! место внутри процесса редактора, исполняет живой poler-engine v0.93+
//! (AI-Native Topographical Search Engine). Два режима:
//!
//! 1. **Сетевой MCP-гейт (демон):** `poler-engine --mcp-http 127.0.0.1:<port>
//!    --mcp-token <32-hex>` порождается лениво при первом `engine_*`-запросе,
//!    живёт в `CoreSession`, убивается в `Drop` (+ `PR_SET_PDEATHSIG` на Linux —
//!    демон не переживает родителя даже при аварийном падении ядра редактора).
//!    Транспорт — рукописный HTTP/1.1 поверх `std::net::TcpStream`: движок
//!    отвечает всегда с `Content-Length` (никогда chunked), авторизация —
//!    `Authorization: Bearer`. **Ноль новых зависимостей** — симметрия с
//!    `poler_engine::mcp_http`.
//! 2. **One-shot CLI:** пакетные операции без демона — упаковка каталога в
//!    `.poler` (`--pack`), дотискание контейнеров TAR/GZ/ZST на лету
//!    (`--stream-file`, «закон семантики»), листинг любого архива
//!    (`--archive-list --archive-json`), AIDDE impact-радар (`--impact`),
//!    кристаллизация корпусов в `.t5c` (`--crystal-ingest-dir`).
//!
//! Поиск бинаря движка: `POLER_ENGINE_BIN` → `PATH` → `~/.local/bin` →
//! вендоренная копия `poler-engine/target/{release,debug}/poler-engine`
//! (по предкам каталога бинарника ядра — работает из любого cwd).
//!
//! **Политика путей демона:** движок держит path-jail (cwd + веб-кэш +
//! `POLER_MCP_EXTRA_ROOTS`). Гейт снимает его через `POLER_MCP_ALLOW_ANY_PATH=1`:
//! демон слушает только 127.0.0.1 со случайным 128-битным токеном, виден
//! лишь самому ядру; суверенный редактор уже имеет полный доступ к ФС
//! пользователя, а движок здесь — локальный IPC-сателлит, а не внешний
//! сервис. (Для сетевых сценариев `--mcp-http` движка настраивается
//! отдельно, руками, с tunnel+токеном.)
//!
//! Живая верификация форматов (см. `scripts/smoke_phase6.py`):
//! boilerplate-проект 598 590 Б → `.poler` 3 890 Б (154:1, закон масштаба);
//! `corpus.tar.gz` → `.poler` c `tar_mode: true` — движок РАЗВЕРНУЛ контейнер
//! на лету и лёг пофайловой таблицей (закон семантики).

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Сколько ждём /health после spawn демона.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(15);
/// Таймаут одного HTTP-запроса к демону (поиск по гигантскому корпусу — не мгновение).
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(600);

// ─────────────────────────────────────────────────────────────────
// Поиск и опрос бинаря движка
// ─────────────────────────────────────────────────────────────────

/// Фаза 6.E «Монолит»: poler-engine зашит в бинарь ядра (build.rs +
/// include_bytes). При первом обращении материализуется во внешний кэш
/// ~/.cache/poler-edit/engine-<blake3> (атомарная запись tmp+rename, 0755)
/// и исполняется как обычный бинарь — флешка без установки движка работает.
#[cfg(feature = "monolithic")]
mod embedded_engine {
    use std::io::Write as _;
    use std::path::PathBuf;

    /// Зашитые байты движка (OUT_DIR/poler-engine-embedded, см. build.rs).
    pub const DATA: &[u8] =
        include_bytes!(concat!(env!("OUT_DIR"), "/poler-engine-embedded"));

    /// Имя в кэше — по blake3-хешу байтов (обновление движка = новое имя,
    /// старые копии не конфликтуют; длина — фолбэк для быстрых проверок).
    fn cache_name() -> String {
        let h = blake3::hash(DATA);
        format!("engine-{}", &h.to_hex().as_str()[..16])
    }

    /// Материализовать движок в ~/.cache/poler-edit и вернуть путь.
    pub fn materialize() -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        let dir = PathBuf::from(home).join(".cache").join("poler-edit");
        std::fs::create_dir_all(&dir).ok()?;
        let dst = dir.join(cache_name());
        // быстрый путь: уже лежит и совпадает длиной
        if dst.is_file() {
            if let Ok(meta) = std::fs::metadata(&dst) {
                if meta.len() as usize == DATA.len() {
                    return Some(dst);
                }
            }
        }
        let tmp = dir.join(format!("{}.tmp{}", cache_name(), std::process::id()));
        {
            let mut f = std::fs::File::create(&tmp).ok()?;
            f.write_all(DATA).ok()?;
            f.sync_all().ok()?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).ok()?;
        }
        std::fs::rename(&tmp, &dst).ok()?;
        Some(dst)
    }
}

/// Найти бинарь poler-engine: `POLER_ENGINE_BIN` → PATH → `~/.local/bin` →
/// вендоренная копия `poler-engine/target/{release,debug}` рядом с ядром →
/// (монолит) зашитая в бинарь копия.
pub fn find_engine_bin() -> Option<PathBuf> {
    // 0) Явное указание окружением (для тестов и нестандартных установок).
    if let Ok(p) = std::env::var("POLER_ENGINE_BIN") {
        let p = p.trim();
        if !p.is_empty() && Path::new(p).is_file() {
            return Some(PathBuf::from(p));
        }
    }
    // 1) Фаза 6.E «Монолит»: если бинарь собран с зашитым движком — используем
    //    именно его (материализуем во внешний кэш ~/.cache/poler-edit). Монолит
    //    самодостаточен и гарантирует согласованную версию движка.
    #[cfg(feature = "monolithic")]
    {
        if let Some(p) = embedded_engine::materialize() {
            return Some(p);
        }
    }
    // 2) PATH (семантика which; PATH обычно содержит ~/.local/bin).
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("poler-engine");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    // 3) ~/.local/bin (типовая установка «движок как инструмент»).
    if let Some(home) = std::env::var_os("HOME") {
        let cand = PathBuf::from(&home).join(".local").join("bin").join("poler-engine");
        if cand.is_file() {
            return Some(cand);
        }
    }
    // 4) Вендоренная копия в монорепо редактора: core/../poler-engine/target/...
    //    Ищем по предкам cwd И по предкам каталога бинарника (устойчиво к
    //    любому cwd и к установленному в ~/.local/bin ядру).
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        bases.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            bases.push(dir.to_path_buf());
        }
    }
    for base in &bases {
        for anc in base.ancestors().take(6) {
            for profile in ["release", "debug"] {
                let cand = anc.join("poler-engine").join("target").join(profile).join("poler-engine");
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    None
}

/// Версия движка (`poler-engine --version`, первая строка).
pub fn engine_version(bin: &Path) -> Result<String, String> {
    let out = Command::new(bin)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("spawn {}: {e}", bin.display()))?;
    if !out.status.success() {
        return Err(format!("--version: exit={}", out.status.code().unwrap_or(-1)));
    }
    let s = String::from_utf8_lossy(&out.stdout);
    Ok(s.lines().next().unwrap_or("").trim().to_string())
}

// ─────────────────────────────────────────────────────────────────
// Минимальный HTTP/1.1-клиент поверх std::net (без зависимостей)
// ─────────────────────────────────────────────────────────────────

/// Собрать сырой POST-запрос /mcp с Bearer-токеном.
fn build_post_mcp(token: &str, body: &str) -> String {
    format!(
        "POST /mcp HTTP/1.1\r\nHost: poler-edit-core\r\nAuthorization: Bearer {}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        token,
        body.len(),
        body
    )
}

/// Собрать сырой GET /health.
fn build_get_health() -> &'static str {
    "GET /health HTTP/1.1\r\nHost: poler-edit-core\r\nConnection: close\r\n\r\n"
}

/// Прочитать HTTP-ответ из потока: заголовки → Content-Length → тело.
/// Фолбек без Content-Length — читаем до EOF (Connection: close).
fn read_http_response(stream: &mut TcpStream) -> Result<(u16, String), String> {
    let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = memchr::memmem::find(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("http: EOF до конца заголовков".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| "http: нет статус-кода".to_string())?;
    let content_length = head.to_ascii_lowercase().lines().find_map(|l| {
        let mut it = l.splitn(2, ':');
        let k = it.next()?.trim();
        let v = it.next()?.trim();
        if k == "content-length" {
            v.parse::<usize>().ok()
        } else {
            None
        }
    });
    let body = match content_length {
        Some(len) => {
            while buf.len() < header_end + len {
                let n = stream.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            if buf.len() < header_end + len {
                return Err("http: обрезанное тело (меньше Content-Length)".into());
            }
            buf[header_end..header_end + len].to_vec()
        }
        None => {
            loop {
                let n = match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&chunk[..n]);
            }
            buf[header_end..].to_vec()
        }
    };
    Ok((status, String::from_utf8_lossy(&body).to_string()))
}

/// Один HTTP-раундтрип: записать запрос, прочитать ответ.
fn http_roundtrip(addr: &str, request: &str) -> Result<(u16, String), String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(HTTP_READ_TIMEOUT))
        .map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(Duration::from_secs(30))).ok();
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    read_http_response(&mut stream)
}

// ─────────────────────────────────────────────────────────────────
// Демон: spawn → health → JSON-RPC → Drop(kill)
// ─────────────────────────────────────────────────────────────────

/// Свободный порт на loopback (bind :0, запомнить, отпустить).
fn free_port() -> Result<u16, String> {
    let l = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind:0: {e}"))?;
    let port = l
        .local_addr()
        .map_err(|e| e.to_string())?
        .port();
    drop(l);
    Ok(port)
}

/// Случайный 32-hex токен (128 бит): движок fail-closed подменяет токены
/// короче 16 символов, поэтому генерируем заведомо длинный.
fn generate_token() -> String {
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x504F_4C45_5220_5220);
    let pid = std::process::id() as u64;
    seed ^= pid.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut out = String::with_capacity(32);
    for _ in 0..32 {
        // xorshift64* — детерминированный ПСП для токена сессии.
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let nibble = (seed.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 60) as u8;
        out.push(char::from_digit(nibble as u32, 16).unwrap_or('0'));
    }
    out
}

/// Живой демон poler-engine (MCP over HTTP на loopback).
///
/// Phase 5++.A: демон НАБЛЮДАЕМ — счётчики запросов/латентности гейта,
/// последняя ошибка tool()-вызова, размер каталога MCP (кэш tools/list
/// при спавне), RSS по /proc и живой хвост stderr (кольцевой буфер
/// 4 КиБ, `Stdio::piped` вместо null) — единый источник правды для
/// `engine_status` и панели DaemonPanel.
pub struct EngineDaemon {
    child: std::process::Child,
    pub bin: PathBuf,
    pub version: String,
    pub port: u16,
    pub token: String,
    started: Instant,
    /// Запросов через гейт (JSON-RPC call).
    requests: AtomicU64,
    /// Латентность последнего запроса, мс.
    last_latency_ms: AtomicU64,
    /// Последняя ошибка tool()-вызова (диагностика без консоли).
    last_error: Mutex<Option<String>>,
    /// Каталог MCP демона (кэш ответа tools/list при спавне; 0 — неизвестно).
    tools_count: AtomicU64,
    /// Кольцевой буфер stderr демона (сырые байты, ≤ LOG_TAIL_BYTES).
    log_tail: Arc<Mutex<VecDeque<u8>>>,
}

/// Ёмкость кольцевого буфера stderr демона.
const LOG_TAIL_BYTES: usize = 4096;

impl EngineDaemon {
    /// Породить демон: свободный порт + 128-битный токен + ожидание /health.
    pub fn spawn(bin: PathBuf) -> Result<Self, String> {
        let version = engine_version(&bin)?;
        let port = free_port()?;
        let token = generate_token();
        let mut cmd = Command::new(&bin);
        cmd.args([
            "--mcp-http",
            &format!("127.0.0.1:{port}"),
            "--mcp-token",
            &token,
        ])
        .stdout(Stdio::null())
        // Phase 5++.A: stderr демона — в кольцевую врезку панели (не null):
        // живой журнал без консоли.
        .stderr(Stdio::piped());
        // Суверенный локальный IPC: демон — сателлит ядра редактора
        // (loopback + случайный токен), path-jail снимаем осознанно —
        // см. док-комментарий модуля.
        cmd.env("POLER_MCP_ALLOW_ANY_PATH", "1");
        #[cfg(unix)]
        unsafe {
            use std::os::unix::process::CommandExt;
            // Демон не переживает родителя даже при SIGKILL ядра.
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn {}: {e}", bin.display()))?;
        // Phase 5++.A: поток-читатель stderr → кольцевой буфер 4 КиБ.
        let log_tail: Arc<Mutex<VecDeque<u8>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(LOG_TAIL_BYTES)));
        if let Some(mut err) = child.stderr.take() {
            let sink = Arc::clone(&log_tail);
            std::thread::spawn(move || {
                let mut chunk = [0u8; 512];
                loop {
                    match err.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut buf = sink.lock().unwrap_or_else(|e| e.into_inner());
                            for &b in &chunk[..n] {
                                if buf.len() == LOG_TAIL_BYTES {
                                    buf.pop_front();
                                }
                                buf.push_back(b);
                            }
                        }
                    }
                }
            });
        }
        let mut daemon = Self {
            child,
            bin,
            version,
            port,
            token,
            started: Instant::now(),
            requests: AtomicU64::new(0),
            last_latency_ms: AtomicU64::new(0),
            last_error: Mutex::new(None),
            tools_count: AtomicU64::new(0),
            log_tail,
        };
        daemon.wait_health()?;
        // Phase 5++.A: кэш размера каталога MCP (tools/list) — панель
        // показывает tools_count сразу после спавна; ошибка не фатальна.
        if let Ok(res) = daemon.call("tools/list", json!({})) {
            if let Some(n) = res.get("tools").and_then(|t| t.as_array()).map(|a| a.len()) {
                daemon.tools_count.store(n as u64, Ordering::Relaxed);
            }
        }
        Ok(daemon)
    }

    fn wait_health(&mut self) -> Result<(), String> {
        let addr = self.addr();
        let deadline = Instant::now() + HEALTH_TIMEOUT;
        loop {
            // Демон мог упасть на старте — не ждём зря.
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(format!("демон умер на старте: exit={:?}", status.code()));
            }
            if let Ok((status, body)) = http_roundtrip(&addr, build_get_health()) {
                if status == 200 && body.trim() == "ok" {
                    return Ok(());
                }
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return Err(format!("демон не поднялся за {} с ({addr})", HEALTH_TIMEOUT.as_secs()));
            }
            std::thread::sleep(Duration::from_millis(120));
        }
    }

    pub fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// Phase 5++.A: RSS демона, байт (VmRSS × 1024 из /proc/<pid>/status;
    /// вне Linux или мёртвый процесс — fail-soft 0).
    pub fn rss_bytes(&self) -> u64 {
        let path = format!("/proc/{}/status", self.child.id());
        let Ok(s) = std::fs::read_to_string(path) else {
            return 0;
        };
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb: u64 = rest
                    .trim()
                    .trim_end_matches("kB")
                    .trim()
                    .parse()
                    .unwrap_or(0);
                return kb.saturating_mul(1024);
            }
        }
        0
    }

    /// Phase 5++.A: хвост stderr демона (строкой; сырые байты буфера).
    pub fn log_tail(&self) -> String {
        let mut buf = self.log_tail.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(buf.make_contiguous()).into_owned()
    }

    /// Phase 5++.A: демон ещё жив? (неблокирующий try_wait).
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Phase 5++.A: остановить демон (kill + wait; идемпотентно).
    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Phase 5++.A: полный статус-блок демона для engine_status
    /// (единый источник правды панели DaemonPanel).
    pub fn status_json(&self) -> Value {
        json!({
            "running": true,
            "addr": self.addr(),
            "port": self.port,
            "pid": self.pid(),
            "uptime_secs": self.uptime_secs(),
            "daemon_version": self.version,
            "rss_bytes": self.rss_bytes(),
            "tools_count": self.tools_count.load(Ordering::Relaxed),
            "requests": self.requests.load(Ordering::Relaxed),
            "last_latency_ms": self.last_latency_ms.load(Ordering::Relaxed),
            "last_error": self.last_error.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            "log_tail": self.log_tail(),
        })
    }

    /// JSON-RPC вызов метода демона. Возвращает поле `result`.
    /// Phase 5++.A: гейт считает запросы и латентность; ошибка
    /// запоминается в last_error (диагностика панели без консоли).
    pub fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let r = self.call_inner(method, params);
        self.last_latency_ms
            .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
        if let Err(e) = &r {
            *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.clone());
        }
        r
    }

    fn call_inner(&self, method: &str, params: Value) -> Result<Value, String> {
        let body =
            json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
        let req = build_post_mcp(&self.token, &body);
        let (status, resp) = http_roundtrip(&self.addr(), &req)?;
        if status != 200 {
            return Err(format!("http {status}: {resp}"));
        }
        let v: Value =
            serde_json::from_str(&resp).map_err(|e| format!("json: {e} (body: {resp:.120})"))?;
        if let Some(err) = v.get("error") {
            return Err(format!("rpc error: {err}"));
        }
        v.get("result").cloned().ok_or_else(|| "нет поля result".to_string())
    }

    /// Вызов MCP-инструмента демона. Текст ответа старается распарситься
    /// как JSON (grep c json:true); иначе возвращается строка в {"text": …}.
    /// `Err` — только транспортные/протокольные ошибки; is_error демона
    /// возвращается как Ok({"is_error": true, ...}) для различения.
    pub fn tool(&self, name: &str, args: Value) -> Result<Value, String> {
        let result = self.call("tools/call", json!({"name": name, "arguments": args}))?;
        let is_error = result
            .get("isError")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let text = result
            .get("content")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let trimmed = text.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            if let Ok(mut parsed) = serde_json::from_str::<Value>(trimmed) {
                if let Some(obj) = parsed.as_object_mut() {
                    obj.insert("is_error".into(), json!(is_error));
                }
                return Ok(parsed);
            }
        }
        Ok(json!({"text": text, "is_error": is_error}))
    }
}

impl Drop for EngineDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Разобрать заголовок текста poler_search: «…: 36 хитов (показано 2)».
/// Возвращает (всего_хитов, показано).
pub fn parse_search_hits(text: &str) -> Option<(u64, u64)> {
    let first = text.lines().next()?;
    // «…: N хитов» или «…: N хитов (показано M)»
    let hits = first.split(" хитов").next()?.rsplit([':', ' ']).find_map(|s| s.parse::<u64>().ok())?;
    let shown = first
        .split("(показано ")
        .nth(1)
        .and_then(|s| s.split(')').next())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(hits);
    Some((hits, shown))
}

// ─────────────────────────────────────────────────────────────────
// One-shot CLI-операции (без демона)
// ─────────────────────────────────────────────────────────────────

/// Разобрать stdout подпроцесса как JSON (движок печатает pretty-JSON
/// статистики; допускаем хвостовой мусор после последней скобки).
fn parse_stdout_json(stdout: &[u8]) -> Result<Value, String> {
    let s = String::from_utf8_lossy(stdout);
    let start = s
        .find('{')
        .ok_or_else(|| format!("в stdout нет JSON: {:.200}", s))?;
    let rest = &s[start..];
    if let Ok(v) = serde_json::from_str::<Value>(rest) {
        return Ok(v);
    }
    // Хвостовой мусор: откусываем до последней '}' и пробуем снова.
    if let Some(end) = rest.rfind('}') {
        if let Ok(v) = serde_json::from_str::<Value>(&rest[..=end]) {
            return Ok(v);
        }
    }
    Err(format!("stdout не распарсился как JSON: {:.200}", rest))
}

/// Прогнать one-shot команду движка, вернуть (stdout, stderr) при exit=0.
fn run_cli(bin: &Path, args: &[&str]) -> Result<(Vec<u8>, String), String> {
    let out = Command::new(bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn {}: {e}", bin.display()))?;
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        return Err(format!(
            "exit={} stderr={:.400}",
            out.status.code().unwrap_or(-1),
            stderr
        ));
    }
    Ok((out.stdout, stderr))
}

/// Упаковка каталога в `.poler` (`--pack`): FastCDC + BLAKE3-дедуп между
/// файлами + zstd, пофайловая таблица. Возвращает JSON-статистику движка.
pub fn pack_dir(
    bin: &Path,
    dir: &Path,
    out: &Path,
    gitignore: bool,
    jobs: u64,
) -> Result<Value, String> {
    let mut args: Vec<&str> = vec!["--pack"];
    let dir_s = dir.to_string_lossy().to_string();
    let out_s = out.to_string_lossy().to_string();
    args.push(&dir_s);
    args.push("--output-archive");
    args.push(&out_s);
    if gitignore {
        args.push("--pack-gitignore");
    }
    if jobs > 0 {
        args.push("--pack-jobs");
    }
    // --pack-jobs <N> добавляется отдельным push (число живёт в этом скоупе)
    let jobs_s: String;
    if jobs > 0 {
        jobs_s = jobs.to_string();
        args.push(&jobs_s);
    }
    let (stdout, _stderr) = run_cli(bin, &args)?;
    let mut stats = parse_stdout_json(&stdout)?;
    if let Some(obj) = stats.as_object_mut() {
        obj.insert("output".into(), json!(out_s));
        obj.insert("source_dir".into(), json!(dir_s));
    }
    Ok(stats)
}

/// Дотискание контейнера в `.poler` (`--stream-file`): TAR/tar.gz/tar.zst/
/// gz/zst РАЗВОРАЧИВАЮТСЯ НА ЛЕТУ — «закон семантики» (движок понимает
/// формат, файлы ложатся пофайловой таблицей `tar_mode: true`,
/// BLAKE3-дедуп между ними).
pub fn stream_file(bin: &Path, file: &Path, out: &Path) -> Result<Value, String> {
    let file_s = file.to_string_lossy().to_string();
    let out_s = out.to_string_lossy().to_string();
    let (stdout, _stderr) = run_cli(
        bin,
        &["--stream-file", &file_s, "--output-archive", &out_s],
    )?;
    let mut stats = parse_stdout_json(&stdout)?;
    if let Some(obj) = stats.as_object_mut() {
        obj.insert("output".into(), json!(out_s));
        obj.insert("source_file".into(), json!(file_s));
    }
    Ok(stats)
}

/// Листинг любого архива (`--archive-list --archive-json`): .poler, zip,
/// tar, tar.gz, epub и т.д. — без распаковки.
pub fn archive_list(bin: &Path, archive: &Path) -> Result<Value, String> {
    let arc_s = archive.to_string_lossy().to_string();
    let (stdout, _stderr) = run_cli(bin, &["--archive-list", &arc_s, "--archive-json"])?;
    parse_stdout_json(&stdout)
}

/// AIDDE impact-анализ символа (`--impact`): call graph, upstream/downstream
/// паспорт, уровень риска. JSON в stdout.
pub fn impact(bin: &Path, root: &Path, symbol: &str, depth: u64) -> Result<Value, String> {
    let root_s = root.to_string_lossy().to_string();
    let depth_s = depth.to_string();
    let mut args: Vec<&str> = vec![&root_s, "--impact", symbol];
    if depth > 0 {
        args.push("--impact-depth");
        args.push(&depth_s);
    }
    let (stdout, _stderr) = run_cli(bin, &args)?;
    parse_stdout_json(&stdout)
}

/// Потоковая ингестия каталога/`.poler`-архива в Кристалл `.t5c`
/// (`--crystal-ingest-dir`). Движок печатает человеческий лог — возвращаем
/// его вместе с путём и размером кристалла.
pub fn crystal_ingest(bin: &Path, input: &Path, out: &Path, vocab: u64) -> Result<Value, String> {
    let in_s = input.to_string_lossy().to_string();
    let out_s = out.to_string_lossy().to_string();
    let vocab_s = vocab.to_string();
    let mut args: Vec<&str> = vec!["--crystal-ingest-dir", &in_s, "--crystal-out", &out_s];
    if vocab > 0 {
        args.push("--crystal-vocab");
        args.push(&vocab_s);
    }
    let (stdout, _stderr) = run_cli(bin, &args)?;
    let log = String::from_utf8_lossy(&stdout).to_string();
    let bytes = std::fs::metadata(out)
        .map(|m| m.len())
        .unwrap_or(0);
    Ok(json!({
        "ok": bytes > 0,
        "input": in_s,
        "output": out_s,
        "bytes": bytes,
        "vocab": vocab,
        "log": log.trim(),
    }))
}

// ─────────────────────────────────────────────────────────────────
// Тесты
// ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_bin() -> Option<PathBuf> {
        find_engine_bin()
    }

    #[test]
    fn token_is_32_hex() {
        let t = generate_token();
        assert_eq!(t.len(), 32, "токен должен быть 32 hex-символа");
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()), "hex: {t}");
        let t2 = generate_token();
        assert_ne!(t, t2, "токены сессий различаются");
    }

    #[test]
    fn post_request_format() {
        let req = build_post_mcp("abcdef0123456789", "{\"x\":1}");
        assert!(req.starts_with("POST /mcp HTTP/1.1\r\n"));
        assert!(req.contains("Authorization: Bearer abcdef0123456789\r\n"));
        assert!(req.contains(&format!("Content-Length: {}\r\n", "{\"x\":1}".len())));
        assert!(req.ends_with("\r\n\r\n{\"x\":1}"));
    }

    #[test]
    fn health_request_format() {
        let req = build_get_health();
        assert!(req.starts_with("GET /health HTTP/1.1\r\n"));
        assert!(req.ends_with("\r\n\r\n"));
    }

    #[test]
    fn free_port_is_bindable() {
        let p = free_port().unwrap();
        assert!(p > 0, "порт должен быть ненулевым (ephemeral range)");
    }

    #[test]
    fn stdout_json_parses_with_trailing_garbage() {
        let raw = "\nok\n{\n  \"ratio\": 0.5\n}\nвнимание: хвост\n";
        let v = parse_stdout_json(raw.as_bytes())
            .expect("хвостовой мусор допустим");
        assert_eq!(v["ratio"], 0.5);
    }

    #[test]
    fn find_engine_bin_respects_env() {
        // Явный POLER_ENGINE_BIN имеет приоритет (файл не обязан существовать
        // для проверки приоритета — используем существующий temp-файл).
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("my-engine");
        std::fs::write(&fake, b"#!/bin/sh\n").unwrap();
        // SAFETY: однопоточный тест изолирует переменную до конца функции.
        std::env::set_var("POLER_ENGINE_BIN", &fake);
        let found = find_engine_bin();
        std::env::remove_var("POLER_ENGINE_BIN");
        assert_eq!(found, Some(fake));
    }

    // ── Интеграционные: требуют живой бинарь poler-engine ──

    #[test]
    fn daemon_spawn_health_tools_and_search() {
        let Some(bin) = engine_bin() else {
            eprintln!("SKIP: poler-engine не найден (PATH/POLER_ENGINE_BIN)");
            return;
        };
        let d = EngineDaemon::spawn(bin).expect("демон поднимается");
        assert!(d.uptime_secs() < 15);

        // tools/list — живой каталог демона
        let tools = d.call("tools/list", json!({})).expect("tools/list");
        let n = tools["tools"].as_array().map(|a| a.len()).unwrap_or(0);
        assert!(n >= 30, "у демона ≥30 инструментов, получили {n}");

        // poler_grep c json:true — машинно-читаемый отчёт
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), b"fn alpha() {}\nfn beta() {}\n").unwrap();
        let g = d
            .tool(
                "poler_grep",
                json!({"path": dir.path(), "pattern": "alpha", "output": "count", "json": true}),
            )
            .expect("grep через демон");
        assert_eq!(g["is_error"], false);
        let files = g["files"].as_array().expect("json-отчёт grep");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["count"], 1);
    }

    #[test]
    fn pack_and_squeeze_and_archive_list() {
        let Some(bin) = engine_bin() else {
            eprintln!("SKIP: poler-engine не найден");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("proj");
        std::fs::create_dir_all(&src).unwrap();
        // Boilerplate-корпус: закон масштаба — повторы обязаны схлопнуться.
        let boiler = "fn process(input: &[u8]) -> Vec<u8> {\n    input.iter().map(|&b| b ^ 0x5A).collect()\n}\n\n".repeat(64);
        for i in 0..12 {
            std::fs::write(src.join(format!("mod_{i:02}.rs")), &boiler).unwrap();
        }
        let out = dir.path().join("proj.poler");
        let stats = pack_dir(&bin, &src, &out, false, 2).expect("pack");
        assert!(out.is_file(), ".poler создан");
        assert!(stats["files"].as_u64().unwrap_or(0) >= 12, "files: {stats}");
        let raw = stats["total_raw"].as_f64().unwrap_or(0.0);
        let packed = std::fs::metadata(&out).unwrap().len() as f64;
        assert!(
            raw > 0.0 && packed / raw < 0.05,
            "закон масштаба: raw={raw} packed={packed}"
        );

        // Закон семантики: tar.gz → .poler с tar_mode=true (контейнер развёрнут на лету)
        let tarball = dir.path().join("corpus.tar.gz");
        let tar_out = dir.path().join("corpus.poler");
        let tgz = {
            use std::io::Write as _;
            // Простейший gzip-заголовок не соберёшь руками честно — используем
            // системный tar (Linux CI). Фолбек: tar без сжатия (.tar) — тоже контейнер.
            let status = Command::new("tar")
                .args(["czf"])
                .arg(&tarball)
                .arg("-C")
                .arg(dir.path())
                .arg("proj")
                .status();
            status.map(|s| s.success()).unwrap_or(false)
        };
        if tgz && tarball.is_file() {
            let s = stream_file(&bin, &tarball, &tar_out).expect("stream-file tar.gz");
            assert_eq!(s["tar_mode"], true, "пофайловая таблица (tar-режим): {s}");
            assert!(s["total_raw"].as_f64().unwrap_or(0.0) > 0.0);
            assert!(tar_out.is_file());
            let listing = archive_list(&bin, &tar_out).expect("archive-list");
            let entries = listing["entries"].as_array().expect("entries");
            assert!(!entries.is_empty(), "файлы видны внутри .poler");
        } else {
            eprintln!("SKIP: tar недоступен в окружении");
        }
    }

    #[test]
    fn impact_and_crystal_ingest() {
        let Some(bin) = engine_bin() else {
            eprintln!("SKIP: poler-engine не найден");
            return;
        };
        // impact: на мини-корпусе из одного файла.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn outer() { inner() }\nfn inner() -> u32 { 7 }\n",
        )
        .unwrap();
        let imp = impact(&bin, dir.path(), "inner", 0).expect("impact");
        assert!(imp.get("target_function").is_some() || imp.get("symbol").is_some(),
            "impact JSON: {imp}");

        // crystal-ingest: корпус ≥16 слов.
        let cdir = tempfile::tempdir().unwrap();
        let words: Vec<String> = (0..64).map(|i| format!("слово{i}")).collect();
        std::fs::write(
            cdir.path().join("corpus.txt"),
            words.join(" ") + " резонанс кристалл поле фаза",
        )
        .unwrap();
        let t5c = cdir.path().join("crystal.t5c");
        let c = crystal_ingest(&bin, cdir.path(), &t5c, 512).expect("crystal-ingest");
        assert_eq!(c["ok"], true, "кристалл создан: {c}");
        assert!(t5c.is_file() && std::fs::metadata(&t5c).unwrap().len() > 0);
    }

    // ── Phase 5++.A: наблюдаемость демона (глава 4 плана v2.0) ──

    /// Приёмка 4.4: счётчики гейта, кэш tools_count, RSS живого демона,
    /// полный статус-блок, stop() действительно убивает процесс.
    #[test]
    fn daemon_observability_counters_rss_status() {
        let Some(bin) = engine_bin() else {
            eprintln!("SKIP: poler-engine не найден (PATH/POLER_ENGINE_BIN)");
            return;
        };
        let mut d = EngineDaemon::spawn(bin).expect("демон поднимается");

        // tools_count закэширован при спавне (tools/list) — панель
        // показывает размер каталога сразу
        let tools = d.tools_count.load(Ordering::Relaxed);
        assert!(tools >= 30, "tools_count при спавне: {tools}");

        // счётчик запросов растёт; латентность заполняется
        let before = d.requests.load(Ordering::Relaxed);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), b"fn alpha() {}\n").unwrap();
        let g = d
            .tool(
                "poler_grep",
                json!({"path": dir.path(), "pattern": "alpha", "output": "count", "json": true}),
            )
            .expect("grep через демон");
        assert_eq!(g["is_error"], false);
        let after = d.requests.load(Ordering::Relaxed);
        assert!(after > before, "счётчик запросов растёт: {before} → {after}");
        assert!(
            d.last_latency_ms.load(Ordering::Relaxed) < 60_000,
            "латентность разумна"
        );

        // RSS живого демона — мегабайты, не гигабайты (приёмка 4.4)
        let rss = d.rss_bytes();
        assert!(rss > 1024 * 1024, "RSS живого демона > 1 МиБ: {rss}");
        assert!(rss < 8 * 1024 * 1024 * 1024, "RSS < 8 ГиБ: {rss}");

        // log_tail — строка (возможно пустая: тихий демон)
        let _tail = d.log_tail();

        // статус-блок: полный набор полей панели DaemonPanel
        let st = d.status_json();
        assert_eq!(st["running"], true);
        for k in ["addr", "port", "pid", "uptime_secs", "daemon_version",
                  "rss_bytes", "tools_count", "requests", "last_latency_ms",
                  "last_error", "log_tail"] {
            assert!(st.get(k).is_some(), "нет поля {k}: {st}");
        }
        assert!(st["port"].as_u64().unwrap() > 0, "порт отдельно: {st}");
        assert!(st["requests"].as_u64().unwrap() >= 2, "запросы считаются: {st}");

        // живой демон → is_alive; stop() → мёртвый (идемпотентно)
        assert!(d.is_alive(), "демон жив до stop");
        d.stop();
        assert!(!d.is_alive(), "демон мёртв после stop");
        d.stop(); // повторный stop не паникует
    }
}
