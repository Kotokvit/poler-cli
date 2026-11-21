//! Потоковый инжест контейнеров (v0.62.0, IngestPipeline).
//!
//! Автоматическая распаковка на лету входных архивов (.zip, .tar.gz, .tar.zst, .gz, .zst)
//! с детекцией формата по сигнатурам (magic bytes). Распакованный поток данных льется
//! напрямую в FastCDC + BLAKE3 + ZSTD StreamWriter без промежуточного сохранения на диск.
//!
//! v0.65.0 (ансамбль):
//! * [`ingest_reader`] — тот же конвейер для ЛЮБОГО байтового источника
//!   (HTTP-стрим `--stream-download`, stdin): сниффинг магии первых ≤512 байт,
//!   декодирование gzip/zstd НА ЛЕТУ, границы tar-файлов — TarObserver'ом
//!   писателя. Сетевые исходники ложатся в .poler сразу с пофайловой
//!   таблицей и zstd-сжатием — сырой tar.gz на диск НЕ пишется вовсе.
//! * [`pack_dir`] — «постоянный архиватор сборки»: упаковка каталога
//!   в .poler с пофайловой таблицей (права 0o755 сохраняются — poler-box
//!   запускает бинарники прямо из архива).
//!
//! v0.91.0 CORPUS-TRIT:
//! * [`corpus_config`] — корпусной профиль плотности: CDC 128К/512К/1М,
//!   Deep zstd-19, батч-сжатие чанков на rayon (jobs). Формат НЕ меняется:
//!   уровень zstd не кодируется в .poler (кадр самодокументирован).
//! * [`corpus_pack`] — упаковка ТЕКСТОВОГО КОРПУСА (код + литература,
//!   сотни гигабайт) в один .poler с классификацией текстов по
//!   расширениям ([`CorpusClass`]): код/литература/данные/прочее.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use super::dedup::CdcParams;
use super::stream_writer::{CompressTier, StreamWriter, StreamWriteConfig, StreamWriteStats};
use crate::binfmt;

/// Режим инжеста входного потока.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
pub enum IngestMode {
    /// Автодетекция по сигнатурам (распаковка на лету при обнаружении контейнера).
    #[default]
    Auto,
    /// Прямой слепой поток (без распаковки).
    Off,
}


/// Распознанный внутренний формат входного потока/файла.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestKind {
    Zip,
    TarGz,
    TarZst,
    GzSingle,
    ZstSingle,
    TarRaw,
    /// v0.94.0 BINFMT: ELF (магия `\x7fELF`) — сегментация по секциям.
    Elf,
    /// v0.94.0 BINFMT: PE (MZ + PE\0\0 по e_lfanew) — сегментация
    /// по таблице секций.
    Pe,
    RawStream,
}

/// Детекция формата по сигнатуре первых байт.
///
/// v0.94.0 BINFMT: ELF/PE распознаются ДО проверки ustar — исполняемые
/// файлы сегодня режутся вслепую как RawStream, а по плану Phase 5++.C
/// обязаны попадать в сегментный путь («закон семантики» для бинарников).
pub fn sniff_magic(magic: &[u8]) -> IngestKind {
    if magic.len() >= 4 && &magic[0..4] == b"\x7fELF" {
        return IngestKind::Elf;
    }
    if magic.len() >= 0x40 && &magic[0..2] == b"MZ" {
        // e_lfanew (u32 LE @0x3C) → PE\0\0; в типовом PE укладывается
        // в первые 512 байт сниффинга
        let pe_off =
            u32::from_le_bytes([magic[0x3C], magic[0x3D], magic[0x3E], magic[0x3F]]) as usize;
        if pe_off >= 0x40 && pe_off + 4 <= magic.len() && &magic[pe_off..pe_off + 4] == b"PE\0\0" {
            return IngestKind::Pe;
        }
    }
    if magic.len() >= 4 && &magic[0..4] == b"PK\x03\x04" {
        return IngestKind::Zip;
    }
    if magic.len() >= 2 && magic[0] == 0x1f && magic[1] == 0x8b {
        return IngestKind::TarGz;
    }
    if magic.len() >= 4 && magic[0..4] == [0x28, 0xb5, 0x2f, 0xfd] {
        return IngestKind::TarZst;
    }
    if magic.len() >= 262 && &magic[257..262] == b"ustar" {
        return IngestKind::TarRaw;
    }
    IngestKind::RawStream
}

/// Потоковая упаковка файла в .poler с авто-распаковкой на лету.
pub fn ingest_file(
    src_path: &Path,
    out_path: &Path,
    cfg: StreamWriteConfig,
    mode: IngestMode,
) -> io::Result<StreamWriteStats> {
    let mut f = File::open(src_path)?;
    let file_len = f.metadata()?.len();

    if mode == IngestMode::Off || file_len < 4 {
        return super::stream_writer::write_stream(&mut f, out_path, cfg, &src_path.to_string_lossy());
    }

    let mut header = [0u8; 512];
    let n = f.read(&mut header)?;
    f.seek(SeekFrom::Start(0))?;

    match sniff_magic(&header[..n]) {
        // zip требует seek — только локальный файл.
        IngestKind::Zip => {
            let mut writer = StreamWriter::open(out_path, cfg)?;
            let mut archive = zip::ZipArchive::new(f)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("zip: {e}")))?;

            // Вливаем entries в tar-совместимый поток для сохранения файловой таблицы
            {
                let mut tar_builder = tar::Builder::new(TarWriterShim { writer: &mut writer });
                for i in 0..archive.len() {
                    let mut file = archive.by_index(i)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("zip entry: {e}")))?;
                    if file.is_dir() {
                        continue;
                    }
                    let mut header = tar::Header::new_gnu();
                    header.set_size(file.size());
                    header.set_mode(0o644);
                    header.set_cksum();
                    let name = file.name().to_string();
                    tar_builder.append_data(&mut header, name, &mut file)?;
                }
                tar_builder.finish()?;
            }
            let hint = src_path.file_name().unwrap_or_default().to_string_lossy().to_string();
            writer.finish(&hint)
        }
        // v0.94.0 BINFMT (Phase 5++.C): ELF/PE — формат-aware сегментация:
        // чанки не пересекают границ секций, записи `pe:.text`/
        // `elf:.rodata` в файловой таблице, трейлер — bit2 segmented_mode.
        // Аномалии формата — fail-closed на RawStream (как сегодня).
        IngestKind::Elf | IngestKind::Pe => {
            let hint = src_path.file_name().unwrap_or_default().to_string_lossy().to_string();
            ingest_binary(&mut f, file_len, out_path, cfg, &hint)
        }
        // tar.gz / tar.zst / gz / zst / raw tar / сырой поток — стриминговый путь.
        _ => {
            let hint = src_path.file_name().unwrap_or_default().to_string_lossy().to_string();
            ingest_reader(f, out_path, cfg, mode, &hint)
        }
    }
}

