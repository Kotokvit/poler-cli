//! Слой 0 Native Retrieval: точный поиск в семантике grep.
//!
//! Назначение — заменить агенту внешний grep: ВСЕ совпадения по файлам,
//! без индекса, мгновенно, с гарантией полноты («ноль значит ноль»).
//! Эталон поведения — GNU grep (см. `docs/native-retrieval-analysis.md`):
//! exit-коды 0/1/2 для скриптов, контекст `-A/-B/-C` с групповым
//! разделителем `--`, режимы `-c/-l/-L`, бинарные файлы («Binary file
//! … matches»).
//!
//! Отличия от GNU grep (осознанные):
//! * обход — `ignore::WalkBuilder` (ripgrep-класс): .gitignore/.ignore,
//!   скрытые файлы по флагу, симлинки не преследуются;
//! * результат — машинно-читаемый [`GrepReport`] (serde): каждая строка
//!   несёт `byte_offset` и байтовые диапазоны вхождений — для ИИ-агента;
//! * регистронезависимость — Unicode-fold через крейт `regex` (шире,
//!   чем ASCII-fold grep);
//! * файлы с NUL-байтом считаются бинарными: содержимое не выводится,
//!   но count/list досканировываются полностью;
//! * диапазоны вхождений относятся к lossy-UTF8 тексту строки (файлы с
//!   невалидным UTF-8 дают текст с U+FFFD); `byte_offset` начала строки
//!   всегда точен.
//!
//! Сопоставители: фиксированная строка — Aho-Corasick (крейт
//! `aho-corasick`, тот же класс, что kwset GNU grep); регулярное
//! выражение — крейт `regex`. Оба уже были зависимостями движка.

use std::collections::VecDeque;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use aho_corasick::AhoCorasick;
use ignore::WalkBuilder;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

pub const SKIP_DIRS: &[&str] = &[
    ".git", ".hg", ".svn", "node_modules", "target", "build", ".cache",
    ".idea", ".vscode", "dist", "vendor", "__pycache__", ".venv",
];

/// Потолок размера файла для сканирования (защита от mmap-гигантов).
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// Потолок подсвечиваемых вхождений на строку (минифицированный JS).
const MAX_RANGES_PER_LINE: usize = 256;

/// Режим сопоставления шаблоном.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrepMode {
    /// Фиксированная строка (grep -F): Aho-Corasick.
    Literal,
    /// Расширенное регулярное выражение (grep -E).
    Regex,
}

/// Режим вывода результатов.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrepOutput {
    /// Строки с совпадениями (+контекст) — как grep без ключей.
    Content,
    /// Только счётчик совпавших строк на файл (grep -c).
    Count,
    /// Только пути файлов с совпадениями (grep -l).
    ListMatching,
    /// Только пути файлов без совпадений (grep -L).
    ListNonMatching,
}

/// Конфигурация одного прогона точного поиска.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepConfig {
    /// Искомый шаблон (фиксированная строка либо regex — по `mode`).
    pub pattern: String,
    /// Literal / Regex.
    pub mode: GrepMode,
    /// Регистронезависимость (Unicode-fold).
    pub case_insensitive: bool,
    /// Строк контекста до совпадения (grep -B).
    pub before: usize,
    /// Строк контекста после совпадения (grep -A).
    pub after: usize,
    /// Останов после N совпавших строк на файл (grep -m; N на файл).
    pub max_count: Option<usize>,
    /// Режим вывода.
    pub output: GrepOutput,
    /// Включить скрытые файлы.
    pub include_hidden: bool,
    /// Уважать .gitignore/.ignore (по умолчанию — да, как ripgrep).
    pub respect_ignore: bool,
    /// Скан архивов без распаковки (v0.28.1): zip/tar/tar.gz/tar.zst/gz/zst
    /// внутри корней поиска раскрываются виртуальными файлами
    /// «архив::запись». Сам архивный файл как бинарник НЕ сканируется
    /// (исключаются дубли-маркеры бинарности).
    #[serde(default)]
    pub scan_archives: bool,
    /// Пароль зашифрованных записей zip (ZipCrypto/AES). CLI резолвит:
    /// --archive-password → POLER_ARCHIVE_KEY → TTY-промпт.
    #[serde(default)]
    pub archive_password: Option<String>,
    /// Лимит несжатой записи архива, байт (0 → DEFAULT_MAX_ENTRY_BYTES,
    /// 64 МиБ). Защита от zip-бомб: запись свыше лимита пропускается.
    #[serde(default)]
    pub archive_max_entry_bytes: u64,
}

