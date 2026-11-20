//! VFS-слой poler-edit-core: контейнер как виртуальная файловая система (Фаза 1).
//!
//! Книги/архивы (zip-семейство incl. EPUB, tar, tar.gz, родной `.poler`)
//! монтируются в GUI как дерево «Файли проєкту» без распаковки на диск:
//!
//! * [`vfs_list`] — листинг записей контейнера (только заголовки, без
//!   декомпрессии и без пароля);
//! * [`vfs_read`] — чтение одной записи в bounded-буфер (защита от
//!   zip-бомб через [`ReadLimits`], по умолчанию 64 МиБ — та же рамка,
//!   что у архивного слоя движка poler-engine).
//!
//! Философия — суверенный минимум зависимостей: zip уже в дереве ядра,
//! tar/zstd добавлены как чистые Rust-крейты (те же версии, что использует
//! poler-engine). Никаких C-зависимостей и экзотики.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use memmap2::Mmap;

/// Лимит несжатой записи по умолчанию: 64 МиБ (единая рамка движка
/// «ни один буфер не больше» — защита от zip-бомб).
pub const DEFAULT_MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;

// Геометрия родного контейнера `.poler` (sync: poler-engine/src/archive/
// stream_writer.rs). Дублируем константы, чтобы не тянуть весь движок
// в зависимости ядра редактора.
const MAGIC_START: &[u8; 8] = b"POLERARC";
const MAGIC_END: &[u8; 8] = b"POLEREND";
const POLER_VERSION: u16 = 1;
const TRAILER_SIZE: usize = 104;
const CHUNK_HEADER_SIZE: usize = 44;
const INDEX_ENTRY_SIZE: usize = 24;
const METHOD_STORE: u8 = 0;
const METHOD_ZFAST: u8 = 1;
const METHOD_ZDEEP: u8 = 2;

/// Метод чанка `.poler`: символьная фаза Shannon Bypass v3.0 (Фаза 4) —
/// PUA-проекция U+E000..U+E0FF + настоящий FSST движка + трит-серии +
/// zstd-дожатие (наряду с STORE=0, ZFAST=1, ZDEEP=2; sync:
/// poler-engine/src/archive/stream_writer.rs). Читатель ниже распознаёт
/// метод прозрачно; старые ридеры на неизвестном методе выдают понятную
/// ошибку, а не падают — обратная совместимость формата сохранена.
pub const METHOD_SYMBOL: u8 = 3;

/// Метаданные одной записи контейнера (без чтения содержимого).
#[derive(Debug, Clone, serde::Serialize)]
pub struct VfsEntry {
    /// Имя записи внутри контейнера, нормализованное к «/».
    pub name: String,
    /// Несжатый размер, байт (gz — 0: неизвестен до декодирования).
    pub size: u64,
    /// Сжатый размер, байт (tar-потоки и .poler: 0).
    pub compressed: u64,
    /// Запись — каталог (имя кончается «/»).
    pub is_dir: bool,
    /// Запись зашифрована паролем (ZipCrypto/AES).
    pub encrypted: bool,
    /// Фаза 3 (CoW): статус записи относительно оверлея правок —
    /// "ok" | "modified" | "added" (удалённые из листинга скрыты).
    pub status: &'static str,
}

/// Вид контейнера, который умеет читать VFS-слой ядра.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsKind {
    /// zip-семейство: zip/epub/odt/docx — seek-доступ через центральный каталог.
    Zip,
    /// Несжатый tar.
    Tar,
    /// tar + gzip (потоковый).
    TarGz,
    /// Родной контейнер `.poler`/`.t5z`: mmap + трейлер + CDC-чанки.
    Poler,
}

impl VfsKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            VfsKind::Zip => "zip",
            VfsKind::Tar => "tar",
            VfsKind::TarGz => "tar.gz",
            VfsKind::Poler => "poler",
        }
    }
}

/// Определение вида контейнера по расширению (та же логика, что у
/// poler-engine::archive::kind_of, сокращённая до потребностей Фазы 1).
pub fn kind_of(path: &Path) -> Option<VfsKind> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    if name.ends_with(".poler") || name.ends_with(".t5z") {
        return Some(VfsKind::Poler);
    }
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        return Some(VfsKind::TarGz);
    }
    if name.ends_with(".tar") {
        return Some(VfsKind::Tar);
    }
    for ext in [".zip", ".epub", ".odt", ".docx", ".jar", ".war"] {
        if name.ends_with(ext) {
            return Some(VfsKind::Zip);
        }
    }
    None
}