/// v0.94.0 BINFMT (Phase 5++.C): сегментная упаковка исполняемого файла
/// ELF/PE64 (глава 6.4 плана Phase 5++).
///
/// Декодер не нужен (в отличие от gzip/zstd): заголовки читаются
/// прицельными чтениями (никакого целого файла в RAM — дисциплина
/// ≥256-КиБ-буферов сохранена), сегменты подаются поэлементно через
/// [`StreamWriter::push_segment`]. Полное покрытие проверяется ДО
/// подачи: любая аномалия формата (перекрытия секций, выход за
/// пределы файла, битые заголовки) — fail-closed откат на слепой
/// RawStream-путь `write_stream`, как сегодня: упаковка не прерывается.
fn ingest_binary(
    f: &mut File,
    file_len: u64,
    out_path: &Path,
    cfg: StreamWriteConfig,
    name_hint: &str,
) -> io::Result<StreamWriteStats> {
    let segmentation = match binfmt_segments(f, file_len) {
        Ok(s) => s,
        Err(_anomaly) => {
            // fail-closed: слепой поток, как до Phase 5++.C
            f.seek(SeekFrom::Start(0))?;
            return super::stream_writer::write_stream(f, out_path, cfg, name_hint);
        }
    };
    let mut writer = StreamWriter::open(out_path, cfg)?;
    for seg in &segmentation.segments {
        f.seek(SeekFrom::Start(seg.file_off))?;
        let mut range = (&*f).take(seg.file_len);
        writer.push_segment(&mut range, seg, segmentation.prefix)?;
    }
    writer.finish(name_hint)
}

/// Сегментация открытого файла: ELF — прицельные чтения заголовков,
/// PE — чтение головной части до конца таблицы секций.
fn binfmt_segments(f: &mut File, file_len: u64) -> Result<binfmt::Segmentation, String> {
    let mut ident = [0u8; 4];
    f.seek(SeekFrom::Start(0)).ok();
    f.read_exact(&mut ident).map_err(|e| format!("head: {e}"))?;
    if &ident == b"\x7fELF" {
        return binfmt::elf_segments(f, file_len);
    }
    // PE: голова обязана вместить таблицу секций (типично < 1 КиБ);
    // e_lfanew + nsec известны из первых 0x200 — дочитываем при нужде
    let mut head = vec![0u8; 0x200];
    let n = f
        .seek(SeekFrom::Start(0))
        .and_then(|_| f.read(&mut head))
        .map_err(|e| format!("head: {e}"))?;
    head.truncate(n);
    if head.len() >= 0x40 && &head[0..2] == b"MZ" {
        let pe_off = u32::from_le_bytes([head[0x3C], head[0x3D], head[0x3E], head[0x3F]]) as usize;
        if pe_off + 24 <= head.len() {
            let nsec = u16::from_le_bytes([head[pe_off + 6], head[pe_off + 7]]) as usize;
            let size_of_opt =
                u16::from_le_bytes([head[pe_off + 20], head[pe_off + 21]]) as usize;
            let need = pe_off + 24 + size_of_opt + nsec * 40;
            if need > head.len() && need <= file_len as usize {
                let mut more = vec![0u8; need - head.len()];
                f.read_exact(&mut more).map_err(|e| format!("sec table: {e}"))?;
                head.extend_from_slice(&more);
            }
        }
    }
    binfmt::pe_segments(&head, file_len)
}

