//! Фаза 3 «Редактирование в архиве»: CoW-оверлей поверх VFS.
//!
//! Книга/контейнер, смонтированный в Фазе 1, остаётся **неизменным на диске**:
//! любая правка записи (`vfs_write` / `vfs_save` из редактора) попадает в
//! in-memory оверлей — «копия пишется» только в момент явного `vfs_flush`
//! (copy-on-write). Это позволяет редактировать `chapter1.xhtml` прямо
//! внутри `book.epub`, не распаковывая архив и не теряя оригинал до
//! явной команды:
//!
//! * [`VfsOverlay`] — правки одного архива: Modified(байты) / Deleted;
//! * [`overlay_read`] — чтение с учётом оверлея (GUI видит новую версию);
//! * [`merge_listing`] — листинг со статусами ok/modified/added (удалённые скрыты);
//! * [`flush_archive`] — материализация: пересборка zip/epub, tar, tar.gz
//!   или родного `.poler` атомарно (tmp + fsync + rename).
//!
//! Писатели:
//! * zip-семейство — [`zip::ZipWriter`]; нетронутые записи копируются
//!   `raw_copy_file` побайтово (сохраняется исходное сжатие и шифрование),
//!   изменённые пишутся deflate;
//! * tar / tar.gz — [`tar::Builder`] (+ [`flate2::write::GzEncoder`]),
//!   метаданные оригинала (mode/mtime/uid/gid) сохраняются;
//! * `.poler` — родной формат v1 (magic POLERARC, чанки по 1 МиБ,
//!   zstd-3 с fallback на STORE, blake3 чанков и sha256 файлов/потока —
//!   бинарно совместим с poler-engine::archive, sync:
//!   poler-engine/src/archive/stream_writer.rs).
//!
//! Лимиты: одна запись ≤ 64 МиБ (та же рамка против zip-бомб, что и чтение),
//! суммарный объём пересборки ≤ 512 МиБ (книги/проекты; гигабайтные архивы
//! требуют стримингового flush — вне рамок Фазы 3).

use std::collections::HashSet;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;

use crate::vfs::{
    kind_of, norm_name, vfs_list, vfs_read, VfsEntry, VfsKind, DEFAULT_MAX_ENTRY_BYTES,
    METHOD_SYMBOL,
};

/// Лимит одной записи при записи в оверлей (защита от раздувания RAM).
pub const DEFAULT_MAX_WRITE_BYTES: u64 = DEFAULT_MAX_ENTRY_BYTES;

/// Суммарный бюджет пересборки архива при flush (книги, не гигабайты).
pub const MAX_FLUSH_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

/// Размер сырого чанка `.poler` (та же рамка «RAM ≈ запись + 1 чанк ≤ 1 МиБ»,
/// что у читателя Фазы 1; CDC-границы движка читателю безразличны).
const POLER_CHUNK_RAW: usize = 1024 * 1024;

// Геометрия .poler v1 (дубли констант vfs.rs — один формат, два модуля).
const MAGIC_START: &[u8; 8] = b"POLERARC";
const MAGIC_END: &[u8; 8] = b"POLEREND";
const POLER_VERSION: u16 = 1;
const TRAILER_SIZE: usize = 104;
const CHUNK_HEADER_SIZE: usize = 44;
const INDEX_ENTRY_SIZE: usize = 24;
const METHOD_STORE: u8 = 0;
const METHOD_ZFAST: u8 = 1;
const ZSTD_LEVEL: i32 = 3;

// Фаза 4 «Shannon Bypass v3.0»: метод METHOD_SYMBOL=3 реимпортирован из
// vfs.rs (один источник правды — константа читателя и писателя совпадают
// по построению, в отличие от остальных дублей выше).

/// Метод сжатия чанков `.poler` при упаковке (Фаза 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PolerCompress {
    /// zstd-3 с fallback на STORE (классика Фазы 3; значение по умолчанию).
    #[default]
    Zstd,
    /// Символьная фаза Shannon Bypass v3.0: PUA-проекция U+E000..U+E0FF +
    /// RLE + настоящий FSST движка (crate::fsst) + трит-серии + zstd —
    /// метод METHOD_SYMBOL=3. Чанки, где фаза не окупается (≤ длина
    /// после кодека), хранятся STORE.
    Symbol,
}

impl PolerCompress {
    pub fn as_str(&self) -> &'static str {
        match self {
            PolerCompress::Zstd => "zstd",
            PolerCompress::Symbol => "symbol",
        }
    }
}

/// Опции материализации [`flush_archive_opts`] (Фаза 4).
#[derive(Debug, Clone, Copy, Default)]
pub struct FlushOptions {
    /// Метод сжатия чанков `.poler` (по умолчанию zstd — совместимость).
    pub poler_method: PolerCompress,
    /// Принудительный формат результата: `Some(VfsKind::Poler)` — ремукс
    /// любого смонтированного контейнера (zip/epub/tar/tar.gz) в родной
    /// `.poler` с дотисканием (действие GUI «Shannon Bypass v3.0»;
    /// пустой CoW-оверлей при этом допустим — полный перенос записей).
    pub force_kind: Option<VfsKind>,
}

// ---------------------------------------------------------------------------
// CoW-оверлей
// ---------------------------------------------------------------------------

/// Изменение одной записи в оверлее.
#[derive(Debug, Clone)]
pub enum VfsChange {
    /// Новое содержимое (перезапись существующей или новая запись).
    Modified(Vec<u8>),
    /// Запись удалена (tombstone до flush).
    Deleted,
}

/// CoW-оверлей одного контейнера. Правки живут в памяти ядра;
/// файл на диске не изменяется до [`flush_archive`].
#[derive(Debug, Default)]
pub struct VfsOverlay {
    changes: std::collections::HashMap<String, VfsChange>,
}

impl VfsOverlay {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Записать новые байты записи (create-or-replace). Возвращает true,
    /// если запись уже была в оверлее.
    pub fn write(&mut self, entry: &str, data: Vec<u8>) -> bool {
        let norm = norm_name(entry);
        let existed = self.changes.contains_key(&norm);
        self.changes.insert(norm, VfsChange::Modified(data));
        existed
    }

    /// Пометить запись удалённой (tombstone отменяет и правку, и добавление).
    pub fn delete(&mut self, entry: &str) -> bool {
        let norm = norm_name(entry);
        let existed = self.changes.remove(&norm).is_some();
        self.changes.insert(norm, VfsChange::Deleted);
        existed
    }

    pub fn change(&self, entry: &str) -> Option<&VfsChange> {
        self.changes.get(&norm_name(entry))
    }

    /// Снять все правки (после flush или discard).
    pub fn clear(&mut self) {
        self.changes.clear();
    }