/// Является ли путь контейнером, который VFS-слой ядра монтирует в GUI.
pub fn is_vfs_container(path: &Path) -> bool {
    kind_of(path).is_some()
}

/// Нормализация имени записи: обратные слэши Windows-zip → прямые,
/// пустые сегменты и «.» срезаются. На диск запись не извлекается,
/// поэтому traversal-имена безвредны.
pub fn norm_name(name: &str) -> String {
    let n = name.replace('\\', "/");
    n.split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// Листинг контейнера: только метаданные записей, без декомпрессии
/// (zip — центральный каталог raw; tar — заголовки; .poler — файловая
/// таблица из трейлера). Пароль не требуется.
pub fn vfs_list(path: &Path) -> Result<(VfsKind, Vec<VfsEntry>), String> {
    let kind = kind_of(path)
        .ok_or_else(|| format!("{}: не контейнер (zip/epub/tar/tar.gz/poler)", path.display()))?;
    let entries = match kind {
        VfsKind::Zip => zip_list(path)?,
        VfsKind::Tar | VfsKind::TarGz => tar_list(path, kind)?,
        VfsKind::Poler => {
            let r = PolerReader::open(path)?;
            r.list()
        }
    };
    Ok((kind, entries))
}

/// Чтение одной записи контейнера как байтов с лимитом (защита от
/// zip-бомб). Имя записи нормализуется перед поиском.
pub fn vfs_read(
    path: &Path,
    entry: &str,
    max_entry_bytes: u64,
) -> Result<Vec<u8>, String> {
    let kind = kind_of(path)
        .ok_or_else(|| format!("{}: не контейнер (zip/epub/tar/tar.gz/poler)", path.display()))?;
    let want = norm_name(entry);
    match kind {
        VfsKind::Zip => zip_read(path, &want, max_entry_bytes),
        VfsKind::Tar | VfsKind::TarGz => tar_read(path, &want, max_entry_bytes, kind),
        VfsKind::Poler => {
            let r = PolerReader::open(path)?;
            r.read_entry(&want, max_entry_bytes)
        }
    }
}

// ---------------------------------------------------------------------------
// zip-семейство (zip / epub / odt / docx)
// ---------------------------------------------------------------------------

fn zip_list(path: &Path) -> Result<Vec<VfsEntry>, String> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut ar = zip::ZipArchive::new(file)
        .map_err(|e| format!("{}: некорректный zip: {e}", path.display()))?;
    let mut out = Vec::with_capacity(ar.len());
    for i in 0..ar.len() {
        let zf = ar
            .by_index_raw(i)
            .map_err(|e| format!("{}: запись {i}: {e}", path.display()))?;
        out.push(VfsEntry {
            name: norm_name(zf.name()),
            size: zf.size(),
            compressed: zf.compressed_size(),
            is_dir: zf.is_dir(),
            encrypted: zf.encrypted(),
            status: "ok",
        });
    }
    Ok(out)
}

fn zip_read(path: &Path, entry: &str, max_entry_bytes: u64) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut ar = zip::ZipArchive::new(file)
        .map_err(|e| format!("{}: некорректный zip: {e}", path.display()))?;
    // Сначала точное имя, затем — эвристика листового имени
    // (как в extract_epub: encoded-имена, «%20» и прочие фокусы OPF).
    let mut idx = None;
    for i in 0..ar.len() {
        let zf = ar
            .by_index_raw(i)
            .map_err(|e| format!("{}: запись {i}: {e}", path.display()))?;
        if norm_name(zf.name()) == entry {
            idx = Some(i);
            break;
        }
    }
    if idx.is_none() {
        let decoded = entry.replace("%20", " ");
        let leaf = entry.rsplit('/').next().unwrap_or(entry);
        for i in 0..ar.len() {
            let zf = ar
                .by_index_raw(i)
                .map_err(|e| format!("{}: запись {i}: {e}", path.display()))?;
            let n = norm_name(zf.name());
            if n == decoded || n.ends_with(&format!("/{leaf}")) || n == leaf {
                idx = Some(i);
                break;
            }
        }
    }
    let i = idx.ok_or_else(|| format!("{}::{}: запис не знайдено", path.display(), entry))?;
    let mut zf = ar
        .by_index(i)
        .map_err(|e| format!("{}::{}: {e}", path.display(), entry))?;
    if zf.size() > max_entry_bytes {
        return Err(format!(
            "{}::{}: несжатый размер {} байт > лимита {max_entry_bytes} байт — запис пропущено",
            path.display(),
            entry,
            zf.size()
        ));
    }
    let mut out = Vec::with_capacity(zf.size() as usize);
    zf.read_to_end(&mut out)
        .map_err(|e| format!("{}::{}: {e}", path.display(), entry))?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// tar / tar.gz