impl Default for GrepConfig {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            mode: GrepMode::Literal,
            case_insensitive: false,
            before: 0,
            after: 0,
            max_count: None,
            output: GrepOutput::Content,
            include_hidden: false,
            respect_ignore: true,
            scan_archives: false,
            archive_password: None,
            archive_max_entry_bytes: 0,
        }
    }
}

/// Скомпилированный сопоставитель.
enum Matcher {
    /// Aho-Corasick по фиксированной строке (байты lossy-строки).
    Literal { ac: AhoCorasick },
    /// regex-крейт (regex-режим либо icase-literal через escape).
    Regex(regex::Regex),
}

impl Matcher {
    /// Компиляция шаблона. Ошибка — человекочитаемой строкой (exit 2).
    fn build(config: &GrepConfig) -> Result<Self, String> {
        match config.mode {
            GrepMode::Literal if !config.case_insensitive => {
                let ac = AhoCorasick::new([&config.pattern])
                    .map_err(|e| format!("шаблон не компилируется: {e}"))?;
                Ok(Matcher::Literal { ac })
            }
            _ => {
                // icase-literal: escape → та же семантика, но Unicode-fold.
                let src = match config.mode {
                    GrepMode::Literal => regex::escape(&config.pattern),
                    GrepMode::Regex => config.pattern.clone(),
                };
                let re = regex::RegexBuilder::new(&src)
                    .case_insensitive(config.case_insensitive)
                    .build()
                    .map_err(|e| format!("некорректный шаблон «{}»: {e}", config.pattern))?;
                Ok(Matcher::Regex(re))
            }
        }
    }

    /// Непересекающиеся вхождения в строке: байтовые диапазоны
    /// относительно `text` (lossy-UTF8). Пустой шаблон матчит все строки
    /// без подсветки (семантика grep "").
    fn find_ranges(&self, text: &str) -> Vec<(usize, usize)> {
        let mut ranges = Vec::new();
        match self {
            Matcher::Literal { ac } => {
                for m in ac.find_iter(text.as_bytes()) {
                    if !m.is_empty() && ranges.len() < MAX_RANGES_PER_LINE {
                        ranges.push((m.start(), m.end()));
                    }
                }
            }
            Matcher::Regex(re) => {
                for m in re.find_iter(text) {
                    if !m.is_empty() && ranges.len() < MAX_RANGES_PER_LINE {
                        ranges.push((m.start(), m.end()));
                    }
                }
            }
        }
        ranges
    }
}

/// Одна выходная строка (совпадение или контекст).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepLineOut {
    /// Номер строки, 1-базный.
    pub line_no: usize,
    /// Байтовое смещение начала строки в файле.
    pub byte_offset: usize,
    /// Текст строки (lossy-UTF8, без завершающего \n/\r).
    pub text: String,
    /// Строка-совпадение (true) или контекстная (false).
    pub matched: bool,
    /// Байтовые диапазоны вхождений внутри `text` (только для matched).
    pub ranges: Vec<(usize, usize)>,
}

/// Контекстная группа: непрерывный блок вывода одного файла
/// (несколько совпадений со слипшимся контекстом — одна группа).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepGroup {
    /// Путь файла.
    pub path: String,
    /// Бинарный файл: содержимое не выводилось, но совпадения есть.
    pub binary: bool,
    /// Строки группы (до + совпадения + после, по порядку).
    pub lines: Vec<GrepLineOut>,
}

/// Счётчик на файл (режимы Count / List*).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepFileCounts {
    /// Путь файла.
    pub path: String,
    /// Число совпавших строк.
    pub count: usize,
    /// Файл содержит NUL-байты.
    pub binary: bool,
}

/// Сводка прогона.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GrepStats {
    /// Просканировано файлов.
    pub files_scanned: usize,
    /// Файлов с совпадениями.
    pub files_matched: usize,
    /// Совпавших строк (суммарно).
    pub lines_matched: usize,
    /// Вхождений шаблона (суммарно, включая повторы в строке).
    pub occurrences: usize,
    /// Время прогона, мс.
    pub elapsed_ms: u128,
    /// Накопленные ошибки обхода (права доступа и т.п.).
    pub errors: Vec<String>,
}