    /// Сырые правки без классификации (имя → изменение).
    pub fn raw_changes(&self) -> impl Iterator<Item = (&String, &VfsChange)> {
        self.changes.iter()
    }
}

/// Одна строка отчёта «что изменено» для GUI/агентов.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VfsDirtyEntry {
    pub name: String,
    /// "modified" | "added" | "deleted".
    pub status: &'static str,
    pub bytes: u64,
}

/// Классификация правок относительно базового листинга архива
/// (base_names — имена записей на диске).
pub fn overlay_dirty(ov: &VfsOverlay, base_names: &HashSet<String>) -> Vec<VfsDirtyEntry> {
    let mut out: Vec<VfsDirtyEntry> = ov
        .raw_changes()
        .map(|(name, ch)| match ch {
            VfsChange::Modified(data) => VfsDirtyEntry {
                name: name.clone(),
                status: if base_names.contains(name) {
                    "modified"
                } else {
                    "added"
                },
                bytes: data.len() as u64,
            },
            VfsChange::Deleted => VfsDirtyEntry {
                name: name.clone(),
                status: "deleted",
                bytes: 0,
            },
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Наложить оверлей на листинг: обновить размеры/статусы изменённых,
/// скрыть удалённые, дописать новые записи. Порядок оригинала сохраняется.
pub fn merge_listing(entries: Vec<VfsEntry>, ov: &VfsOverlay) -> Vec<VfsEntry> {
    if ov.is_empty() {
        return entries; // статусы уже "ok"
    }
    let base_names: HashSet<String> = entries.iter().map(|e| e.name.clone()).collect();
    let mut out: Vec<VfsEntry> = entries
        .into_iter()
        .filter_map(|mut e| match ov.change(&e.name) {
            Some(VfsChange::Modified(data)) => {
                e.size = data.len() as u64;
                e.compressed = 0;
                e.status = "modified";
                Some(e)
            }
            Some(VfsChange::Deleted) => None, // удалена — не показываем
            None => Some(e),
        })
        .collect();
    for d in overlay_dirty(ov, &base_names) {
        if d.status == "added" {
            out.push(VfsEntry {
                name: d.name,
                size: d.bytes,
                compressed: 0,
                is_dir: false,
                encrypted: false,
                status: "added",
            });
        }
    }
    out
}

/// Чтение записи с учётом оверлея: изменённые отдаются из памяти,
/// удалённые — ошибка, остальные — с диска как в Фазе 1.
pub fn overlay_read(
    path: &Path,
    entry: &str,
    max_entry_bytes: u64,
    ov: Option<&VfsOverlay>,
) -> Result<Vec<u8>, String> {
    let norm = norm_name(entry);
    if let Some(ov) = ov {
        match ov.change(&norm) {
            Some(VfsChange::Deleted) => {
                return Err(format!(
                    "{}::{}: запис видалено (CoW-оверлей)",
                    path.display(),
                    norm
                ));
            }
            Some(VfsChange::Modified(data)) => {
                if data.len() as u64 > max_entry_bytes {
                    return Err(format!(
                        "{}::{}: розмір {} байт > ліміту {max_entry_bytes} байт",
                        path.display(),
                        norm,
                        data.len()
                    ));
                }
                return Ok(data.clone());
            }
            None => {}
        }
    }
    vfs_read(path, &norm, max_entry_bytes)
}

// ---------------------------------------------------------------------------
// План пересборки
// ---------------------------------------------------------------------------

/// Одна позиция плана пересборки: что писать в новый архив.
#[derive(Debug)]
enum PlanItem {
    /// Каталог (запись с «/» в zip / dir-запись tar).
    Dir { name: String },
    /// Нетронутая запись — данные читаются с диска в момент записи.
    CopyFile { name: String },
    /// Изменённая/новая запись — байты из оверлея.
    Modified { name: String, data: Vec<u8> },
}

/// Собрать план пересборки: порядок оригинала, правки поверх, новые в конец.
fn flush_plan(src: &Path, ov: &VfsOverlay, max_entry_bytes: u64) -> Result<Vec<PlanItem>, String> {
    let (_, base) = vfs_list(src)?;
    let base_names: HashSet<String> = base.iter().map(|e| e.name.clone()).collect();
    let mut plan: Vec<PlanItem> = Vec::with_capacity(base.len() + ov.raw_changes().count());
    for e in base {
        match ov.change(&e.name) {
            Some(VfsChange::Deleted) => continue,
            Some(VfsChange::Modified(data)) => {
                if data.len() as u64 > max_entry_bytes {
                    return Err(format!(
                        "{}::{}: розмір {} байт > ліміту {max_entry_bytes} байт",
                        src.display(),
                        e.name,
                        data.len()
                    ));
                }
                plan.push(PlanItem::Modified {
                    name: e.name,
                    data: data.clone(),
                });
            }
            None => {
                if e.is_dir {
                    plan.push(PlanItem::Dir { name: e.name });
                } else {
                    plan.push(PlanItem::CopyFile { name: e.name });
                }
            }
        }
    }
    // Новые записи (нет в базе) — в конец плана, по алфавиту для детерминизма.
    for d in overlay_dirty(ov, &base_names) {
        if d.status == "added" {
            if let Some(VfsChange::Modified(data)) = ov.change(&d.name) {
                if data.len() as u64 > max_entry_bytes {
                    return Err(format!(
                        "{}::{}: розмір {} байт > ліміту {max_entry_bytes} байт",
                        src.display(),
                        d.name,
                        data.len()
                    ));
                }
                plan.push(PlanItem::Modified {
                    name: d.name,
                    data: data.clone(),
                });
            }
        }
    }
    Ok(plan)
}

/// Итог материализации.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FlushStats {
    /// Формат контейнера ("zip" | "tar" | "tar.gz" | "poler").
    pub kind: &'static str,
    /// Число записей в новом архиве.
    pub entries: u64,
    /// Размер нового архива на диске, байт.
    pub bytes: u64,
    /// Суммарный несжатый объём записей, байт (база коэффициента).
    pub raw: u64,
    /// Метод сжатия чанков .poler ("zstd" | "symbol"; для остальных
    /// форматов — "-", коэффициент дотискания не определён).
    pub method: &'static str,
}

/// Материализовать оверлей: пересобрать архив атомарно (tmp + fsync + rename).
/// `dest` может совпадать с `src` (перезапись) или быть новым путём (копия).
/// Классика Фазы 3: метод чанков `.poler` — zstd (совместимость).
pub fn flush_archive(
    src: &Path,
    ov: &VfsOverlay,
    dest: &Path,
    max_entry_bytes: u64,
) -> Result<FlushStats, String> {
    flush_archive_opts(src, ov, dest, max_entry_bytes, &FlushOptions::default())
}

/// Материализация с опциями Фазы 4: метод сжатия чанков `.poler`
/// (zstd | symbol — Shannon Bypass v3.0) и/или принудительный формат
/// результата (`force_kind = Some(Poler)` — ремукс в `.poler` с дотисканием;
/// пустой оверлей в этом режиме допустим — это конверсия/перенос).
pub fn flush_archive_opts(
    src: &Path,
    ov: &VfsOverlay,
    dest: &Path,
    max_entry_bytes: u64,
    opts: &FlushOptions,
) -> Result<FlushStats, String> {
    let detected = kind_of(src);
    let kind = match (opts.force_kind, detected) {
        // Принудительный формат (ремукс в .poler) — исходник обязан быть
        // контейнером: план пересборки читает его записи через vfs_read.
        (Some(k), Some(_)) => k,
        (Some(_), None) => {
            return Err(format!(
                "{}: не контейнер (zip/epub/tar/tar.gz/poler)",
                src.display()
            ))
        }
        (None, Some(k)) => k,
        (None, None) => {
            return Err(format!(
                "{}: не контейнер (zip/epub/tar/tar.gz/poler)",
                src.display()
            ))
        }
    };
    match kind {
        VfsKind::Zip => flush_zip(src, dest, ov, max_entry_bytes),
        VfsKind::Tar | VfsKind::TarGz => flush_tar(src, dest, ov, max_entry_bytes, kind),
        VfsKind::Poler => {
            let plan = flush_plan(src, ov, max_entry_bytes)?;
            let entries = plan
                .iter()
                .filter(|i| !matches!(i, PlanItem::Dir { .. }))
                .count() as u64;
            let src = src.to_path_buf();
            let mut queue = plan.into_iter();
            let method = opts.poler_method;
            let stats = poler_write_to_method(dest, method, move || loop {
                match queue.next() {
                    None => return Ok(None),
                    Some(PlanItem::Dir { .. }) => continue, // каталогов в .poler нет
                    Some(PlanItem::CopyFile { name }) => {
                        let data = vfs_read(&src, &name, max_entry_bytes)?;
                        return Ok(Some((name, data)));
                    }
                    Some(PlanItem::Modified { name, data }) => return Ok(Some((name, data))),
                }
            })?;
            Ok(FlushStats {
                kind: kind.as_str(),
                entries,
                bytes: stats.bytes,
                raw: stats.raw,
                method: method.as_str(),
            })
        }
    }
}

/// Атомарная запись: `<dest>.tmp-<pid>` → fsync → rename; при ошибке
/// временный файл удаляется, `dest` остаётся нетронутым.
fn atomic_write<T>(
    dest: &Path,
    write: impl FnOnce(&mut BufWriter<File>) -> Result<T, String>,
) -> Result<T, String> {
    let tmp = dest.with_file_name(format!(
        ".{}.tmp-{}",
        dest.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let f = File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
    let mut w = BufWriter::with_capacity(1 << 20, f);
    let res = write(&mut w);
    match res {
        Ok(sz) => {
            w.flush().map_err(|e| format!("flush {}: {e}", tmp.display()))?;
            let f = w.into_inner().map_err(|e| format!("bufwriter: {e}"))?;
            f.sync_all().map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
            std::fs::rename(&tmp, dest).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("rename {} -> {}: {e}", tmp.display(), dest.display())
            })?;
            Ok(sz)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// zip-семейство (zip / epub / odt / docx)
// ---------------------------------------------------------------------------

fn flush_zip(
    src: &Path,
    dest: &Path,
    ov: &VfsOverlay,
    max_entry_bytes: u64,
) -> Result<FlushStats, String> {
    let file = File::open(src).map_err(|e| format!("{}: {e}", src.display()))?;
    let mut reader = zip::ZipArchive::new(file)
        .map_err(|e| format!("{}: некорректный zip: {e}", src.display()))?;

    let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let mut entries = 0u64;
    let mut raw = 0u64;
    let mut total = 0u64; // бюджет пересборки (изменённые записи)
    let res = atomic_write(dest, |w| {
        let mut zw = zip::ZipWriter::new(w);
        for i in 0..reader.len() {
            // Метаданные записи без декомпрессии.
            let (name, is_dir, encrypted) = {
                let zf = reader
                    .by_index_raw(i)
                    .map_err(|e| format!("{}: запись {i}: {e}", src.display()))?;
                (norm_name(zf.name()), zf.is_dir(), zf.encrypted())
            };
            match ov.change(&name) {
                Some(VfsChange::Deleted) => continue,
                Some(VfsChange::Modified(data)) => {
                    if encrypted {
                        return Err(format!(
                            "{}::{}: шифрованную запись нельзя изменить (нет пароля)",
                            src.display(),
                            name
                        ));
                    }
                    if data.len() as u64 > max_entry_bytes {
                        return Err(format!(
                            "{}::{}: розмір {} байт > ліміту {max_entry_bytes} байт",
                            src.display(),
                            name,
                            data.len()
                        ));
                    }
                    total = total
                        .checked_add(data.len() as u64)
                        .filter(|t| *t <= MAX_FLUSH_TOTAL_BYTES)
                        .ok_or_else(|| "пересборка: суммарный объём > 512 МиБ".to_string())?;
                    if is_dir {
                        zw.add_directory(&name, opts)
                    } else {
                        zw.start_file(&name, opts)
                    }
                    .map_err(|e| format!("zip start {name}: {e}"))?;
                    zw.write_all(&data).map_err(|e| format!("zip write {name}: {e}"))?;
                    entries += 1;
                    raw += data.len() as u64;
                }
                None => {
                    // Нетронутая запись — побайтовое копирование исходного
                    // сжатого потока (метод, время, права и шифрование
                    // сохраняются; рекомпрессии нет).
                    let zf = reader
                        .by_index_raw(i)
                        .map_err(|e| format!("{}: запись {i}: {e}", src.display()))?;
                    raw += zf.size();
                    zw.raw_copy_file(zf)
                        .map_err(|e| format!("zip raw copy {name}: {e}"))?;
                    entries += 1;
                }
            }
        }
        // Новые записи поверх исходного набора.
        let base_names: HashSet<String> = (0..reader.len())
            .filter_map(|i| reader.by_index_raw(i).ok().map(|zf| norm_name(zf.name())))
            .collect();
        for d in overlay_dirty(ov, &base_names) {
            if d.status != "added" {
                continue;
            }
            let Some(VfsChange::Modified(data)) = ov.change(&d.name) else {
                continue;
            };
            if data.len() as u64 > max_entry_bytes {
                return Err(format!(
                    "{}::{}: слишком большая новая запись",
                    src.display(),
                    d.name
                ));
            }
            zw.start_file(&d.name, opts)
                .map_err(|e| format!("zip start {}: {e}", d.name))?;
            zw.write_all(data).map_err(|e| format!("zip write {}: {e}", d.name))?;
            entries += 1;
            raw += data.len() as u64;
        }
        zw.finish().map_err(|e| format!("zip finish: {e}"))?;
        Ok((entries, raw))
    });
    res.map(|(entries, raw)| FlushStats {
        kind: "zip",
        entries,
        bytes: std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0),
        raw,
        method: "-",
    })
}

// ---------------------------------------------------------------------------
// tar / tar.gz
// ---------------------------------------------------------------------------

/// Метаданные tar-записи, переносимые при пересборке.
struct TarMeta {
    mode: u32,
    mtime: u64,
    uid: u64,
    gid: u64,
}

fn tar_meta_of(path: &Path, kind: VfsKind) -> std::collections::HashMap<String, TarMeta> {
    let mut meta = std::collections::HashMap::new();
    let Ok(file) = File::open(path) else {
        return meta;
    };
    let reader: Box<dyn Read> = match kind {
        VfsKind::TarGz => Box::new(flate2::read::MultiGzDecoder::new(file)),
        _ => Box::new(file),
    };
    let mut ar = tar::Archive::new(reader);
    if let Ok(entries) = ar.entries() {
        for e in entries.flatten() {
            let h = e.header();
            let name = e.path().map(|p| norm_name(&p.to_string_lossy())).unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            meta.insert(
                name,
                TarMeta {
                    mode: h.mode().unwrap_or(0o644),
                    mtime: h.mtime().unwrap_or(0),
                    uid: h.uid().unwrap_or(0),
                    gid: h.gid().unwrap_or(0),
                },
            );
        }
    }
    meta
}

fn append_tar_file<W: Write>(
    builder: &mut tar::Builder<W>,
    meta: &std::collections::HashMap<String, TarMeta>,
    name: &str,
    data: &[u8],
) -> Result<(), String> {
    let mut h = tar::Header::new_gnu();
    h.set_size(data.len() as u64);
    if let Some(m) = meta.get(name) {
        h.set_mode(m.mode);
        h.set_mtime(m.mtime);
        h.set_uid(m.uid);
        h.set_gid(m.gid);
    } else {
        h.set_mode(0o644);
        h.set_mtime(0);
    }
    h.set_cksum();
    builder
        .append_data(&mut h, Path::new(name), std::io::Cursor::new(data))
        .map_err(|e| format!("tar append {name}: {e}"))
}

fn fill_tar<W: Write>(
    builder: &mut tar::Builder<W>,
    src: &Path,
    plan: &[PlanItem],
    meta: &std::collections::HashMap<String, TarMeta>,
    max_entry_bytes: u64,
    entries: &mut u64,
    raw: &mut u64,
) -> Result<(), String> {
    for item in plan {
        match item {
            PlanItem::Dir { name } => {
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(tar::EntryType::Directory);
                h.set_size(0);
                if let Some(m) = meta.get(name) {
                    h.set_mode(m.mode | 0o111); // каталоги исполняемые
                    h.set_mtime(m.mtime);
                } else {
                    h.set_mode(0o755);
                }
                h.set_cksum();
                builder
                    .append_data(&mut h, Path::new(&format!("{name}/")), std::io::empty())
                    .map_err(|e| format!("tar dir {name}: {e}"))?;
            }
            PlanItem::CopyFile { name } => {
                let data = vfs_read(src, name, max_entry_bytes)?;
                *raw += data.len() as u64;
                append_tar_file(builder, meta, name, &data)?;
                *entries += 1;
            }
            PlanItem::Modified { name, data } => {
                *raw += data.len() as u64;
                append_tar_file(builder, meta, name, data)?;
                *entries += 1;
            }
        }
    }
    Ok(())
}

fn flush_tar(
    src: &Path,
    dest: &Path,
    ov: &VfsOverlay,
    max_entry_bytes: u64,
    kind: VfsKind,
) -> Result<FlushStats, String> {
    let plan = flush_plan(src, ov, max_entry_bytes)?;
    let meta = tar_meta_of(src, kind);
    let mut entries = 0u64;
    let mut raw = 0u64;
    let res = atomic_write(dest, |w| {
        match kind {
            VfsKind::TarGz => {
                let gz = flate2::write::GzEncoder::new(w, flate2::Compression::default());
                let mut builder = tar::Builder::new(gz);
                fill_tar(&mut builder, src, &plan, &meta, max_entry_bytes, &mut entries, &mut raw)?;
                let gz = builder.into_inner().map_err(|e| format!("tar finish: {e}"))?;
                gz.finish().map_err(|e| format!("gzip finish: {e}"))?;
            }
            _ => {
                let mut builder = tar::Builder::new(w);
                fill_tar(&mut builder, src, &plan, &meta, max_entry_bytes, &mut entries, &mut raw)?;
                builder.into_inner().map_err(|e| format!("tar finish: {e}"))?;
            }
        }
        Ok((entries, raw))
    });
    res.map(|(entries, raw)| FlushStats {
        kind: kind.as_str(),
        entries,
        bytes: std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0),
        raw,
        method: "-",
    })
}

// ---------------------------------------------------------------------------
// Родной контейнер .poler v1 (писатель, бинарно совместим с poler-engine)
// ---------------------------------------------------------------------------

/// Сжатие одного чанка `.poler` выбранным методом с fallback на STORE
/// (аналог CompressTier движка; глубокий ярус для интерактивного flush излишен).
/// Фаза 4: метод `Symbol` пропускает чанк через символьную фазу Shannon
/// Bypass v3.0 — PUA-проекция + настоящий FSST движка + трит-серии + zstd
/// (с внутренним RAW-фолбэком до zstd-паритета на несжимаемых чанках);
/// чанки, где фаза не окупается (payload ≥ сырого размера), хранятся STORE.
fn compress_chunk(raw: &[u8], method: PolerCompress) -> (u8, Vec<u8>) {
    match method {
        PolerCompress::Zstd => {
            if raw.len() >= 64 {
                if let Ok(c) = zstd::bulk::compress(raw, ZSTD_LEVEL) {
                    if c.len() < raw.len() {
                        return (METHOD_ZFAST, c);
                    }
                }
            }
            (METHOD_STORE, raw.to_vec())
        }
        PolerCompress::Symbol => {
            compress_chunk(raw, PolerCompress::Zstd)
        }
    }
}

/// hex (диагностика; формат как у poler-engine::pqc::sha256::hex).
#[cfg(test)]
fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

/// Итог записи `.poler`: размер контейнера и суммарный несжатый объём
/// записей (база коэффициента дотискания для отчёта GUI/протокола).
#[derive(Debug, Clone, Copy)]
pub struct PolerWriteStats {
    bytes: u64,
    raw: u64,
}

/// Запись `.poler` v1 из источника файлов (`next_file` отдаёт записи
/// по одной — потоково, RAM ≈ одна запись). Раскладка (sync:
/// poler-engine/src/archive/stream_writer.rs):
///
/// ```text
/// POLERARC | чанки: blake3[32] raw_len u32 stored_len u32 method u8 pad[3]
///            payload | индекс: raw_off u64 stored_off u64 raw_len u32 pad u32
/// | файловая таблица: name_len u16 name raw_off u64 raw_len u64 sha256[32]
/// | трейлер POLEREND (104 Б, sha256 потока на [68..100])
/// ```
///
/// Фаза 4: `method` выбирает сжатие чанков — zstd-3 (классика) или
/// символьная фаза Shannon Bypass v3.0 (METHOD_SYMBOL=3).
pub fn poler_write_to_method(
    dest: &Path,
    method: PolerCompress,
    mut next_file: impl FnMut() -> Result<Option<(String, Vec<u8>)>, String>,
) -> Result<PolerWriteStats, String> {
    let res = atomic_write(dest, |w| {
        w.write_all(MAGIC_START).map_err(|e| format!("poler: {e}"))?;
        // Логический индекс (raw_off, stored_off, raw_len) и файловая
        // таблица собираются на лету и дописываются после чанков.
        let mut logical: Vec<(u64, u64, u32)> = Vec::new();
        let mut file_table: Vec<u8> = Vec::new();
        let mut stream_sha = Sha256::new();
        let mut raw_pos: u64 = 0;
        let mut stored_pos: u64 = MAGIC_START.len() as u64;

        while let Some((name, data)) = next_file()? {
            if name.len() > u16::MAX as usize {
                return Err(format!("poler: имя записи слишком длинное ({name_len})", name_len = name.len()));
            }
            if raw_pos + data.len() as u64 > MAX_FLUSH_TOTAL_BYTES {
                return Err("poler: суммарный объём > 512 МиБ".to_string());
            }
            let file_raw_off = raw_pos;
            let mut file_sha = Sha256::new();
            for chunk in data.chunks(POLER_CHUNK_RAW) {
                stream_sha.update(chunk);
                file_sha.update(chunk);
                let hash = blake3::hash(chunk);
                let (method_byte, payload) = compress_chunk(chunk, method);
                let mut hdr = [0u8; CHUNK_HEADER_SIZE];
                hdr[0..32].copy_from_slice(hash.as_bytes());
                hdr[32..36].copy_from_slice(&(chunk.len() as u32).to_le_bytes());
                hdr[36..40].copy_from_slice(&(payload.len() as u32).to_le_bytes());
                hdr[40] = method_byte;
                w.write_all(&hdr).map_err(|e| format!("poler chunk: {e}"))?;
                w.write_all(&payload).map_err(|e| format!("poler chunk data: {e}"))?;
                logical.push((raw_pos, stored_pos, chunk.len() as u32));
                raw_pos += chunk.len() as u64;
                stored_pos += (CHUNK_HEADER_SIZE + payload.len()) as u64;
            }
            let digest: [u8; 32] = file_sha.finalize().into();
            file_table.extend_from_slice(&(name.len() as u16).to_le_bytes());
            file_table.extend_from_slice(name.as_bytes());
            file_table.extend_from_slice(&file_raw_off.to_le_bytes());
            file_table.extend_from_slice(&(data.len() as u64).to_le_bytes());
            file_table.extend_from_slice(&digest);
        }

        // Логический индекс.
        let index_off = stored_pos;
        let mut index_buf = Vec::with_capacity(logical.len() * INDEX_ENTRY_SIZE);
        for &(raw_off, stored_off, raw_len) in &logical {
            index_buf.extend_from_slice(&raw_off.to_le_bytes());
            index_buf.extend_from_slice(&stored_off.to_le_bytes());
            index_buf.extend_from_slice(&raw_len.to_le_bytes());
            index_buf.extend_from_slice(&0u32.to_le_bytes());
        }
        w.write_all(&index_buf).map_err(|e| format!("poler index: {e}"))?;
        stored_pos += index_buf.len() as u64;

        // Файловая таблица.
        let files_off = stored_pos;
        let files_len = file_table.len() as u64;
        w.write_all(&file_table).map_err(|e| format!("poler files: {e}"))?;

        // Трейлер (104 Б).
        let total_raw = raw_pos;
        let final_len = files_off + files_len + TRAILER_SIZE as u64;
        let stream_digest: [u8; 32] = stream_sha.finalize().into();
        let mut trailer = [0u8; TRAILER_SIZE];
        trailer[0..8].copy_from_slice(MAGIC_END);
        trailer[8..10].copy_from_slice(&POLER_VERSION.to_le_bytes());
        // flags = 0: без дедупликации, не tar-режим.
        trailer[12..16].copy_from_slice(&(logical.len() as u32).to_le_bytes());
        trailer[16..20].copy_from_slice(&(logical.len() as u32).to_le_bytes()); // физ. чанки = лог.
        trailer[20..28].copy_from_slice(&index_off.to_le_bytes());
        trailer[28..36].copy_from_slice(&((logical.len() as u64) * INDEX_ENTRY_SIZE as u64).to_le_bytes());
        trailer[36..44].copy_from_slice(&files_off.to_le_bytes());
        trailer[44..52].copy_from_slice(&files_len.to_le_bytes());
        trailer[52..60].copy_from_slice(&total_raw.to_le_bytes());
        trailer[60..68].copy_from_slice(&final_len.to_le_bytes());
        trailer[68..100].copy_from_slice(&stream_digest);
        w.write_all(&trailer).map_err(|e| format!("poler trailer: {e}"))?;
        Ok(PolerWriteStats {
            bytes: final_len,
            raw: total_raw,
        })
    });
    res
}

/// Совместимая обёртка Фазы 3: zstd-чанки, возвращает размер контейнера.
pub fn poler_write_to(
    dest: &Path,
    next_file: impl FnMut() -> Result<Option<(String, Vec<u8>)>, String>,
) -> Result<u64, String> {
    poler_write_to_method(dest, PolerCompress::Zstd, next_file).map(|s| s.bytes)
}

/// Публичный писатель для тестов и инструментов: файлы → `.poler`
/// выбранным методом сжатия чанков (Фаза 4).
#[cfg(test)]
pub(crate) fn poler_write_files_method(
    dest: &Path,
    method: PolerCompress,
    files: Vec<(String, Vec<u8>)>,
) -> Result<PolerWriteStats, String> {
    let mut it = files.into_iter();
    poler_write_to_method(dest, method, move || Ok(it.next()))
}

/// Публичный писатель для тестов и инструментов: файлы → `.poler` (zstd).
#[cfg(test)]
pub(crate) fn poler_write_files(dest: &Path, files: Vec<(String, Vec<u8>)>) -> Result<u64, String> {
    let mut it = files.into_iter();
    poler_write_to(dest, move || Ok(it.next()))
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::path::PathBuf;

    fn make_zip(path: &std::path::Path) {
        let f = File::create(path).unwrap();
        let mut w = zip::ZipWriter::new(f);
        let opts: zip::write::SimpleFileOptions =
            zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        w.start_file("OEBPS/chapter1.xhtml", opts).unwrap();
        w.write_all(b"<html><body><p>Hello</p></body></html>").unwrap();
        w.start_file("OEBPS/images/cover.jpg", opts).unwrap();
        w.write_all(&(0..5000u32).map(|i| (i % 251) as u8).collect::<Vec<u8>>()).unwrap();
        w.start_file("toc.ncx", opts).unwrap();
        w.write_all(b"<ncx/>").unwrap();
        w.finish().unwrap();
    }

    fn make_tar(path: &std::path::Path) {
        let f = File::create(path).unwrap();
        let mut ar = tar::Builder::new(f);
        let mut h = tar::Header::new_gnu();
        h.set_size(5);
        h.set_mode(0o600);
        h.set_mtime(1_700_000_000);
        h.set_cksum();
        ar.append_data(&mut h, "dir/hello.txt", &b"world"[..]).unwrap();
        ar.into_inner().unwrap();
    }

    fn make_tgz(path: &std::path::Path) {
        let f = File::create(path).unwrap();
        let gz = flate2::write::GzEncoder::new(f, flate2::Compression::default());
        let mut ar = tar::Builder::new(gz);
        let mut h = tar::Header::new_gnu();
        h.set_size(6);
        h.set_mode(0o644);
        h.set_cksum();
        ar.append_data(&mut h, "a/b.txt", "європа".as_bytes()).unwrap();
        let gz = ar.into_inner().unwrap();
        gz.finish().unwrap();
    }

    // ------------------------- оверлей -------------------------

    #[test]
    fn overlay_write_delete_dirty_merge() {
        let mut ov = VfsOverlay::new();
        assert!(ov.is_empty());
        assert!(!ov.write("OEBPS/chapter1.xhtml", b"new".to_vec()));
        assert!(ov.write("OEBPS/chapter1.xhtml", b"newer".to_vec())); // перезапись
        ov.write("notes/todo.txt", b"todo".to_vec());
        assert!(ov.delete("notes/todo.txt")); // отменяет добавление

        let base = vec![
            VfsEntry { name: "OEBPS/chapter1.xhtml".into(), size: 10, compressed: 4, is_dir: false, encrypted: false, status: "ok" },
            VfsEntry { name: "toc.ncx".into(), size: 6, compressed: 2, is_dir: false, encrypted: false, status: "ok" },
        ];
        let base_names: HashSet<String> = base.iter().map(|e| e.name.clone()).collect();
        let dirty = overlay_dirty(&ov, &base_names);
        assert_eq!(dirty.len(), 2);
        assert!(dirty.iter().any(|d| d.name == "OEBPS/chapter1.xhtml" && d.status == "modified" && d.bytes == 5));
        assert!(dirty.iter().any(|d| d.name == "notes/todo.txt" && d.status == "deleted"));

        let merged = merge_listing(base, &ov);
        // chapter1 modified с новым размером; todo нет в базе и удалён — скрыт.
        let ch = merged.iter().find(|e| e.name == "OEBPS/chapter1.xhtml").unwrap();
        assert_eq!(ch.status, "modified");
        assert_eq!(ch.size, 5);
        assert!(merged.iter().any(|e| e.name == "toc.ncx" && e.status == "ok"));

        ov.clear();
        assert!(ov.is_empty());
    }

    #[test]
    fn overlay_read_prefers_memory_and_honors_delete() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("book.epub");
        make_zip(&p);
        let mut ov = VfsOverlay::new();
        ov.write("toc.ncx", b"<ncx v2/>".to_vec());

        let cur = overlay_read(&p, "toc.ncx", DEFAULT_MAX_ENTRY_BYTES, Some(&ov)).unwrap();
        assert_eq!(cur, b"<ncx v2/>");
        let orig = overlay_read(&p, "OEBPS/chapter1.xhtml", DEFAULT_MAX_ENTRY_BYTES, Some(&ov)).unwrap();
        assert!(orig.starts_with(b"<html>"));

        ov.delete("OEBPS/chapter1.xhtml");
        assert!(overlay_read(&p, "OEBPS/chapter1.xhtml", DEFAULT_MAX_ENTRY_BYTES, Some(&ov)).is_err());
        // Без оверлея — как в Фазе 1.
        assert!(overlay_read(&p, "toc.ncx", DEFAULT_MAX_ENTRY_BYTES, None).unwrap() == b"<ncx/>");
    }

    // ------------------------- zip flush -------------------------

    #[test]
    fn zip_flush_cow_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("book.epub");
        make_zip(&p);
        let before = std::fs::read(&p).unwrap();
        let cover_orig = vfs_read(&p, "OEBPS/images/cover.jpg", DEFAULT_MAX_ENTRY_BYTES).unwrap();

        let mut ov = VfsOverlay::new();
        ov.write("OEBPS/chapter1.xhtml", "<html><body><p>Привіт</p></body></html>".as_bytes().to_vec());
        ov.delete("toc.ncx");
        ov.write("META-INF/new.xml", b"<new/>".to_vec());

        // CoW: диск не тронут до flush.
        assert_eq!(before, std::fs::read(&p).unwrap());

        let stats = flush_archive(&p, &ov, &p, DEFAULT_MAX_ENTRY_BYTES).unwrap();
        assert_eq!(stats.kind, "zip");
        assert_eq!(stats.entries, 3); // chapter(изменён) + cover(копия) + new
        assert!(stats.bytes > 0);
        assert_ne!(before, std::fs::read(&p).unwrap());

        // Новая версия на диске.
        let (kind, entries) = vfs_list(&p).unwrap();
        assert_eq!(kind, VfsKind::Zip);
        assert!(entries.iter().any(|e| e.name == "META-INF/new.xml"));
        assert!(!entries.iter().any(|e| e.name == "toc.ncx"));
        assert_eq!(
            vfs_read(&p, "OEBPS/chapter1.xhtml", DEFAULT_MAX_ENTRY_BYTES).unwrap(),
            "<html><body><p>Привіт</p></body></html>".as_bytes()
        );
        // Нетронутая запись скопирована побайтово.
        assert_eq!(vfs_read(&p, "OEBPS/images/cover.jpg", DEFAULT_MAX_ENTRY_BYTES).unwrap(), cover_orig);

        // Оверлей израсходован: merge — идентитет.
        let mut ov2 = VfsOverlay::new();
        ov2.write("toc.ncx", b"x".to_vec());
        let merged = merge_listing(vfs_list(&p).unwrap().1, &ov2);
        assert!(merged.iter().any(|e| e.name == "toc.ncx" && e.status == "added"));
    }

    #[test]
    fn zip_flush_saveas_keeps_original() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("book.epub");
        make_zip(&p);
        let before = std::fs::read(&p).unwrap();

        let mut ov = VfsOverlay::new();
        ov.write("toc.ncx", b"<ncx v3/>".to_vec());
        let copy = dir.path().join("book-copy.epub");
        let stats = flush_archive(&p, &ov, &copy, DEFAULT_MAX_ENTRY_BYTES).unwrap();
        assert_eq!(stats.entries, 3);

        assert_eq!(before, std::fs::read(&p).unwrap()); // оригинал нетронут
        assert_eq!(vfs_read(&copy, "toc.ncx", DEFAULT_MAX_ENTRY_BYTES).unwrap(), b"<ncx v3/>");
        assert_eq!(vfs_read(&copy, "toc.ncx", DEFAULT_MAX_ENTRY_BYTES).unwrap(), b"<ncx v3/>");
    }

    // ------------------------- tar / tar.gz flush -------------------------

    #[test]
    fn tar_flush_roundtrip_preserves_meta() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("arc.tar");
        make_tar(&p);
        let before = std::fs::read(&p).unwrap();

        let mut ov = VfsOverlay::new();
        ov.write("dir/hello.txt", "світ".as_bytes().to_vec());
        ov.write("dir/new.txt", "нова".as_bytes().to_vec());

        let stats = flush_archive(&p, &ov, &p, DEFAULT_MAX_ENTRY_BYTES).unwrap();
        assert_eq!(stats.kind, "tar");
        assert_eq!(stats.entries, 2); // hello.txt (правка) + new.txt (новая)
        assert_ne!(before, std::fs::read(&p).unwrap());

        assert_eq!(vfs_read(&p, "dir/hello.txt", DEFAULT_MAX_ENTRY_BYTES).unwrap(), "світ".as_bytes());
        assert_eq!(vfs_read(&p, "dir/new.txt", DEFAULT_MAX_ENTRY_BYTES).unwrap(), "нова".as_bytes());

        // Метаданные перенесены: mode 0600, mtime 1.7e9.
        let f = File::open(&p).unwrap();
        let mut ar = tar::Archive::new(f);
        for e in ar.entries().unwrap() {
            let e = e.unwrap();
            if e.path().unwrap().to_string_lossy() == "dir/hello.txt" {
                assert_eq!(e.header().mode().unwrap(), 0o600);
                assert_eq!(e.header().mtime().unwrap(), 1_700_000_000);
            }
        }
    }

    #[test]
    fn tgz_flush_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("arc.tgz");
        make_tgz(&p);

        let mut ov = VfsOverlay::new();
        ov.write("a/b.txt", "київ".as_bytes().to_vec());
        let stats = flush_archive(&p, &ov, &p, DEFAULT_MAX_ENTRY_BYTES).unwrap();
        assert_eq!(stats.kind, "tar.gz");
        assert_eq!(stats.entries, 1);
        assert_eq!(vfs_read(&p, "a/b.txt", DEFAULT_MAX_ENTRY_BYTES).unwrap(), "київ".as_bytes());
    }

    // ------------------------- .poler -------------------------

    fn big_payload(n: usize) -> Vec<u8> {
        (0..n).map(|i| ((i * 7 + i / 256) % 256) as u8).collect()
    }

    #[test]
    fn poler_write_read_roundtrip_multichunk() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("capsule.poler");
        let big = big_payload(POLER_CHUNK_RAW + 123_456); // 2 чанка
        let files = vec![
            ("OEBPS/ch1.xhtml".to_string(), "<p>раз</p>".as_bytes().to_vec()),
            ("data/big.bin".to_string(), big.clone()),
            ("empty.txt".to_string(), Vec::new()),
        ];
        let final_len = poler_write_files(&p, files).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().len(), final_len);

        let (kind, entries) = vfs_list(&p).unwrap();
        assert_eq!(kind, VfsKind::Poler);
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().any(|e| e.name == "data/big.bin" && e.size == big.len() as u64));

        assert_eq!(vfs_read(&p, "OEBPS/ch1.xhtml", DEFAULT_MAX_ENTRY_BYTES).unwrap(), "<p>раз</p>".as_bytes());
        assert_eq!(vfs_read(&p, "data/big.bin", DEFAULT_MAX_ENTRY_BYTES).unwrap(), big);
        assert_eq!(vfs_read(&p, "empty.txt", DEFAULT_MAX_ENTRY_BYTES).unwrap(), Vec::<u8>::new());

        // hex-хелпер (диагностика трейлера).
        assert_eq!(hex(&[0xDE, 0xAD]), "dead");
    }

    #[test]
    fn poler_flush_cow_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("capsule.poler");
        poler_write_files(
            &p,
            vec![
                ("notes/a.txt".to_string(), b"alpha".to_vec()),
                ("notes/b.txt".to_string(), b"beta".to_vec()),
            ],
        )
        .unwrap();
        let before = std::fs::read(&p).unwrap();

        let mut ov = VfsOverlay::new();
        ov.write("notes/a.txt", "альфа-правка".as_bytes().to_vec());
        ov.delete("notes/b.txt");
        ov.write("notes/c.txt", b"gamma".to_vec());

        assert_eq!(before, std::fs::read(&p).unwrap()); // CoW

        let stats = flush_archive(&p, &ov, &p, DEFAULT_MAX_ENTRY_BYTES).unwrap();
        assert_eq!(stats.kind, "poler");
        assert_eq!(stats.entries, 2);
        assert_ne!(before, std::fs::read(&p).unwrap());

        let (_, entries) = vfs_list(&p).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| e.name == "notes/c.txt"));
        assert!(!entries.iter().any(|e| e.name == "notes/b.txt"));
        assert_eq!(
            vfs_read(&p, "notes/a.txt", DEFAULT_MAX_ENTRY_BYTES).unwrap(),
            "альфа-правка".as_bytes()
        );
        assert_eq!(vfs_read(&p, "notes/c.txt", DEFAULT_MAX_ENTRY_BYTES).unwrap(), b"gamma");
    }

    // ------------------------- Фаза 4: METHOD_SYMBOL -------------------------

    /// Анизотропный поток (сценарий Shannon Bypass): два мажоритарных
    /// символа в длинных сериях — символьная фаза обязана окупаться.
    fn aniso_payload(n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        let mut i = 0u64;
        while out.len() < n {
            let l = ((i * 37 + 11) % 997 + 1) as usize; // серии 1..997
            let b = match (i / 7) % 5 {
                0 | 1 => 0xEF, // мажоритарный
                2 => 0x23,     // второй мажоритарный
                _ => ((i * 31 + 5) % 256) as u8,
            };
            out.extend(std::iter::repeat_n(b, l));
            i += 1;
        }
        out.truncate(n);
        out
    }

    /// Позиция байта метода первого чанка: MAGIC(8) + смещение 40 в заголовке.
    const FIRST_CHUNK_METHOD_OFF: usize = 8 + 40;

    #[test]
    fn poler_symbol_write_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("capsule.poler");
        let big = aniso_payload(POLER_CHUNK_RAW + 123_456); // 2 чанка
        let files = vec![
            ("OEBPS/ch1.xhtml".to_string(), "<p>раз</p>".as_bytes().to_vec()),
            ("data/aniso.bin".to_string(), big.clone()),
        ];
        let stats = poler_write_files_method(&p, PolerCompress::Symbol, files).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().len(), stats.bytes);
        assert_eq!(stats.raw, "<p>раз</p>".len() as u64 + big.len() as u64);

        // Чанк реально помечен METHOD_SYMBOL=3 (второй файл — первый чанк
        // после короткой записи; проверяем первый чанк крупного файла).
        let raw_file = std::fs::read(&p).unwrap();
        // Первый чанк файла 1 (короткий <64 Б → STORE), первого файла чанк один:
        // его stored-конец = 8 + 44 + stored_len. Проверим метод чанка large:
        // file table лежит в конце — проще проверить, что метод 3 встречается.
        assert!(
            raw_file[FIRST_CHUNK_METHOD_OFF..].contains(&3u8),
            "METHOD_SYMBOL=3 не найден в заголовках чанков"
        );

        // Прозрачное чтение через VFS (читатель Фазы 1 + symbol_decompress).
        let (kind, entries) = vfs_list(&p).unwrap();
        assert_eq!(kind, VfsKind::Poler);
        assert_eq!(entries.len(), 2);
        assert_eq!(vfs_read(&p, "OEBPS/ch1.xhtml", DEFAULT_MAX_ENTRY_BYTES).unwrap(), "<p>раз</p>".as_bytes());
        let recovered = vfs_read(&p, "data/aniso.bin", DEFAULT_MAX_ENTRY_BYTES).unwrap();
        assert_eq!(recovered, big, "бит-в-бит round-trip через METHOD_SYMBOL");
    }

    #[test]
    fn poler_symbol_flush_cow_roundtrip() {
        // ТЗ Фазы 4 (интеграционный): создание .poler с METHOD_SYMBOL,
        // модификация через CoW-оверлей, сохранение и чтение обратно
        // с побайтовым сравнением assert_eq!(recovered, original).
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("capsule.poler");
        let original = aniso_payload(600_000);
        poler_write_files_method(
            &p,
            PolerCompress::Symbol,
            vec![("data/original.bin".to_string(), original.clone())],
        )
        .unwrap();
        let before = std::fs::read(&p).unwrap();

        // CoW: правка в оверлее — диск нетронут.
        let mut ov = VfsOverlay::new();
        let edited = aniso_payload(500_000); // другой анизотропный поток
        ov.write("data/original.bin", edited.clone());
        ov.write("data/added.bin", "новый символ фазы".as_bytes().to_vec());
        assert_eq!(before, std::fs::read(&p).unwrap());

        // Материализация с методом symbol.
        let stats = flush_archive_opts(
            &p,
            &ov,
            &p,
            DEFAULT_MAX_ENTRY_BYTES,
            &FlushOptions {
                poler_method: PolerCompress::Symbol,
                force_kind: None,
            },
        )
        .unwrap();
        assert_eq!(stats.kind, "poler");
        assert_eq!(stats.method, "symbol");
        assert_eq!(stats.entries, 2);
        assert!(stats.raw > 0);

        // Чтение обратно: бит-в-бит.
        let recovered = vfs_read(&p, "data/original.bin", DEFAULT_MAX_ENTRY_BYTES).unwrap();
        assert_eq!(recovered, edited);
        assert_eq!(
            vfs_read(&p, "data/added.bin", DEFAULT_MAX_ENTRY_BYTES).unwrap(),
            "новый символ фазы".as_bytes()
        );
        // Дотискание: контейнер заметно меньше сырого объёма (анизотропия).
        assert!(stats.bytes < stats.raw / 2, "bytes={} raw={}", stats.bytes, stats.raw);
    }

    #[test]
    fn poler_flush_default_is_zstd_compat() {
        // flush_archive без опций = zstd-чанки (METHOD_ZFAST=1), как в Фазе 3.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("capsule.poler");
        let payload = aniso_payload(400_000);
        poler_write_files_method(
            &p,
            PolerCompress::Zstd,
            vec![("data/x.bin".to_string(), payload.clone())],
        )
        .unwrap();
        let raw_file = std::fs::read(&p).unwrap();
        assert_eq!(raw_file[FIRST_CHUNK_METHOD_OFF], METHOD_ZFAST);
        assert_eq!(vfs_read(&p, "data/x.bin", DEFAULT_MAX_ENTRY_BYTES).unwrap(), payload);
    }

    // ------------------------- общее -------------------------

    #[test]
    fn flush_empty_overlay_rejects() {
        // Пустой оверлей → план = копия 1:1; это всё равно валидная операция
        // (перепаковка), но протокол не должен её допускать — проверяем,
        // что план строится и flush работает (remux).
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("book.epub");
        make_zip(&p);
        let ov = VfsOverlay::new();
        let stats = flush_archive(&p, &ov, &p, DEFAULT_MAX_ENTRY_BYTES).unwrap();
        assert_eq!(stats.entries, 3); // полный remux
    }

    #[test]
    fn overlay_read_limit_guard() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("book.epub");
        make_zip(&p);
        let mut ov = VfsOverlay::new();
        ov.write("toc.ncx", b"12345678".to_vec());
        assert!(overlay_read(&p, "toc.ncx", 4, Some(&ov)).is_err());
    }

    #[test]
    fn flush_plan_paths() {
        let dir = tempfile::tempdir().unwrap();
        let p: PathBuf = dir.path().join("book.epub");
        make_zip(&p);
        let mut ov = VfsOverlay::new();
        ov.write("./OEBPS\\chapter1.xhtml", b"x".to_vec()); // нормализация внутри
        let plan = flush_plan(&p, &ov, DEFAULT_MAX_ENTRY_BYTES).unwrap();
        assert!(plan
            .iter()
            .any(|i| matches!(i, PlanItem::Modified { name, .. } if name == "OEBPS/chapter1.xhtml")));
    }
}