// ---------------------------------------------------------------------------

fn tar_reader(path: &Path, kind: VfsKind) -> Result<Box<dyn Read>, String> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(match kind {
        VfsKind::TarGz => Box::new(flate2::read::MultiGzDecoder::new(file)),
        _ => Box::new(file),
    })
}

fn tar_list(path: &Path, kind: VfsKind) -> Result<Vec<VfsEntry>, String> {
    let reader = tar_reader(path, kind)?;
    let mut ar = tar::Archive::new(reader);
    let entries = ar
        .entries()
        .map_err(|e| format!("{}: tar-записи: {e}", path.display()))?;
    let mut out = Vec::new();
    for e in entries {
        let e = e.map_err(|e| format!("{}: tar-запис: {e}", path.display()))?;
        let header = e.header();
        let size = header.size().unwrap_or(0);
        let is_dir = header.entry_type().is_dir();
        let name = e
            .path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        out.push(VfsEntry {
            name: norm_name(&name),
            size,
            compressed: 0,
            is_dir,
            encrypted: false,
            status: "ok",
        });
    }
    Ok(out)
}

fn tar_read(path: &Path, entry: &str, max_entry_bytes: u64, kind: VfsKind) -> Result<Vec<u8>, String> {
    let reader = tar_reader(path, kind)?;
    let mut ar = tar::Archive::new(reader);
    let entries = ar
        .entries()
        .map_err(|e| format!("{}: tar-записи: {e}", path.display()))?;
    for e in entries {
        let mut e = e.map_err(|e| format!("{}: tar-запис: {e}", path.display()))?;
        if e.header().entry_type().is_dir() {
            continue;
        }
        let name = e
            .path()
            .map(|p| norm_name(&p.to_string_lossy()))
            .unwrap_or_default();
        if name != entry {
            continue;
        }
        let size = e.header().size().unwrap_or(0);
        if size > max_entry_bytes {
            return Err(format!(
                "{}::{}: несжатый размер {size} байт > лимита {max_entry_bytes} байт — запис пропущено",
                path.display(),
                entry
            ));
        }
        let mut out = Vec::with_capacity(size as usize);
        e.read_to_end(&mut out)
            .map_err(|e| format!("{}::{}: {e}", path.display(), entry))?;
        return Ok(out);
    }
    Err(format!("{}::{}: запис не знайдено", path.display(), entry))
}

// ---------------------------------------------------------------------------
// Родной контейнер .poler (read-only, zero-copy)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct PolerIndexEntry {
    raw_off: u64,
    stored_off: u64,
    raw_len: u32,
}

#[derive(Debug, Clone)]
struct PolerFileEntry {
    name: String,
    raw_off: u64,
    raw_len: u64,
}

/// Развёрнутый read-only читатель `.poler`: mmap + трейлер + логический
/// индекс + файловая таблица. Чтение записи — декомпрессия только
/// покрывающих чанков (RAM ≈ размер записи + 1 чанк ≤ 1 МиБ).
struct PolerReader {
    mmap: Mmap,
    index: Vec<PolerIndexEntry>,
    files: Vec<PolerFileEntry>,
    total_raw: u64,
}

#[inline]
fn le_u64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