/// Полный результат точного поиска (машинно-читаемый).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepReport {
    /// Шаблон прогона.
    pub pattern: String,
    /// Режим вывода.
    pub output: GrepOutput,
    /// Контекстные группы (Content-режим).
    pub groups: Vec<GrepGroup>,
    /// Счётчики на файл (Count / List*).
    pub files: Vec<GrepFileCounts>,
    /// Сводка.
    pub stats: GrepStats,
}

impl GrepReport {
    /// Есть ли результат: критерий exit-кода 0. Для ListNonMatching
    /// «успех» = выведен хотя бы один файл без совпадений.
    pub fn any_match(&self) -> bool {
        if self.output == GrepOutput::ListNonMatching {
            return self.files.iter().any(|f| f.count == 0);
        }
        self.stats.files_matched > 0
    }

    /// Exit-код в семантике grep: 0 — есть результат, 1 — пусто.
    pub fn exit_code(&self) -> i32 {
        if self.any_match() {
            0
        } else {
            1
        }
    }
}

/// Результат сканирования одного файла.
struct FileScan {
    path: String,
    binary: bool,
    matched_lines: usize,
    occurrences: usize,
    /// Content-режим: контекстные группы.
    groups: Vec<GrepGroup>,
    error: Option<String>,
}

/// Сканирование одного файла: построчный проход с контекстом.
/// M6: `cache` — резидентный RAM-кэш (None — классическое чтение с диска).
fn scan_file(path: &Path, matcher: &Matcher, config: &GrepConfig) -> FileScan {
    let mut scan = FileScan {
        path: path.display().to_string(),
        binary: false,
        matched_lines: 0,
        occurrences: 0,
        groups: Vec::new(),
        error: None,
    };
    // Аудит-фикс №F1: размер по metadata ДО чтения. Раньше файл целиком
    // попадал в память и только потом отбрасывался за превышение лимита —
    // на rayon-пуле (16 потоков × гигантские файлы) это врыв RAM.
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > MAX_FILE_BYTES {
            scan.error = Some(format!(
                "{}: {} байт > лимита {MAX_FILE_BYTES} — пропущен",
                path.display(),
                meta.len()
            ));
            return scan;
        }
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            scan.error = Some(format!("{}: {e}", path.display()));
            return scan;
        }
    };
    scan_bytes(&scan.path.clone(), &bytes, matcher, config)
}

/// v0.22.0 (Terminal Gateway): сканирование БУФЕРА из памяти (stdin
/// конвейера) с той же семантикой контекста/бинарности, что и файлы.
fn scan_bytes(name: &str, bytes: &[u8], matcher: &Matcher, config: &GrepConfig) -> FileScan {
    let mut scan = FileScan {
        path: name.to_string(),
        binary: false,
        matched_lines: 0,
        occurrences: 0,
        groups: Vec::new(),
        error: None,
    };
    if bytes.len() as u64 > MAX_FILE_BYTES {
        scan.error = Some(format!(
            "{name}: {} байт > лимита {MAX_FILE_BYTES} — пропущен",
            bytes.len()
        ));
        return scan;
    }

    let want_content = config.output == GrepOutput::Content;
    // Состояние контекста (grep -A/-B): pending-буфер «до», счётчик «после».
    let mut before_buf: VecDeque<(usize, usize, String)> = VecDeque::new();
    let mut after_left = 0usize;
    let mut last_emitted: Option<usize> = None;
    let mut group: Option<Vec<GrepLineOut>> = None;
    let mut closing = false; // max_count достигнут: добираем хвост -A

    let mut line_no = 0usize;
    let mut offset = 0usize;
    for (start, end) in split_lines(bytes) {
        line_no += 1;
        let line_bytes = &bytes[start..end];
        let text = String::from_utf8_lossy(line_bytes)
            .trim_end_matches('\r')
            .to_string();

        // Бинарность: NUL-байт в строке. Содержимое не выводим,
        // но счётчики (count/list) продолжаем добывать.
        if line_bytes.contains(&0) {
            scan.binary = true;
        }

        let ranges = matcher.find_ranges(&text);
        let matched = !ranges.is_empty() || config.pattern.is_empty();
        if matched {
            scan.matched_lines += 1;
            scan.occurrences += ranges.len();
        }

        if want_content && !scan.binary {
            if matched {
                // Разрыв между группами → закрыть предыдущую (разделитель
                // «--» рендерится между группами одного файла).
                let first_new = before_buf
                    .front()
                    .map(|(n, _, _)| *n)
                    .unwrap_or(line_no);
                if let Some(last) = last_emitted {
                    if first_new > last + 1 && group.is_some() {
                        scan.groups.push(GrepGroup {
                            path: scan.path.clone(),
                            binary: false,
                            lines: group.take().unwrap(),
                        });
                    }
                }
                let g = group.get_or_insert_with(Vec::new);
                // Ведущий контекст: только ещё не выведенные строки.
                while let Some(&(n, off, ref t)) = before_buf.front() {
                    if Some(n) <= last_emitted {
                        before_buf.pop_front();
                        continue;
                    }
                    g.push(GrepLineOut {
                        line_no: n,
                        byte_offset: off,
                        text: t.clone(),
                        matched: false,
                        ranges: Vec::new(),
                    });
                    last_emitted = Some(n);
                    before_buf.pop_front();
                }
                g.push(GrepLineOut {
                    line_no,
                    byte_offset: offset,
                    text: text.clone(),
                    matched: true,
                    ranges,
                });
                last_emitted = Some(line_no);
                after_left = config.after;
                // -m: совпадений достаточно — добрать хвост -A и выйти.
                if let Some(max) = config.max_count {
                    if scan.matched_lines >= max {
                        closing = true;
                    }
                }
            } else if after_left > 0 {
                let g = group.get_or_insert_with(Vec::new);
                g.push(GrepLineOut {
                    line_no,
                    byte_offset: offset,
                    text: text.clone(),
                    matched: false,
                    ranges: Vec::new(),
                });
                last_emitted = Some(line_no);
                after_left -= 1;
            } else {
                // Кандидат ведущего контекста будущей группы.
                before_buf.push_back((line_no, offset, text.clone()));
                if before_buf.len() > config.before {
                    before_buf.pop_front();
                }
            }
        }

        offset = end + 1; // за \n
        if closing && after_left == 0 {
            break;
        }
    }
    if let Some(lines) = group.take() {
        if !lines.is_empty() {
            scan.groups.push(GrepGroup {
                path: scan.path.clone(),
                binary: false,
                lines,
            });
        }
    }
    scan
}