/// Потоковый инжест ЛЮБОГО байтового источника (v0.65.0): HTTP-стрим
/// `--stream-download`, stdin, сокет. Сниффинг магии первых ≤512 байт →
/// декодер gzip/zstd НА ЛЕТУ → StreamWriter (TarObserver сам разводит
/// пофайловую таблицу tar-потока). zip по потоку НЕ распаковывается
/// (требует seek) — сохраняется как есть одним entry.
///
/// RAM-дисциплина: буферы 256 КиБ, MultiGzDecoder/Decoder держат
/// только свои окна; сырые байты на диск не попадают НИКОГДА.
pub fn ingest_reader<R: Read>(
    src: R,
    out_path: &Path,
    cfg: StreamWriteConfig,
    mode: IngestMode,
    name_hint: &str,
) -> io::Result<StreamWriteStats> {
    let mut src = src;
    let mut head = [0u8; 512];
    let mut n = 0usize;
    while n < head.len() {
        let r = src.read(&mut head[n..])?;
        if r == 0 {
            break;
        }
        n += r;
    }
    let kind = if mode == IngestMode::Off || n < 4 {
        IngestKind::RawStream
    } else {
        sniff_magic(&head[..n])
    };
    let head_vec = head[..n].to_vec();

    match kind {
        IngestKind::TarGz | IngestKind::GzSingle => {
            let mut writer = StreamWriter::open(out_path, cfg)?;
            let chained = io::Cursor::new(head_vec).chain(src);
            let gz = flate2::read::MultiGzDecoder::new(chained);
            let mut reader = std::io::BufReader::with_capacity(256 * 1024, gz);
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                writer.push_bytes(&buf[..n])?;
            }
            writer.finish(name_hint)
        }
        IngestKind::TarZst | IngestKind::ZstSingle => {
            let mut writer = StreamWriter::open(out_path, cfg)?;
            let chained = io::Cursor::new(head_vec).chain(src);
            let zst = zstd::stream::read::Decoder::new(chained)?;
            let mut reader = std::io::BufReader::with_capacity(256 * 1024, zst);
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                writer.push_bytes(&buf[..n])?;
            }
            writer.finish(name_hint)
        }
        // Raw tar / сырой поток / zip-по-стриму: TarObserver сам определяет
        // tar; не-tar поток станет одиночной записью name_hint.
        _ => {
            let mut writer = StreamWriter::open(out_path, cfg)?;
            writer.push_bytes(&head_vec)?;
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let n = src.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                writer.push_bytes(&buf[..n])?;
            }
            writer.finish(name_hint)
        }
    }
}

// ────────────────────────── v0.91.0 CORPUS-TRIT ──────────────────────────

/// Класс текста корпуса (по расширению имени; статистика обхода —
/// бинарность содержимого дополнительно решает NUL-снифф кристалла
/// `--archive-to-crystal` и семантического слоя, здесь — только отчёт).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorpusClass {
    /// Программный код.
    Code,
    /// Литература/проза (markdown, книги, субтитры, логи прозы).
    Prose,
    /// Структурированные данные/разметка (json, yaml, csv, html…).
    Data,
    /// Прочее (бинарное/неизвестное).
    Other,
}

impl CorpusClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            CorpusClass::Code => "code",
            CorpusClass::Prose => "prose",
            CorpusClass::Data => "data",
            CorpusClass::Other => "other",
        }
    }

    /// Классификация по расширению (без точки, lowercase; пустое — Other).
    pub fn of_ext(ext: &str) -> CorpusClass {
        match ext {
            "rs" | "py" | "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "js" | "mjs" | "cjs"
            | "jsx" | "ts" | "tsx" | "java" | "kt" | "kts" | "go" | "rb" | "php" | "cs"
            | "swift" | "zig" | "scala" | "sc" | "sh" | "bash" | "zsh" | "fish" | "sql"
            | "pl" | "pm" | "lua" | "r" | "jl" | "dart" | "ex" | "exs" | "erl" | "hs"
            | "ml" | "mli" | "fs" | "f90" | "f95" | "asm" | "s" | "svelte" | "vue"
            | "awk" | "ps1" | "bat" | "cmd" | "v" | "cr" | "groovy" | "gradle" => {
                CorpusClass::Code
            }
            "md" | "markdown" | "mdown" | "txt" | "rst" | "tex" | "org" | "adoc"
            | "asciidoc" | "rtf" | "epub" | "mobi" | "fb2" | "djvu" | "srt" | "vtt"
            | "nfo" | "log" | "letter" | "doc" | "docx" | "pdf" => CorpusClass::Prose,
            "json" | "yaml" | "yml" | "toml" | "xml" | "csv" | "tsv" | "ini" | "cfg"
            | "conf" | "html" | "htm" | "css" | "scss" | "less" | "svg" | "geojson"
            | "ndjson" | "parquet" | "db" | "sqlite" | "properties" => {
                CorpusClass::Data
            }
            _ => CorpusClass::Other,
        }
    }
}

/// Статистика классов корпуса (собирается на обходе БЕЗ чтения тел).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CorpusClassStats {
    pub code_files: u64,
    pub code_bytes: u64,
    pub prose_files: u64,
    pub prose_bytes: u64,
    pub data_files: u64,
    pub data_bytes: u64,
    pub other_files: u64,
    pub other_bytes: u64,
    /// Топ расширений по байтам: [ext, files, bytes].
    pub top_ext: Vec<(String, u64, u64)>,
}

impl CorpusClassStats {
    /// Учесть файл класса (используется пакером и --poler-stats).
    pub fn bump(&mut self, class: CorpusClass, bytes: u64) {
        match class {
            CorpusClass::Code => {
                self.code_files += 1;
                self.code_bytes += bytes;
            }
            CorpusClass::Prose => {
                self.prose_files += 1;
                self.prose_bytes += bytes;
            }
            CorpusClass::Data => {
                self.data_files += 1;
                self.data_bytes += bytes;
            }
            CorpusClass::Other => {
                self.other_files += 1;
                self.other_bytes += bytes;
            }
        }
    }
}

/// Корпусной профиль плотности (v0.91.0 CORPUS-TRIT):
/// * CDC 128 КиБ / 512 КиБ / 1 МиБ — крупнее стримингового дефолта
///   (64К/256К/1М): длиннее окно zstd на чанк, выше плотность текста;
///   дедуп остаётся точным — границы по содержимому, повторяющиеся
///   тома/клоны кода схлопываются чанково;
/// * Deep zstd-19 (формат не меняется: уровень не кодируется в .poler,
///   кадр zstd самодокументирован — любой читатель v0.39+ читает);
/// * jobs≥2 — батч-сжатие чанков на rayon: 150 ГБ корпуса упаковываются
///   за часы, а не за ночь.
pub fn corpus_config(jobs: usize) -> StreamWriteConfig {
    StreamWriteConfig {
        cdc: CdcParams::new(128 * 1024, 512 * 1024, 1024 * 1024),
        dedup: true,
        tier: CompressTier::Deep,
        read_chunk: 1024 * 1024,
        progress_bytes: 1024 * 1024 * 1024,
        jobs,
        deep_level: 19,
    }
}