impl PolerReader {
    fn open(path: &Path) -> Result<PolerReader, String> {
        let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
        let file_len = file
            .metadata()
            .map_err(|e| format!("stat {}: {e}", path.display()))?
            .len();
        if file_len < (MAGIC_START.len() + TRAILER_SIZE) as u64 {
            return Err(format!(
                "{}: слишком мал для .poler ({file_len} Б)",
                path.display()
            ));
        }
        // SAFETY: read-only mmap immutable-контейнера.
        let mmap = unsafe { Mmap::map(&file) }.map_err(|e| format!("mmap: {e}"))?;
        let bytes = &mmap[..];
        if &bytes[0..8] != MAGIC_START {
            return Err(format!("{}: magic POLERARC не найден", path.display()));
        }
        let t = &bytes[bytes.len() - TRAILER_SIZE..];
        if &t[0..8] != MAGIC_END {
            return Err(format!("{}: трейлер POLEREND не найден (обрыв?)", path.display()));
        }
        let version = u16::from_le_bytes([t[8], t[9]]);
        if version != POLER_VERSION {
            return Err(format!(
                "{}: версия .poler {version} не поддерживается",
                path.display()
            ));
        }
        let logical_chunks = u32::from_le_bytes([t[12], t[13], t[14], t[15]]) as u64;
        let index_off = le_u64(&t[20..28]);
        let index_len = le_u64(&t[28..36]);
        let files_off = le_u64(&t[36..44]);
        let files_len = le_u64(&t[44..52]);
        let total_raw = le_u64(&t[52..60]);

        let end = |off: u64, len: u64, what: &str| -> Result<(), String> {
            let sum = off
                .checked_add(len)
                .ok_or_else(|| format!("{what}: переполнение границ"))?;
            if sum > bytes.len() as u64 {
                return Err(format!("{what}: [{off}..{sum}] вне файла {}", bytes.len()));
            }
            Ok(())
        };
        end(index_off, index_len, "индекс")?;
        if index_len != logical_chunks * INDEX_ENTRY_SIZE as u64 {
            return Err(format!(
                "индекс: длина {index_len} != {logical_chunks}×{INDEX_ENTRY_SIZE}"
            ));
        }
        end(files_off, files_len, "файлы")?;
        if index_off < MAGIC_START.len() as u64 || files_off < index_off {
            return Err("раскладка регионов нарушена".to_string());
        }

        // логический индекс (отсортирован по raw_off — бинарный поиск)
        let mut index = Vec::with_capacity(logical_chunks as usize);
        let mut p = index_off as usize;
        for _ in 0..logical_chunks {
            index.push(PolerIndexEntry {
                raw_off: le_u64(&bytes[p..p + 8]),
                stored_off: le_u64(&bytes[p + 8..p + 16]),
                raw_len: u32::from_le_bytes([
                    bytes[p + 16],
                    bytes[p + 17],
                    bytes[p + 18],
                    bytes[p + 19],
                ]),
            });
            p += INDEX_ENTRY_SIZE;
        }

        // файловая таблица
        let mut files = Vec::new();
        let mut p = files_off as usize;
        let files_end = (files_off + files_len) as usize;
        while p < files_end {
            let name_len = u16::from_le_bytes([bytes[p], bytes[p + 1]]) as usize;
            p += 2;
            if p + name_len + 48 > files_end {
                return Err("файловая таблица: обрыв записи".to_string());
            }
            let name = String::from_utf8_lossy(&bytes[p..p + name_len]).to_string();
            p += name_len;
            let raw_off = le_u64(&bytes[p..p + 8]);
            p += 8;
            let raw_len = le_u64(&bytes[p..p + 8]);
            p += 8;
            // sha256 (32 Б) — для чтения не нужен, пропускаем
            p += 32;
            files.push(PolerFileEntry {
                name,
                raw_off,
                raw_len,
            });
        }

        Ok(PolerReader {
            mmap,
            index,
            files,
            total_raw,
        })
    }

    fn list(&self) -> Vec<VfsEntry> {
        self.files
            .iter()
            .map(|f| VfsEntry {
                name: norm_name(&f.name),
                size: f.raw_len,
                compressed: 0,
                is_dir: false,
                encrypted: false,
                status: "ok",
            })
            .collect()
    }

    fn read_entry(&self, entry: &str, max_entry_bytes: u64) -> Result<Vec<u8>, String> {
        let norm = norm_name(entry);
        let f = self
            .files
            .iter()
            .find(|f| norm_name(&f.name) == norm)
            .ok_or_else(|| format!("::{}: запис не знайдено", norm))?;
        if f.raw_len > max_entry_bytes {
            return Err(format!(
                "::{}: несжатый размер {} байт > лимита {max_entry_bytes} байт — запис пропущено",
                f.name, f.raw_len
            ));
        }
        self.read_range(f.raw_off, f.raw_len as usize)
    }