/// Разбивка байтов на строки: (начало, конец) БЕЗ завершающего \n.
/// Последняя строка без \n включается; пустой хвост — нет.
fn split_lines(bytes: &[u8]) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        if pos >= bytes.len() {
            return None;
        }
        match memchr::memchr(b'\n', &bytes[pos..]) {
            Some(i) => {
                let start = pos;
                let end = pos + i;
                pos = end + 1;
                Some((start, end))
            }
            None => {
                let start = pos;
                let end = bytes.len();
                pos = end + 1;
                Some((start, end))
            }
        }
    })
}

/// Сбор файлов для поиска: файл напрямую либо рекурсивный обход
/// (ripgrep-класс: .gitignore/.ignore, скрытые, SKIP_DIRS движка).
fn collect_grep_files(root: &Path, config: &GrepConfig) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    WalkBuilder::new(root)
        .hidden(!config.include_hidden)
        .git_ignore(config.respect_ignore)
        .git_global(config.respect_ignore)
        .git_exclude(config.respect_ignore)
        .ignore(config.respect_ignore)
        .require_git(false)
        .filter_entry(move |e| {
            e.file_type().map_or(true, |t| !t.is_dir())
                || !SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref())
        })
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
        .collect()
}

/// Полный прогон точного поиска. `roots` — файлы или каталоги
/// (несуществующий корень → Err, exit 2).
pub fn grep_run(roots: &[PathBuf], config: &GrepConfig) -> Result<GrepReport, String> {
    let started = std::time::Instant::now();
    let matcher = Matcher::build(config)?;

    let mut files: Vec<PathBuf> = Vec::new();
    for root in roots {
        if !root.exists() {
            return Err(format!("путь не найден: {}", root.display()));
        }
        files.extend(collect_grep_files(root, config));
    }
    files.sort();
    files.dedup();

    let scans: Vec<FileScan> = files
        .par_iter()
        .map(|p| scan_file(p, &matcher, config))
        .collect();

    let report = assemble_report(scans, config, started);
    Ok(report)
}