/// «Постоянный архиватор сборки» (v0.65.0): упаковка каталога в .poler
/// с ПОФАЙЛОВОЙ таблицей. Обход как у grep-слоя (respect .gitignore,
/// скрытые по умолчанию пропускаются); файлы льются через tar-обёртку —
/// TarObserver писателя разводит границы файлов, SHA256 каждой записи
/// и BLAKE3-дедуп МЕЖДУ файлами достаются бесплатно. Права (0o755)
/// сохраняются в tar-заголовках — poler-box запускает бинарники прямо
/// из архива (циклический ансамбль: движок в коробке из своего .poler).
pub fn pack_dir(
    root: &Path,
    out_path: &Path,
    cfg: StreamWriteConfig,
    include_hidden: bool,
    respect_ignore: bool,
) -> io::Result<StreamWriteStats> {
    pack_dir_inner(root, out_path, cfg, include_hidden, respect_ignore, false).map(|(s, _)| s)
}

/// v0.91.0 CORPUS-TRIT: упаковка ТЕКСТОВОГО КОРПУСА (код + литература)
/// в один .poler — те же механизмы, что [`pack_dir`] (пофайловая таблица,
/// BLAKE3-дедуп между файлами, zstd), плюс статистика классов текстов
/// для отчёта. Классификация — по расширениям на обходе, тела файлов
/// для неё не читаются. Профиль плотности задаёт вызывающий
/// ([`corpus_config`]).
pub fn corpus_pack(
    root: &Path,
    out_path: &Path,
    cfg: StreamWriteConfig,
    include_hidden: bool,
    respect_ignore: bool,
) -> io::Result<(StreamWriteStats, CorpusClassStats)> {
    pack_dir_inner(root, out_path, cfg, include_hidden, respect_ignore, true)
}

/// Общая реализация [`pack_dir`]/[`corpus_pack`]; `classify` включает
/// сбор статистики классов и гистограммы расширений.
fn pack_dir_inner(
    root: &Path,
    out_path: &Path,
    cfg: StreamWriteConfig,
    include_hidden: bool,
    respect_ignore: bool,
    classify: bool,
) -> io::Result<(StreamWriteStats, CorpusClassStats)> {
    use ignore::WalkBuilder;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    // Грабля Сессии-12: выходной архив, писавшийся ВНУТРИ упаковываемого
    // каталога, попадал в обход — пакуется растущий сам-себя .part и
    // sidecar таблицы, реальные файлы теряются молча. Пропускаем вывод
    // и его sidecar-файлы (out, out.part, out.part.files, out.files).
    let self_outputs: Vec<std::path::PathBuf> = {
        let abs = |p: &Path| -> std::path::PathBuf {
            if let Ok(c) = std::fs::canonicalize(p) {
                return c;
            }
            // Файла ещё нет (например, финальный .poler до rename) —
            // лексическая нормализация от текущего каталога.
            let joined = if p.is_absolute() {
                p.to_path_buf()
            } else {
                std::env::current_dir().unwrap_or_default().join(p)
            };
            let mut pb = std::path::PathBuf::new();
            for c in joined.components() {
                match c {
                    std::path::Component::CurDir => {}
                    std::path::Component::ParentDir => {
                        pb.pop();
                    }
                    other => {
                        pb.push(other.as_os_str());
                    }
                }
            }
            pb
        };
        let mut suffixes = vec![std::path::PathBuf::new()];
        suffixes.push(".part".into());
        suffixes.push(".part.files".into());
        suffixes.push(".files".into());
        let mut v = Vec::new();
        for sfx in &suffixes {
            let mut p = out_path.as_os_str().to_os_string();
            p.push(sfx.as_os_str());
            v.push(abs(Path::new(&p)));
        }
        v
    };

    let mut writer = StreamWriter::open(out_path, cfg)?;
    let mut classes = CorpusClassStats::default();
    let mut ext_hist: HashMap<String, (u64, u64)> = HashMap::new();
    {
        let mut tar_builder = tar::Builder::new(TarWriterShim { writer: &mut writer });
        let walker = WalkBuilder::new(root)
            .hidden(!include_hidden)
            .git_ignore(respect_ignore)
            .git_global(respect_ignore)
            .git_exclude(respect_ignore)
            .ignore(respect_ignore)
            .require_git(false)
            .build();
        for entry in walker.filter_map(|e| e.ok()) {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            // Свой вывод не пакуем (self-reference — см. граблю выше).
            let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            if self_outputs.iter().any(|p| *p == canon) {
                continue;
            }
            let rel = path.strip_prefix(root).unwrap_or(path);
            let name = rel.to_string_lossy().replace('\\', "/");
            let meta = std::fs::metadata(path)?;
            if classify {
                // v0.91.0: класс текста по расширению — статистика
                // корпуса без чтения тел (бинарность решает NUL-снифф
                // кристалла/семантики, здесь — только отчёт).
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_lowercase())
                    .unwrap_or_default();
                classes.bump(CorpusClass::of_ext(&ext), meta.len());
                let key = if ext.is_empty() { "(none)".to_string() } else { ext };
                let slot = ext_hist.entry(key).or_insert((0, 0));
                slot.0 += 1;
                slot.1 += meta.len();
            }
            let mut f = File::open(path)?;
            let mut header = tar::Header::new_gnu();
            header.set_size(meta.len());
            #[cfg(unix)]
            header.set_mode(meta.permissions().mode() & 0o7777);
            #[cfg(not(unix))]
            header.set_mode(0o644);
            header.set_cksum();
            tar_builder.append_data(&mut header, name, &mut f)?;
        }
        tar_builder.finish()?;
    }
    let hint = root
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let stats = writer.finish(&hint)?;
    if classify {
        let mut top: Vec<(String, u64, u64)> =
            ext_hist.into_iter().map(|(k, (f, b))| (k, f, b)).collect();
        top.sort_by(|a, b| b.1.cmp(&a.1));
        top.truncate(16);
        classes.top_ext = top;
    }
    Ok((stats, classes))
}