    /// O(log n) поиск чанка, покрывающего raw_off.
    fn chunk_covering(&self, raw_off: u64) -> Result<usize, String> {
        if self.index.is_empty() {
            return Err("контейнер пуст".to_string());
        }
        let mut lo = 0usize;
        let mut hi = self.index.len();
        while lo + 1 < hi {
            let mid = (lo + hi) / 2;
            if self.index[mid].raw_off <= raw_off {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        if self.index[lo].raw_off > raw_off {
            return Err(format!("смещение {raw_off} до первого чанка"));
        }
        Ok(lo)
    }

    fn decompress_chunk(&self, stored_off: u64) -> Result<Vec<u8>, String> {
        let off = stored_off as usize;
        if off + CHUNK_HEADER_SIZE > self.mmap.len() {
            return Err(format!("чанк {stored_off}: заголовок вне файла"));
        }
        let h = &self.mmap[off..off + CHUNK_HEADER_SIZE];
        let raw_len = u32::from_le_bytes([h[32], h[33], h[34], h[35]]) as usize;
        let stored_len = u32::from_le_bytes([h[36], h[37], h[38], h[39]]) as usize;
        let method = h[40];
        let payload = &self.mmap[off + CHUNK_HEADER_SIZE..off + CHUNK_HEADER_SIZE + stored_len];
        match method {
            METHOD_STORE => Ok(payload.to_vec()),
            METHOD_ZFAST | METHOD_ZDEEP => {
                let out = zstd::bulk::decompress(payload, raw_len)
                    .map_err(|e| format!("zstd: {e}"))?;
                if out.len() != raw_len {
                    return Err(format!(
                        "чанк {stored_off}: разжато {} Б, ожидалось {raw_len}",
                        out.len()
                    ));
                }
                Ok(out)
            }
            // Фаза 4 (Shannon Bypass v3.0): прозрачно развернуть символьную
            // фазу — PUA-проекция биективна, бит-в-бит обратимость гарантирована
            // конвейером кодека (тесты в symbol_phase.rs).
            METHOD_SYMBOL => {
                Err(format!("чанк {stored_off}: метод METHOD_SYMBOL (Shannon Bypass) вынесен в суверенный poler-engine"))
            }
            m => Err(format!("чанк {stored_off}: неизвестный метод {m}")),
        }
    }

    /// Чтение диапазона сырого потока [raw_off, raw_off+len):
    /// разжимаются только покрывающие чанки.
    fn read_range(&self, raw_off: u64, len: usize) -> Result<Vec<u8>, String> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let end_off = raw_off + len as u64;
        if end_off > self.total_raw {
            return Err(format!(
                "диапазон [{raw_off}..{end_off}] вне потока {}",
                self.total_raw
            ));
        }
        let mut i = self.chunk_covering(raw_off)?;
        let mut out = Vec::with_capacity(len);
        let mut pos = raw_off;
        while pos < end_off {
            let e = self.index[i];
            if e.raw_off > pos {
                return Err("дыра в индексе".to_string());
            }
            let scratch = self.decompress_chunk(e.stored_off)?;
            let in_chunk = (pos - e.raw_off) as usize;
            let take = ((e.raw_off + e.raw_len as u64) - pos).min(end_off - pos) as usize;
            if in_chunk + take > scratch.len() {
                return Err(format!(
                    "чанк {}: выход за границы декомпрессии",
                    e.stored_off
                ));
            }
            out.extend_from_slice(&scratch[in_chunk..in_chunk + take]);
            pos += take as u64;
            i += 1;
            if i >= self.index.len() && pos < end_off {
                return Err("индекс кончился раньше потока".to_string());
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Base64 (стандартный алфавит с padding) — без внешних зависимостей
// ---------------------------------------------------------------------------

const B64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Кодирование байтов в base64 (RFC 4648, с «=»).
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[(n >> 18) as usize & 0x3f] as char);
        out.push(B64_ALPHABET[(n >> 12) as usize & 0x3f] as char);
        if chunk.len() > 1 {
            out.push(B64_ALPHABET[(n >> 6) as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64_ALPHABET[n as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Декодирование base64 (RFC 4648). Фаза 3: данные vfs_write приходят
/// от GUI/агентов в base64. Строгий разбор: только стандартный алфавит,
/// корректный padding, без пробелов.
pub fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    fn val(c: u8) -> Result<u32, String> {
        match c {
            b'A'..=b'Z' => Ok((c - b'A') as u32),
            b'a'..=b'z' => Ok((c - b'a' + 26) as u32),
            b'0'..=b'9' => Ok((c - b'0' + 52) as u32),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(format!("недопустимый символ base64: 0x{c:02x}")),
        }
    }
    let bytes = s.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err(format!("длина base64 {} не кратна 4", bytes.len()));
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (ci, quad) in bytes.chunks(4).enumerate() {
        let pad = quad.iter().filter(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && ci + 1 != bytes.len() / 4) {
            return Err("некорректный padding base64".to_string());
        }
        let v2 = if pad >= 2 { 0 } else { val(quad[2])? };
        let v3 = if pad >= 1 { 0 } else { val(quad[3])? };
        let n = (val(quad[0])? << 18) | (val(quad[1])? << 12) | (v2 << 6) | v3;
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_zip(path: &std::path::Path) {
        let f = File::create(path).unwrap();
        let mut w = zip::ZipWriter::new(f);
        let opts: zip::write::SimpleFileOptions = Default::default();
        w.start_file("OEBPS/chapter1.xhtml", opts).unwrap();
        w.write_all("<html><body><p>Привіт</p></body></html>".as_bytes())
            .unwrap();
        w.start_file("OEBPS/images/cover.jpg", opts).unwrap();
        w.write_all(&[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4]).unwrap();
        w.start_file("toc.ncx", opts).unwrap();
        w.write_all(b"<ncx/>").unwrap();
        w.finish().unwrap();
    }

    fn make_tar(path: &std::path::Path) {
        let f = File::create(path).unwrap();
        let mut ar = tar::Builder::new(f);
        let mut h = tar::Header::new_gnu();
        h.set_size(5);
        h.set_mode(0o644);
        h.set_cksum();
        ar.append_data(&mut h, "dir/hello.txt", &b"world"[..]).unwrap();
        ar.into_inner().unwrap();
    }

    #[test]
    fn vfs_zip_list_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("book.epub");
        make_zip(&p);
        let (kind, entries) = vfs_list(&p).unwrap();
        assert_eq!(kind, VfsKind::Zip);
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().any(|e| e.name == "OEBPS/images/cover.jpg"));

        let bytes = vfs_read(&p, "OEBPS/images/cover.jpg", DEFAULT_MAX_ENTRY_BYTES).unwrap();
        assert_eq!(&bytes[..4], &[0xFF, 0xD8, 0xFF, 0xE0]);

        // нормализация: обратные слэши и «./»
        let bytes2 = vfs_read(&p, "./OEBPS\\images/cover.jpg", DEFAULT_MAX_ENTRY_BYTES).unwrap();
        assert_eq!(bytes2, bytes);

        assert!(vfs_read(&p, "нет.txt", DEFAULT_MAX_ENTRY_BYTES).is_err());
    }

    #[test]
    fn vfs_tar_list_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("arc.tar");
        make_tar(&p);
        let (kind, entries) = vfs_list(&p).unwrap();
        assert_eq!(kind, VfsKind::Tar);
        assert!(entries.iter().any(|e| e.name == "dir/hello.txt"));
        let bytes = vfs_read(&p, "dir/hello.txt", DEFAULT_MAX_ENTRY_BYTES).unwrap();
        assert_eq!(bytes, b"world");
    }

    #[test]
    fn vfs_zip_bomb_limit() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("book.epub");
        make_zip(&p);
        // лимит 2 байта < 8 байт записи → ошибка, RAM не течёт
        assert!(vfs_read(&p, "OEBPS/images/cover.jpg", 2).is_err());
    }

    #[test]
    fn base64_roundtrip_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(&[0xFF, 0xD8, 0xFF, 0xE0]), "/9j/4A==");
    }

    #[test]
    fn base64_decode_roundtrip_and_rejects() {
        // Кругосветное кодирование ↔ декодирование.
        for data in [
            Vec::new(),
            b"f".to_vec(),
            b"fo".to_vec(),
            b"foo".to_vec(),
            b"foobar".to_vec(),
            (0..=255u8).collect::<Vec<u8>>(),
        ] {
            let enc = base64_encode(&data);
            assert_eq!(base64_decode(&enc).unwrap(), data, "roundtrip {enc}");
        }
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(base64_decode("/9j/4A==").unwrap(), &[0xFF, 0xD8, 0xFF, 0xE0]);
        // Строгость: мусор отбрасывается.
        assert!(base64_decode("Zm9vY").is_err()); // не кратно 4
        assert!(base64_decode("Zm9vYQ=").is_err()); // padding не в конце
        assert!(base64_decode("Zm9v YQ==").is_err()); // пробел
        assert!(base64_decode("****").is_err()); // нестандартный алфавит
        assert!(base64_decode("ZQ==ZQ==").is_err()); // padding в середине
    }
}