/// Сборка GrepReport из набора сканов (общая для grep_run и grep_buffer).
fn assemble_report(scans: Vec<FileScan>, config: &GrepConfig, started: std::time::Instant) -> GrepReport {
    let mut report = GrepReport {
        pattern: config.pattern.clone(),
        output: config.output,
        groups: Vec::new(),
        files: Vec::new(),
        stats: GrepStats {
            files_scanned: scans.len(),
            ..Default::default()
        },
    };
    for s in scans {
        if let Some(e) = s.error {
            report.stats.errors.push(e);
        }
        if s.matched_lines > 0 {
            report.stats.files_matched += 1;
        }
        report.stats.lines_matched += s.matched_lines;
        report.stats.occurrences += s.occurrences;
        match config.output {
            GrepOutput::Content => {
                // Бинарный файл: одна группа-маркер «совпадения есть».
                if s.binary && s.matched_lines > 0 {
                    report.groups.push(GrepGroup {
                        path: s.path.clone(),
                        binary: true,
                        lines: Vec::new(),
                    });
                } else {
                    report.groups.extend(s.groups);
                }
            }
            _ => report.files.push(GrepFileCounts {
                path: s.path,
                count: s.matched_lines,
                binary: s.binary,
            }),
        }
    }
    report.stats.elapsed_ms = started.elapsed().as_millis();
    report
}

/// v0.22.0 (Terminal Gateway): точный поиск по БУФЕРУ из памяти —
/// stdin конвейера (`cat notes.md | poler grep TODO`). Та же семантика
/// (контекст, бинарность, exit-коды), что и у файлового grep_run;
/// `name` — псевдоним источника в выводе (обычно "stdin").
pub fn grep_buffer(name: &str, text: &str, config: &GrepConfig) -> Result<GrepReport, String> {
    let started = std::time::Instant::now();
    let matcher = Matcher::build(config)?;
    let scan = scan_bytes(name, text.as_bytes(), &matcher, config);
    Ok(assemble_report(vec![scan], config, started))
}

/// Человекочитаемый рендер в семантике grep: `path:LINE:text`,
/// контекст через `path-LINE-text`, `--` между группами, бинарные —
/// «Binary file … matches». `color` — ANSI-подсветка вхождений.
pub fn render_text(report: &GrepReport, color: bool) -> String {
    let mut out = String::new();
    let mut prev_path: Option<&str> = None;
    match report.output {
        GrepOutput::Content => {
            for g in &report.groups {
                if g.binary {
                    out.push_str(&format!("Binary file {} matches\n", g.path));
                    continue;
                }
                if let Some(p) = prev_path {
                    if p == g.path {
                        out.push_str("--\n");
                    }
                }
                prev_path = Some(&g.path);
                for l in &g.lines {
                    let sep = if l.matched { ':' } else { '-' };
                    out.push_str(&format!("{}{}{}:", g.path, sep, l.line_no));
                    if color && !l.ranges.is_empty() {
                        out.push_str(&highlight(&l.text, &l.ranges));
                    } else {
                        out.push_str(&l.text);
                    }
                    out.push('\n');
                }
            }
        }
        GrepOutput::Count => {
            for f in &report.files {
                out.push_str(&format!("{}:{}\n", f.path, f.count));
            }
        }
        GrepOutput::ListMatching => {
            for f in report.files.iter().filter(|f| f.count > 0) {
                out.push_str(&format!("{}\n", f.path));
            }
        }
        GrepOutput::ListNonMatching => {
            for f in report.files.iter().filter(|f| f.count == 0) {
                out.push_str(&format!("{}\n", f.path));
            }
        }
    }
    out
}

/// Подсветка вхождений ANSI-красным (диапазоны не пересекаются и
/// выровнены по границам символов — см. инварианты Matcher).
fn highlight(text: &str, ranges: &[(usize, usize)]) -> String {
    let mut out = String::with_capacity(text.len() + ranges.len() * 8);
    let mut pos = 0usize;
    for &(s, e) in ranges {
        if s < pos {
            continue; // защита от пересечений
        }
        out.push_str(&text[pos..s]);
        out.push_str("\x1b[31m\x1b[1m");
        out.push_str(&text[s..e]);
        out.push_str("\x1b[0m");
        pos = e;
    }
    out.push_str(&text[pos..]);
    out
}

/// Автовыбор подсветки: терминал — да, пайп — нет.
pub fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grep_basic_literal() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "hello world\nfoo bar\nhello rust\n").unwrap();

        let cfg = GrepConfig {
            pattern: "hello".to_string(),
            mode: GrepMode::Literal,
            case_insensitive: false,
            max_count: Some(100),
            ..Default::default()
        };
        let r = grep_run(&[dir.path().to_path_buf()], &cfg).unwrap();
        assert_eq!(r.stats.lines_matched, 2);
    }
}