struct TarWriterShim<'a> {
    writer: &'a mut StreamWriter,
}

impl<'a> io::Write for TarWriterShim<'a> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.push_bytes(buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ────────────────────────── тесты (v0.65.0 ансамбль) ──────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::reader::PolerReader;
    use std::io::Write;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("poler-ing-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.join(name)
    }

    fn cfg() -> StreamWriteConfig {
        StreamWriteConfig { progress_bytes: 0, ..Default::default() }
    }

    // ── v0.94.0 BINFMT (Phase 5++.C): сегментация ELF/PE64 ──

    const SEVEN_ZA: &[u8] = include_bytes!("../../tests/fixtures/pe/7za.exe");
    const LIBSSP: &[u8] = include_bytes!("../../tests/fixtures/pe/libssp-0.dll");

    /// Боевой PE64 (7za.exe, 1.2 МБ): segmented_mode=true, пофайловая
    /// таблица записей pe:*, бит-в-бит round-trip, verify зелёный —
    /// приёмка 6.6 плана Phase 5++.
    #[test]
    fn ingest_file_pe_roundtrip_bitexact() {
        let src = tmp("7za.exe");
        std::fs::write(&src, SEVEN_ZA).unwrap();
        let p = tmp("7za.poler");
        let stats = ingest_file(&src, &p, cfg(), IngestMode::Auto).unwrap();
        assert!(stats.segmented_mode, "сегментный путь не включился: {stats:?}");
        assert!(!stats.tar_mode, "бинарник — не tar");
        assert!(stats.files >= 8, "записей-сегментов мало: {}", stats.files);
        assert!(!stats.segments.is_empty());

        let r = PolerReader::open(&p).unwrap();
        let names: Vec<&str> = r.files().iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"pe:headers"), "нет pe:headers: {names:?}");
        assert!(names.contains(&"pe:.text"), "нет pe:.text: {names:?}");
        assert!(names.contains(&"pe:.rdata"), "нет pe:.rdata: {names:?}");
        let rep = r.verify().unwrap();
        assert!(rep.all_ok, "verify: {:?}", rep.files_bad);
        assert!(rep.stream_ok, "сквозной дайджест: {:?}", rep);

        // бит-в-бит round-trip всего образа
        let mut out = Vec::new();
        let n = r.stream_out(&mut out).unwrap();
        assert_eq!(n, SEVEN_ZA.len() as u64);
        assert_eq!(out, SEVEN_ZA, "образ обязан восстанавливаться бит-в-бит");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&src);
    }

    /// DLL с EAT (libssp-0.dll): тот же инвариант на второй фикстуре.
    #[test]
    fn ingest_file_pe_dll_roundtrip() {
        let src = tmp("libssp.dll");
        std::fs::write(&src, LIBSSP).unwrap();
        let p = tmp("libssp.poler");
        let stats = ingest_file(&src, &p, cfg(), IngestMode::Auto).unwrap();
        assert!(stats.segmented_mode, "{stats:?}");
        let r = PolerReader::open(&p).unwrap();
        let rep = r.verify().unwrap();
        assert!(rep.all_ok && rep.stream_ok, "{rep:?}");
        let mut out = Vec::new();
        r.stream_out(&mut out).unwrap();
        assert_eq!(out, LIBSSP);
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&src);
    }

    /// Синтетический ELF64: записи elf:.text / elf:.rodata, виртуальная
    /// зона .bss — записью нулевой длины, round-trip бит-в-бит.
    #[test]
    fn ingest_file_elf_roundtrip() {
        let img = crate::binfmt::tests::synth_elf64(300 * 1024, 64 * 1024);
        let src = tmp("synth.elf");
        std::fs::write(&src, &img).unwrap();
        let p = tmp("synth.poler");
        let stats = ingest_file(&src, &p, cfg(), IngestMode::Auto).unwrap();
        assert!(stats.segmented_mode, "{stats:?}");
        let r = PolerReader::open(&p).unwrap();
        let names: Vec<&str> = r.files().iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"elf:.text"), "{names:?}");
        assert!(names.contains(&"elf:.rodata"), "{names:?}");
        assert!(names.contains(&"elf:.data"), "{names:?}");
        assert!(names.contains(&"elf:.bss"), "виртуальная зона — в таблице: {names:?}");
        let bss = r.find_file("elf:.bss").unwrap();
        assert_eq!(bss.raw_len, 0, "у .bss нет файловых байтов");
        let rep = r.verify().unwrap();
        assert!(rep.all_ok && rep.stream_ok, "{rep:?}");
        let mut out = Vec::new();
        r.stream_out(&mut out).unwrap();
        assert_eq!(out, img, "бит-в-бит");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&src);
    }

    /// Fail-closed (глава 6.4/8 плана): аномальный PE (секция за
    /// пределами файла) НЕ прерывает упаковку — откат на RawStream,
    /// round-trip всё равно бит-в-бит.
    #[test]
    fn ingest_file_pe_anomaly_falls_back_to_rawstream() {
        let mut img = crate::binfmt::tests::synth_pe(64 * 1024, 16 * 1024);
        // ломаем: raw_size .text растягиваем за пределы файла
        let pe_off = u32::from_le_bytes([img[0x3C], img[0x3D], img[0x3E], img[0x3F]]) as usize;
        let size_of_opt = u16::from_le_bytes([img[pe_off + 20], img[pe_off + 21]]) as usize;
        let sec_off = pe_off + 24 + size_of_opt;
        let text_raw_size = sec_off + 16; // поле raw_size .text
        let bad = u32::from_le_bytes([
            img[text_raw_size],
            img[text_raw_size + 1],
            img[text_raw_size + 2],
            img[text_raw_size + 3],
        ]) + 0x100000;
        img[text_raw_size..text_raw_size + 4].copy_from_slice(&bad.to_le_bytes());

        let src = tmp("broken.exe");
        std::fs::write(&src, &img).unwrap();
        let p = tmp("broken.poler");
        let stats = ingest_file(&src, &p, cfg(), IngestMode::Auto).unwrap();
        assert!(!stats.segmented_mode, "аномалия обязана уйти в RawStream: {stats:?}");
        assert_eq!(stats.files, 1);
        let r = PolerReader::open(&p).unwrap();
        let rep = r.verify().unwrap();
        assert!(rep.all_ok && rep.stream_ok, "{rep:?}");
        let mut out = Vec::new();
        r.stream_out(&mut out).unwrap();
        assert_eq!(out, img, "даже битый образ не теряет байтов");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&src);
    }

    /// Дедуп соседних билдов (приёмка 6.6): рост .data сдвигает .reloc,
    /// но границы чанков .text привязаны к началу сегмента — при изменении
    /// только .data ≥ 90% чанков .text переиспользуются (BLAKE3-хиты).
    #[test]
    fn segmented_dedup_between_builds() {
        use std::io::Cursor;
        // билд A и билд B: .text идентичен, .data вырос втрое
        let a = crate::binfmt::tests::synth_pe(512 * 1024, 32 * 1024);
        let b = crate::binfmt::tests::synth_pe(512 * 1024, 96 * 1024);
        assert_ne!(a.len(), b.len(), "билды обязаны отличаться размером");

        let p = tmp("builds.poler");
        let mut w = StreamWriter::open(&p, cfg()).unwrap();
        for img in [&a, &b] {
            let seg = binfmt::pe_segments(img, img.len() as u64).unwrap();
            for s in &seg.segments {
                let mut cur = Cursor::new(
                    &img[s.file_off as usize..(s.file_off + s.file_len) as usize],
                );
                w.push_segment(&mut cur, s, seg.prefix).unwrap();
            }
        }
        let stats = w.finish("builds").unwrap();
        assert!(stats.segmented_mode, "{stats:?}");

        // .text билда B — последний сегмент с именем pe:.text
        let text_b = stats.segments.iter().rev().find(|s| s.name == "pe:.text").unwrap();
        assert!(text_b.chunks >= 4, "слишком мало чанков для вывода: {text_b:?}");
        let reuse = text_b.dedup_hits as f64 / text_b.chunks as f64;
        assert!(
            reuse >= 0.9,
            "переиспользование .text {reuse:.2} < 0.90 (чанков {}, хитов {})",
            text_b.chunks,
            text_b.dedup_hits
        );
        // .data билда B отличается — обязан быть миссом
        let data_b = stats.segments.iter().rev().find(|s| s.name == "pe:.data").unwrap();
        assert_eq!(data_b.dedup_hits, 0, "разный .data не может дедупиться: {data_b:?}");

        // поток = билд A || билд B, оба образа восстанавливаются
        let r = PolerReader::open(&p).unwrap();
        let rep = r.verify().unwrap();
        assert!(rep.all_ok && rep.stream_ok, "{rep:?}");
        let mut out = Vec::new();
        r.stream_out(&mut out).unwrap();
        assert_eq!(out.len(), a.len() + b.len());
        assert_eq!(&out[..a.len()], &a[..]);
        assert_eq!(&out[a.len()..], &b[..]);
        let _ = std::fs::remove_file(&p);
    }

    /// Сниффинг: ELF и PE распознаются до ustar (глава 6.4).
    #[test]
    fn sniff_magic_elf_pe() {
        assert_eq!(sniff_magic(b"\x7fELF\x02\x01\x01\x00"), IngestKind::Elf);
        let mut mz = [0u8; 0x100];
        mz[0] = b'M';
        mz[1] = b'Z';
        mz[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        mz[0x80..0x84].copy_from_slice(b"PE\0\0");
        assert_eq!(sniff_magic(&mz), IngestKind::Pe);
        // tar не пострадал
        let mut tar_head = vec![0u8; 512];
        tar_head[257..262].copy_from_slice(b"ustar");
        assert_eq!(sniff_magic(&tar_head), IngestKind::TarRaw);
        assert_eq!(sniff_magic(b"PK\x03\x04xxxx"), IngestKind::Zip);
        assert_eq!(sniff_magic(b"random bytes.."), IngestKind::RawStream);
    }

    /// Инвариант покрытия снаружи: конкатенация записей pe:* в порядке
    /// raw_off восстанавливает образ (приёмка 6.6 — archive-list).
    #[test]
    fn segmented_table_reassembles_image() {
        let src = tmp("7za2.exe");
        std::fs::write(&src, SEVEN_ZA).unwrap();
        let p = tmp("7za2.poler");
        ingest_file(&src, &p, cfg(), IngestMode::Auto).unwrap();
        let r = PolerReader::open(&p).unwrap();
        let mut files: Vec<&crate::archive::reader::PolerFile> =
            r.files().iter().filter(|f| f.name.starts_with("pe:")).collect();
        files.sort_by_key(|f| f.raw_off);
        let mut rebuilt = Vec::new();
        let mut part = Vec::new();
        let mut pos = 0u64;
        for f in &files {
            assert_eq!(f.raw_off, pos, "записи обязаны идти подряд: {}", f.name);
            r.read_range(f.raw_off, f.raw_len as usize, &mut part).unwrap();
            rebuilt.extend_from_slice(&part);
            pos += f.raw_len;
        }
        assert_eq!(pos, SEVEN_ZA.len() as u64);
        assert_eq!(rebuilt, SEVEN_ZA);
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&src);
    }

    /// tar.gz в памяти: три файла, вложенные каталоги. Возвращает
    /// (gzip-байты, размер РАСПАКОВАННОГО tar) — экономия диска
    /// меряется против распакованного, а не против gz.
    fn tar_gz_bytes() -> (Vec<u8>, usize) {
        let mut tar_bytes = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_bytes);
            b.append_data(&mut header_for(20), "geo/src/a.txt", b"alpha trait CoordNum".as_slice()).unwrap();
            b.append_data(&mut header_for(22), "geo/src/b.txt", b"beta pub fn intersects".as_slice()).unwrap();
            // 150 КиБ СЖИМАЕМОГО исходника (повторяющиеся строки Rust-кода)
            let line = b"pub fn intersects(p: &Point<C>, q: &Point<C>, out: &mut Vec<Coord>) -> bool {\n";
            let big: Vec<u8> = line.repeat(3600);
            b.append_data(&mut header_for(big.len() as u64), "geo/src/geometry/c.rs", big.as_slice()).unwrap();
            b.finish().unwrap();
        }
        let tar_len = tar_bytes.len();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_bytes).unwrap();
        (gz.finish().unwrap(), tar_len)
    }

    fn header_for(data_len: u64) -> tar::Header {
        let mut h = tar::Header::new_gnu();
        h.set_size(data_len);
        h.set_mode(0o644);
        h.set_cksum();
        h
    }

    /// Сетевой путь (--stream-download): gzip-стрим раскладывается в
    /// .poler с ПОФАЙЛОВОЙ таблицей и сжатием, сырой tar.gz не материализуется.
    #[test]
    fn ingest_reader_gz_stream_per_file_table() {
        let (gz, tar_len) = tar_gz_bytes();
        let p = tmp("net_gz.poler");
        let stats = ingest_reader(
            io::Cursor::new(&gz),
            &p,
            cfg(),
            IngestMode::Auto,
            "geo.tar.gz",
        )
        .unwrap();

        assert!(stats.tar_mode, "tar распознан на лету: {stats:?}");
        assert_eq!(stats.files, 3, "пофайловая таблица из tar-стрима");
        assert!(
            stats.total_stored < tar_len as u64,
            "экономия диска: .poler={} < распакованный tar={} (gz={})",
            stats.total_stored,
            tar_len,
            gz.len()
        );

        let r = PolerReader::open(&p).unwrap();
        let names: Vec<&str> = r.files().iter().map(|f| f.name.as_str()).collect();
        for want in ["geo/src/a.txt", "geo/src/b.txt", "geo/src/geometry/c.rs"] {
            assert!(names.contains(&want), "нет {want}: {names:?}");
        }
        // содержимое записи a.txt — байт-в-байт
        let f = r.find_file("geo/src/a.txt").unwrap();
        let mut out = Vec::new();
        r.read_range(f.raw_off, f.raw_len as usize, &mut out).unwrap();
        assert_eq!(out, b"alpha trait CoordNum".as_slice());

        let rep = r.verify().unwrap();
        assert!(rep.all_ok, "verify: {:?}", rep.files_bad);
    }

    /// Сырой не-тар поток → одиночная запись с именем-подсказкой.
    #[test]
    fn ingest_reader_raw_single_entry() {
        let p = tmp("raw.poler");
        let data: Vec<u8> = (0..100_000u32).map(|i| (i * 7 % 253) as u8).collect();
        let stats = ingest_reader(
            io::Cursor::new(&data),
            &p,
            cfg(),
            IngestMode::Auto,
            "blob.bin",
        )
        .unwrap();
        assert!(!stats.tar_mode);
        assert_eq!(stats.files, 1);
        let r = PolerReader::open(&p).unwrap();
        assert_eq!(r.files()[0].name, "blob.bin");
        let mut out = Vec::new();
        let n = r.stream_out(&mut out).unwrap();
        assert_eq!(n, data.len() as u64);
        assert_eq!(out, data, "lossless");
    }

    /// Постоянный архиватор: каталог → .poler с пофайловой таблицей,
    /// права 0o755 сохранены, verify зелёный.
    #[test]
    fn pack_dir_roundtrip() {
        let d = std::env::temp_dir().join(format!("poler-pack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("bin")).unwrap();
        std::fs::create_dir_all(d.join("viz")).unwrap();
        std::fs::write(d.join("bin/poler-engine"), b"ELF-payload-not-real").unwrap();
        std::fs::write(d.join("viz/scene.svg"), b"<svg>hello</svg>").unwrap();
        std::fs::write(d.join("README.md"), b"# build artifacts").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                d.join("bin/poler-engine"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }

        let p = tmp("build.poler");
        let stats = pack_dir(&d, &p, cfg(), false, false).unwrap();
        assert!(stats.tar_mode);
        assert_eq!(stats.files, 3, "файлы каталога в таблице");

        let r = PolerReader::open(&p).unwrap();
        assert!(r.find_file("bin/poler-engine").is_some());
        assert!(r.find_file("viz/scene.svg").is_some());
        assert!(r.find_file("README.md").is_some());
        let rep = r.verify().unwrap();
        assert!(rep.all_ok, "verify: {:?}", rep.files_bad);

        // extract_all восстанавливает дерево
        let out_dir = tmp("build_unpacked");
        let _ = std::fs::remove_dir_all(&out_dir);
        r.extract_all(&out_dir).unwrap();
        assert_eq!(
            std::fs::read(out_dir.join("bin/poler-engine")).unwrap(),
            b"ELF-payload-not-real"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Грабля Сессии-12: выход .poler ВНУТРИ упаковываемого каталога.
    /// Раньше растущий .part + sidecar попадали в обход, а реальные
    /// файлы терялись молча (архив «сам-себя»). Теперь вывод
    /// и его sidecar-файлы исключаются из обхода.
    #[test]
    fn pack_dir_output_inside_tree_not_packed() {
        let d = std::env::temp_dir().join(format!(
            "poler-selfref-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("first.txt"), b"AAA").unwrap();
        std::fs::write(d.join("second.txt"), b"BBB").unwrap();

        // Вывод — в тот же каталог, который пакуем.
        let out = d.join("same_dir.poler");
        let stats = pack_dir(&d, &out, cfg(), false, false).unwrap();
        assert_eq!(stats.files, 2, "только реальные файлы, без self-reference");

        let r = PolerReader::open(&out).unwrap();
        let names: Vec<&str> = r.files().iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"first.txt"), "{names:?}");
        assert!(names.contains(&"second.txt"), "{names:?}");
        for name in &names {
            assert!(
                !name.contains("same_dir.poler"),
                "свой вывод в архиве: {name}"
            );
        }
        let rep = r.verify().unwrap();
        assert!(rep.all_ok, "verify: {:?}", rep.files_bad);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// v0.91.0 CORPUS-TRIT: корпус → .poler (батч-сжатие jobs=2),
    /// классификация код/литература/данные, дедуп дубля файла,
    /// lossless-verify, извлечение байт-в-байт.
    ///
    /// ГРАБЛЯ, пойманная этим тестом: дедуп ВНУТРИ батча терялся
    /// (фаза планов делала все lookup'ы до вставки хешей в реестр —
    /// дубли одной пачки не схлопывались). Вторая грабля: идеально
    /// ПЕРИОДИЧЕСКИЙ контент без энтропии не даёт CDC-триггеров —
    /// границы жёстко по лимиту, копии не ресинхронизируются; поэтому
    /// тестовый «код» энтропичен (счётчики в строках), как реальный.
    #[test]
    fn corpus_pack_roundtrip_and_classes() {
        let d = std::env::temp_dir().join(format!("poler-corpus-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::create_dir_all(d.join("docs")).unwrap();
        // ~160 КиБ «кода»: строки различны (энтропия → CDC-триггеры)
        let mut code: Vec<u8> = Vec::with_capacity(164 * 1024);
        let mut i = 0u32;
        while code.len() < 160 * 1024 {
            let line = format!(
                "pub fn intersects_{i}(p: &Point<C>, q: &Point<C>, out: &mut Vec<Coord>) -> bool {{ p.x * {} + q.y >= {} }}\n",
                i.wrapping_mul(7) % 997,
                i % 13
            );
            code.extend_from_slice(line.as_bytes());
            i += 1;
        }
        std::fs::write(d.join("src/geometry.rs"), &code).unwrap();
        // «литература»: проза с абзацами
        let prose = "Троичный кристалл помнит весь корпус: трит −1, 0, +1 живёт в байте впятером.\n\nСуверенный архиватор льёт поток прямо в кристалл без сырой выгрузки.\n\n"
            .repeat(600);
        std::fs::write(d.join("docs/story.md"), prose.as_bytes()).unwrap();
        // «данные»
        std::fs::write(
            d.join("meta.json"),
            br#"{"trits": 3, "qutrits": 9, "corpus": "poler-engine"}"#,
        )
        .unwrap();
        // ДУБЛЬ кода: CDC-дедуп обязан схлопнуть повторяющиеся чанки
        std::fs::write(d.join("src/geometry_copy.rs"), &code).unwrap();

        let p = tmp("corpus.poler");
        let mut cfg = corpus_config(2);
        cfg.cdc = CdcParams::new(4096, 16 * 1024, 64 * 1024); // скорость теста
        let (stats, classes) = corpus_pack(&d, &p, cfg, false, false).unwrap();
        assert!(stats.tar_mode, "пофайловая таблица: {stats:?}");
        assert_eq!(stats.files, 4, "4 файла корпуса в таблице");
        assert!(
            stats.dedup_chunks >= 2,
            "дубль кода не схлопнут: {} хитов",
            stats.dedup_chunks
        );
        assert_eq!(classes.code_files, 2);
        assert_eq!(classes.prose_files, 1);
        assert_eq!(classes.data_files, 1);
        assert!(classes.top_ext.iter().any(|(e, _, _)| e == "rs"));

        let r = PolerReader::open(&p).unwrap();
        let rep = r.verify().unwrap();
        assert!(rep.all_ok, "verify: {:?}", rep.files_bad);
        let f = r.find_file("src/geometry.rs").unwrap();
        let mut out = Vec::new();
        r.read_range(f.raw_off, f.raw_len as usize, &mut out).unwrap();
        assert_eq!(out, code, "lossless байт-в-байт");
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_file(&p);
    }

    /// v0.91.0: классификация расширений корпуса.
    #[test]
    fn corpus_class_of_ext() {
        assert_eq!(CorpusClass::of_ext("rs"), CorpusClass::Code);
        assert_eq!(CorpusClass::of_ext("py"), CorpusClass::Code);
        assert_eq!(CorpusClass::of_ext("md"), CorpusClass::Prose);
        assert_eq!(CorpusClass::of_ext("txt"), CorpusClass::Prose);
        assert_eq!(CorpusClass::of_ext("json"), CorpusClass::Data);
        assert_eq!(CorpusClass::of_ext("yaml"), CorpusClass::Data);
        assert_eq!(CorpusClass::of_ext("so"), CorpusClass::Other);
        assert_eq!(CorpusClass::of_ext(""), CorpusClass::Other);
        assert_eq!(CorpusClass::of_ext("pdf"), CorpusClass::Prose);
    }
}
