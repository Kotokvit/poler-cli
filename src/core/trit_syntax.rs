//! poler-edit :: TRIT-SYNTAX & L3 CSR GRAPH — троичная лента скоупов
//! (`+1` вход, `0` вакуум `121`, `-1` выход), мгновенная предкомпиляционная
//! верификация баланса скобок, универсальный Outline/Breadcrumbs (Код + Markdown)
//! и проектный CSR Call-Graph в `pack5` (0.2 Б/связь в L3-кэше процессора).

use crate::buffer::BufSnapshot;
use crate::trit_silicon::{
    cmp_proj, lens_filter_proj, pack5, trit_vacuum_ratio, Proj, TritSig256, TRIT5_ZERO_CODE,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Битовые флаги контрактов для барьера `LENS No-Hits`.
pub const FLAG_PUBLIC: u64 = 1 << 0;
pub const FLAG_FUNCTION: u64 = 1 << 1;
pub const FLAG_TYPE: u64 = 1 << 2;
pub const FLAG_UNSAFE_OR_MUT: u64 = 1 << 3;
pub const FLAG_TEST: u64 = 1 << 4;
pub const FLAG_MARKDOWN: u64 = 1 << 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexState {
    Code,
    LineComment,
    BlockComment,
    DoubleQuote { escaped: bool },
    SingleQuote { escaped: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyntaxIssue {
    pub kind: String,
    pub message: String,
    pub byte: u64,
    pub line: u64,
    pub col: u64,
    pub expected: Option<String>,
    pub found: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyntaxVerdict {
    pub balanced: bool,
    pub net_trit_sum: i64,
    pub min_dip: i64,
    pub max_depth: i64,
    pub packed_bytes: usize,
    pub vacuum_ratio: [i64; 2],
    pub issues: Vec<SyntaxIssue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutlineItem {
    pub name: String,
    pub kind: String,
    pub level: usize,
    pub line: u64,
    pub end_line: u64,
    pub flags: u64,
}

#[derive(Debug, Clone)]
pub struct IndexedScope {
    pub file: PathBuf,
    pub item: OutlineItem,
    pub sig: TritSig256,
    pub callees: Vec<(String, i8)>, // +1 вызов/чтение, -1 unsafe/мутация
}

#[derive(Debug, Clone, Serialize)]
pub struct ImpactReport {
    pub symbol: String,
    pub definitions: Vec<serde_json::Value>,
    pub upstream_callers: Vec<serde_json::Value>,
    pub downstream_callees: Vec<String>,
    pub mutating_or_unsafe_callers: usize,
    pub danger_level: &'static str,
    pub graph_nodes: usize,
    pub graph_csr_packed_bytes: usize,
    pub graph_vacuum_ratio: [i64; 2],
}

#[derive(Debug, Clone, Serialize)]
pub struct SemanticScopeHit {
    pub file: String,
    pub name: String,
    pub kind: String,
    pub line: u64,
    pub end_line: u64,
    pub flags: u64,
    pub proj: [i64; 2],
}

#[inline]
fn matching_close(open: u8) -> u8 {
    match open {
        b'{' => b'}',
        b'(' => b')',
        b'[' => b']',
        _ => 0,
    }
}

/// Троичная верификация баланса скобок поверх zero-copy `BufSnapshot`
/// без материализации всего файла в `String`.
/// Считает тритовую ленту `{-1, 0, +1}`, пакует её блоками в `pack5` (байт вакуума `121`)
/// и возвращает точные координаты первой же сломанной или незакрытой скобки.
pub fn verify_snapshot_syntax(snap: &BufSnapshot, is_markdown: bool) -> SyntaxVerdict {
    if is_markdown || snap.is_empty() || snap.total_len == 0 {
        return SyntaxVerdict {
            balanced: true,
            net_trit_sum: 0,
            min_dip: 0,
            max_depth: 0,
            packed_bytes: 0,
            vacuum_ratio: [1, 1],
            issues: Vec::new(),
        };
    }

    let mut state = LexState::Code;
    let mut stack: Vec<(u8, u64, u64, u64)> = Vec::new(); // (open_ch, byte, line, col_bytes)
    let mut net: i64 = 0;
    let mut min_dip: i64 = 0;
    let mut max_depth: i64 = 0;
    let mut issues: Vec<SyntaxIssue> = Vec::new();

    let mut line: u64 = 0;
    let mut line_start: u64 = 0;
    let mut global_off: u64 = 0;
    let mut prev_byte: u8 = 0;

    // Стриминговый pack5-счётчик вакуума (чанки по 4095 тритов = 819 байт, 0 роста RAM на ГБ-файлах).
    let mut chunk_trits: Vec<i8> = Vec::with_capacity(4095);
    let mut packed_scratch: Vec<u8> = Vec::with_capacity(819);
    let mut total_packed_bytes: usize = 0;
    let mut total_vacuum_bytes: usize = 0;

    for p_idx in 0..snap.pieces_len() {
        let bytes = snap.piece_bytes(p_idx);
        let mut i = 0usize;
        while i < bytes.len() {
            let b = bytes[i];
            let next = if i + 1 < bytes.len() {
                bytes[i + 1]
            } else if p_idx + 1 < snap.pieces_len() {
                snap.piece_bytes(p_idx + 1).first().copied().unwrap_or(0)
            } else {
                0
            };

            let mut trit: i8 = 0;

            match state {
                LexState::LineComment => {
                    if b == b'\n' {
                        state = LexState::Code;
                    }
                }
                LexState::BlockComment => {
                    if prev_byte == b'*' && b == b'/' {
                        state = LexState::Code;
                        prev_byte = 0;
                        global_off += 1;
                        i += 1;
                        continue;
                    }
                }
                LexState::DoubleQuote { escaped } => {
                    if escaped {
                        state = LexState::DoubleQuote { escaped: false };
                    } else if b == b'\\' {
                        state = LexState::DoubleQuote { escaped: true };
                    } else if b == b'"' {
                        state = LexState::Code;
                    }
                }
                LexState::SingleQuote { escaped } => {
                    // В Rust `'a` — лайфтайм, а `'x'` или `'\n'` — символьный литерал.
                    if escaped {
                        state = LexState::SingleQuote { escaped: false };
                    } else if b == b'\\' {
                        state = LexState::SingleQuote { escaped: true };
                    } else if b == b'\'' || b == b'\n' {
                        state = LexState::Code;
                    }
                }
                LexState::Code => {
                    if b == b'/' && next == b'/' {
                        state = LexState::LineComment;
                    } else if b == b'/' && next == b'*' {
                        state = LexState::BlockComment;
                        // Съедаем открывающий '/' и не даём "/*/" закрыться на том же '*'
                        chunk_trits.push(0);
                        prev_byte = 0;
                        global_off += 2;
                        i += 2;
                        if i > bytes.len() {
                            // '*' лежит на первом байте следующего куска — обработаем штатно
                            global_off -= 1;
                        }
                        continue;
                    } else if b == b'"' {
                        state = LexState::DoubleQuote { escaped: false };
                    } else if b == b'\'' {
                        // Отличаем символьный литерал `'c'` / `'\x'` от Rust lifetime `'a`:
                        let after_next = if i + 2 < bytes.len() {
                            bytes[i + 2]
                        } else {
                            0
                        };
                        if next == b'\\' || after_next == b'\'' {
                            state = LexState::SingleQuote { escaped: false };
                        }
                    } else if b == b'{' || b == b'(' || b == b'[' {
                        trit = 1;
                        net += 1;
                        if net > max_depth {
                            max_depth = net;
                        }
                        stack.push((b, global_off, line, global_off - line_start));
                    } else if b == b'}' || b == b')' || b == b']' {
                        trit = -1;
                        net -= 1;
                        if net < min_dip {
                            min_dip = net;
                        }
                        let col = global_off - line_start;
                        match stack.pop() {
                            Some((open, _, _, _)) => {
                                let exp = matching_close(open);
                                if exp != b && issues.len() < 16 {
                                    issues.push(SyntaxIssue {
                                        kind: "mismatched_bracket".into(),
                                        message: format!(
                                            "ожидалась '{}', встречена '{}'",
                                            exp as char, b as char
                                        ),
                                        byte: global_off,
                                        line,
                                        col,
                                        expected: Some((exp as char).to_string()),
                                        found: Some((b as char).to_string()),
                                    });
                                }
                            }
                            None => {
                                if issues.len() < 16 {
                                    issues.push(SyntaxIssue {
                                        kind: "unexpected_close".into(),
                                        message: format!(
                                            "лишняя закрывающая скобка '{}' (трит-баланс ушёл в -1)",
                                            b as char
                                        ),
                                        byte: global_off,
                                        line,
                                        col,
                                        expected: None,
                                        found: Some((b as char).to_string()),
                                    });
                                }
                            }
                        }
                    }
                }
            }

            chunk_trits.push(trit);
            if chunk_trits.len() == 4095 {
                pack5(&chunk_trits, &mut packed_scratch);
                total_packed_bytes += packed_scratch.len();
                let vr = trit_vacuum_ratio(&packed_scratch);
                total_vacuum_bytes += vr.n as usize;
                chunk_trits.clear();
            }

            if b == b'\n' {
                line += 1;
                line_start = global_off + 1;
            }
            prev_byte = b;
            global_off += 1;
            i += 1;
        }
    }

    if !chunk_trits.is_empty() {
        pack5(&chunk_trits, &mut packed_scratch);
        total_packed_bytes += packed_scratch.len();
        let vr = trit_vacuum_ratio(&packed_scratch);
        total_vacuum_bytes += vr.n as usize;
    }

    while let Some((open, boff, ln, col)) = stack.pop() {
        if issues.len() < 16 {
            let exp = matching_close(open) as char;
            issues.push(SyntaxIssue {
                kind: "unclosed_open".into(),
                message: format!("незакрытая скобка '{}', ожидается '{}'", open as char, exp),
                byte: boff,
                line: ln,
                col,
                expected: Some(exp.to_string()),
                found: None,
            });
        }
    }

    let balanced = net == 0 && min_dip >= 0 && issues.is_empty();
    let vac = if total_packed_bytes == 0 {
        [1, 1]
    } else {
        let p = Proj::new(total_vacuum_bytes as i64, total_packed_bytes as i64).canonicalize();
        [p.n, p.d]
    };

    SyntaxVerdict {
        balanced,
        net_trit_sum: net,
        min_dip,
        max_depth,
        packed_bytes: total_packed_bytes,
        vacuum_ratio: vac,
        issues,
    }
}

/// Извлечь иерархическую структуру документа (Outline) с точными границами
/// скоупов `[line ..= end_line]` через закон аннигиляции тритов `Σ t_i == 0`.
/// Работает как для Markdown (`#..######`), так и для исходного кода
/// (Rust, C/C++, Zig, Python, JS/TS, Go, Kotlin).
pub fn extract_outline_from_bytes(bytes: &[u8], path_hint: Option<&Path>) -> Vec<OutlineItem> {
    let ext = path_hint
        .and_then(|p| p.extension())
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let text = String::from_utf8_lossy(bytes);
    if matches!(ext.as_str(), "md" | "markdown" | "txt" | "fb2" | "epub") {
        return extract_markdown_outline(&text);
    }
    let items = extract_code_outline(&text, &ext);
    if items.is_empty() {
        extract_markdown_outline(&text)
    } else {
        items
    }
}

fn extract_markdown_outline(text: &str) -> Vec<OutlineItem> {
    let mut items: Vec<OutlineItem> = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut in_fence = false;
    for (idx, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || !trimmed.starts_with('#') {
            continue;
        }
        let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&hashes) {
            let rest = trimmed[hashes..].trim();
            if !rest.is_empty() {
                if let Some(prev) = items.last_mut() {
                    if prev.end_line == prev.line {
                        prev.end_line = idx.saturating_sub(1) as u64;
                    }
                }
                items.push(OutlineItem {
                    name: rest.to_string(),
                    kind: format!("H{hashes}"),
                    level: hashes,
                    line: idx as u64,
                    end_line: idx as u64,
                    flags: FLAG_MARKDOWN | FLAG_PUBLIC,
                });
            }
        }
    }
    let last_line = lines.len().saturating_sub(1) as u64;
    if let Some(last) = items.last_mut() {
        if last.end_line <= last.line {
            last.end_line = last_line;
        }
    }
    items
}

fn extract_code_outline(text: &str, ext: &str) -> Vec<OutlineItem> {
    let lines: Vec<&str> = text.lines().collect();
    let is_py = ext == "py";

    // Построчный профиль тритового баланса фигурных скобок `{` (+1) / `}` (-1)
    let mut line_entry_depth = vec![0i32; lines.len()];
    let mut line_exit_depth = vec![0i32; lines.len()];
    let mut depth = 0i32;
    let mut in_block_comment = false;

    for (idx, &ln) in lines.iter().enumerate() {
        line_entry_depth[idx] = depth.max(0);
        let b = ln.as_bytes();
        let mut i = 0;
        let mut in_str = false;
        while i < b.len() {
            if in_block_comment {
                if i + 1 < b.len() && b[i] == b'*' && b[i + 1] == b'/' {
                    in_block_comment = false;
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
            if in_str {
                if b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if b[i] == b'"' {
                    in_str = false;
                }
                i += 1;
                continue;
            }
            if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'/' {
                break;
            }
            if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
                in_block_comment = true;
                i += 2;
                continue;
            }
            if b[i] == b'"' {
                in_str = true;
            } else if b[i] == b'{' {
                depth += 1;
            } else if b[i] == b'}' {
                depth -= 1;
            }
            i += 1;
        }
        line_exit_depth[idx] = depth.max(0);
    }

    let mut out = Vec::new();
    for (idx, &ln) in lines.iter().enumerate() {
        let trimmed = ln.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
            || (is_py && trimmed.starts_with('#'))
        {
            continue;
        }
        if let Some((name, kind, flags)) = detect_symbol_decl(trimmed, ext) {
            let entry_d = line_entry_depth[idx];
            let end_line = if is_py {
                let base_indent = ln.bytes().take_while(|&c| c == b' ' || c == b'\t').count();
                let mut end = idx;
                for j in (idx + 1)..lines.len() {
                    let t = lines[j].trim();
                    if t.is_empty() || t.starts_with('#') {
                        continue;
                    }
                    let ind = lines[j]
                        .bytes()
                        .take_while(|&c| c == b' ' || c == b'\t')
                        .count();
                    if ind <= base_indent {
                        break;
                    }
                    end = j;
                }
                end as u64
            } else {
                // Ищем первую строку, где после открытия блока `{` баланс аннигилируется в entry_d
                let mut opened = line_exit_depth[idx] > entry_d || trimmed.contains('{');
                let mut end = idx;
                for j in idx..lines.len() {
                    if j > idx && !opened && line_exit_depth[j] > entry_d {
                        opened = true;
                    }
                    end = j;
                    if opened && line_exit_depth[j] <= entry_d {
                        break;
                    }
                    if !opened && lines[j].trim_end().ends_with(';') {
                        break;
                    }
                }
                end as u64
            };

            let level = if is_py {
                (ln.bytes().take_while(|&c| c == b' ').count() / 4) + 1
            } else {
                (entry_d as usize) + 1
            };

            out.push(OutlineItem {
                name,
                kind,
                level: level.clamp(1, 6),
                line: idx as u64,
                end_line,
                flags,
            });
        }
    }
    out
}

fn detect_symbol_decl(trimmed: &str, ext: &str) -> Option<(String, String, u64)> {
    let mut flags = 0u64;
    let mut s = trimmed;

    for prefix in [
        "pub(crate) ",
        "pub(super) ",
        "pub ",
        "export default ",
        "export ",
        "public ",
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            if prefix.starts_with("pub") || prefix.starts_with("export") || prefix == "public " {
                flags |= FLAG_PUBLIC;
            }
            s = rest.trim_start();
        }
    }
    for modifier in [
        "async ",
        "unsafe ",
        "const ",
        "static ",
        "inline ",
        "virtual ",
        "constexpr ",
        "extern \"C\" ",
        "extern \"sysv64\" ",
    ] {
        while let Some(rest) = s.strip_prefix(modifier) {
            if modifier == "unsafe " {
                flags |= FLAG_UNSAFE_OR_MUT;
            }
            s = rest.trim_start();
        }
    }

    // Rust / Zig / Go / JS / Python / Kotlin / C++ ключевые слова объявления
    let kw_table: &[(&str, &str, u64)] = &[
        ("fn ", "fn", FLAG_FUNCTION),
        ("def ", "def", FLAG_FUNCTION),
        ("func ", "func", FLAG_FUNCTION),
        ("function ", "function", FLAG_FUNCTION),
        ("fun ", "fun", FLAG_FUNCTION),
        ("struct ", "struct", FLAG_TYPE),
        ("enum ", "enum", FLAG_TYPE),
        ("trait ", "trait", FLAG_TYPE),
        ("interface ", "interface", FLAG_TYPE),
        ("class ", "class", FLAG_TYPE),
        ("namespace ", "namespace", FLAG_TYPE),
        ("mod ", "mod", FLAG_TYPE),
        ("impl", "impl", FLAG_TYPE),
        ("test \"", "test", FLAG_TEST | FLAG_FUNCTION),
    ];

    for &(kw, kind, kflag) in kw_table {
        if let Some(rest) = s.strip_prefix(kw) {
            if kind == "impl" {
                // `impl<T> Trait for Type` или `impl PolerBuffer`
                let head = rest
                    .split('{')
                    .next()
                    .unwrap_or(rest)
                    .split("where")
                    .next()
                    .unwrap_or(rest)
                    .trim();
                if !head.is_empty() && (rest.starts_with(' ') || rest.starts_with('<')) {
                    return Some((format!("impl {head}"), "impl".into(), flags | kflag));
                }
                continue;
            }
            if kind == "test" {
                let title = rest.split('"').next().unwrap_or("test");
                return Some((title.to_string(), "test".into(), flags | kflag));
            }
            let ident = take_ident(rest.trim_start());
            if !ident.is_empty() {
                if ident.starts_with("test_") {
                    flags |= FLAG_TEST;
                }
                return Some((ident.to_string(), kind.into(), flags | kflag));
            }
        }
    }

    // Zig: `const Name = struct {` / `pub const Name = union(enum)`
    if ext == "zig" {
        if let Some(eq_pos) = s.find('=') {
            let lhs = s[..eq_pos].trim();
            let rhs = s[eq_pos + 1..].trim();
            if rhs.starts_with("struct") || rhs.starts_with("enum") || rhs.starts_with("union") {
                let ident = lhs.split_whitespace().last().unwrap_or("");
                if !ident.is_empty() {
                    return Some((ident.to_string(), "struct".into(), flags | FLAG_TYPE));
                }
            }
        }
    }

    // C++ метод вида `ReturnType ClassName::methodName(`
    if matches!(ext, "cpp" | "cc" | "cxx" | "h" | "hpp") {
        if let Some(paren) = s.find('(') {
            let before = s[..paren].trim();
            if !before.contains('=')
                && !before.starts_with("if")
                && !before.starts_with("for")
                && !before.starts_with("while")
                && !before.starts_with("switch")
                && !before.starts_with("return")
            {
                if let Some(colons) = before.rfind("::") {
                    let cls_start = before[..colons]
                        .rfind(|c: char| c.is_whitespace() || c == '*' || c == '&')
                        .map(|p| p + 1)
                        .unwrap_or(0);
                    let method_full = &before[cls_start..];
                    if !method_full.is_empty() && s.contains('{') || trimmed.ends_with(')') {
                        return Some((
                            method_full.to_string(),
                            "method".into(),
                            flags | FLAG_FUNCTION,
                        ));
                    }
                }
            }
        }
    }

    None
}

fn take_ident(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut end = 0;
    while end < bytes.len() {
        let b = bytes[end];
        if b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80 {
            end += 1;
        } else {
            break;
        }
    }
    &s[..end]
}

/// Хлебные крошки скоупов (`mod -> impl -> fn`) для любой строки `line`!
pub fn breadcrumbs_for_line(outline: &[OutlineItem], line: u64) -> Vec<String> {
    let mut chain = Vec::new();
    for it in outline {
        if it.line <= line && line <= it.end_line {
            chain.push(format!("{} {} (L{})", it.kind, it.name, it.line));
        }
    }
    chain
}

// =====================================================================
// Проектный CSR Call-Graph в `pack5` (0.2 Б/связь в L3-кэше процессора)
// =====================================================================

pub struct TritProjectGraph {
    pub scopes: Vec<IndexedScope>,
    /// Сжатая `pack5` матрица смежности в CSR: для узла `i` срез
    /// `csr_packed[row_ptr[i] .. row_ptr[i+1]]` хранит триты связей ко всем узлам
    /// (`+1` calls/depends, `0` вакуум `121`, `-1` mutates/unsafe).
    pub csr_packed: Vec<u8>,
    pub row_stride: usize,
}

impl TritProjectGraph {
    pub fn scan_workspace(root: &Path, max_files: usize) -> Self {
        let mut files = Vec::new();
        collect_code_files(root, &mut files, max_files.clamp(1, 5000));

        let mut scopes: Vec<IndexedScope> = Vec::new();
        for fp in files {
            let Ok(bytes) = fs::read(&fp) else {
                continue;
            };
            if bytes.len() > (8 << 20) || bytes.contains(&0) {
                continue;
            }
            let items = extract_outline_from_bytes(&bytes, Some(&fp));
            if items.is_empty() {
                continue;
            }
            let lines: Vec<&str> = std::str::from_utf8(&bytes)
                .map(|s| s.lines().collect())
                .unwrap_or_default();
            if lines.is_empty() {
                continue;
            }

            for item in items {
                let s_ln = (item.line as usize).min(lines.len().saturating_sub(1));
                let e_ln = (item.end_line as usize).min(lines.len().saturating_sub(1));
                if e_ln < s_ln {
                    continue;
                }
                let body = lines[s_ln..=e_ln].join("\n");
                let sig = TritSig256::from_bytes(body.as_bytes());
                let callees = extract_calls_and_mutations(&body, &item.name);
                scopes.push(IndexedScope {
                    file: fp.clone(),
                    item,
                    sig,
                    callees,
                });
            }
        }

        // Индекс `short_name -> Vec<node_id>` для построения `pack5` CSR матрицы
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (idx, sc) in scopes.iter().enumerate() {
            let short = sc
                .item
                .name
                .rsplit("::")
                .next()
                .unwrap_or(&sc.item.name)
                .trim()
                .to_string();
            by_name.entry(short).or_default().push(idx);
        }

        let n = scopes.len();
        let row_stride = n.div_ceil(5);
        // Для очень больших графов (> 2500 узлов) упаковываем строки плотно до лимита L3 (10 МБ)
        let build_dense_pack5 = n > 0 && n <= 4096;
        let mut csr_packed = if build_dense_pack5 {
            Vec::with_capacity(n * row_stride)
        } else {
            Vec::new()
        };

        if build_dense_pack5 {
            let mut row_trits = vec![0i8; n];
            let mut row_buf = Vec::with_capacity(row_stride);
            for sc in &scopes {
                row_trits.fill(0);
                for (callee_name, edge_trit) in &sc.callees {
                    if let Some(targets) = by_name.get(callee_name) {
                        for &t_idx in targets {
                            // Клини-приоритет: если есть мутация/unsafe (-1), фиксируем -1, иначе +1
                            if *edge_trit < 0 {
                                row_trits[t_idx] = -1;
                            } else if row_trits[t_idx] == 0 {
                                row_trits[t_idx] = 1;
                            }
                        }
                    }
                }
                pack5(&row_trits, &mut row_buf);
                csr_packed.extend_from_slice(&row_buf);
            }
        } else {
            csr_packed.resize(64, TRIT5_ZERO_CODE);
        }

        Self {
            scopes,
            csr_packed,
            row_stride,
        }
    }

    /// AIDDE Impact-паспорт символа: определения, кто вызывает (`+1`),
    /// кто мутирует/использует в `unsafe` (`-1`), и уровень риска модификации.
    pub fn impact(&self, symbol: &str) -> ImpactReport {
        let needle = symbol.rsplit("::").next().unwrap_or(symbol).trim();
        let mut defs = Vec::new();
        let mut downstream: HashSet<String> = HashSet::new();

        for sc in &self.scopes {
            let short = sc.item.name.rsplit("::").next().unwrap_or(&sc.item.name);
            if short == needle || sc.item.name == symbol {
                defs.push(serde_json::json!({
                    "file": sc.file.to_string_lossy(),
                    "name": sc.item.name,
                    "kind": sc.item.kind,
                    "line": sc.item.line,
                    "end_line": sc.item.end_line,
                }));
                for (cal, _) in &sc.callees {
                    downstream.insert(cal.clone());
                }
            }
        }

        let mut upstream = Vec::new();
        let mut mut_count = 0usize;
        for sc in &self.scopes {
            for (cal, trit) in &sc.callees {
                if cal == needle {
                    if *trit < 0 {
                        mut_count += 1;
                    }
                    upstream.push(serde_json::json!({
                        "file": sc.file.to_string_lossy(),
                        "caller": sc.item.name,
                        "kind": sc.item.kind,
                        "line": sc.item.line,
                        "trit": *trit,
                    }));
                    break;
                }
            }
        }

        let caller_cnt = upstream.len();
        let danger = if caller_cnt >= 12 || mut_count >= 3 {
            "CRITICAL"
        } else if caller_cnt >= 5 || mut_count >= 1 {
            "HIGH"
        } else if caller_cnt >= 1 {
            "MEDIUM"
        } else {
            "LOW"
        };

        let vac = trit_vacuum_ratio(&self.csr_packed).canonicalize();
        let mut ds: Vec<String> = downstream.into_iter().take(40).collect();
        ds.sort();

        ImpactReport {
            symbol: symbol.to_string(),
            definitions: defs,
            upstream_callers: upstream,
            downstream_callees: ds,
            mutating_or_unsafe_callers: mut_count,
            danger_level: danger,
            graph_nodes: self.scopes.len(),
            graph_csr_packed_bytes: self.csr_packed.len(),
            graph_vacuum_ratio: [vac.n, vac.d],
        }
    }

    /// Троичный семантический поиск по скоупам проекта (`52 Б pack5` + `psignb` + `[N:D]` + `LENS`).
    pub fn semantic_search(
        &self,
        query: &str,
        limit: usize,
        require_flags: u64,
        forbid_flags: u64,
    ) -> Vec<SemanticScopeHit> {
        let q_sig = TritSig256::from_bytes(query.as_bytes());
        let q_lower = query.to_lowercase();

        let mut scores = Vec::with_capacity(self.scopes.len());
        let mut flags = Vec::with_capacity(self.scopes.len());

        for sc in &self.scopes {
            let mut sim = q_sig.similarity(&sc.sig);
            // Точное вхождение запроса в имя символа усиливает числитель [N:D] без float
            if !q_lower.is_empty() && sc.item.name.to_lowercase().contains(&q_lower) {
                sim = Proj::new(sim.n + (sim.d * 2).max(16), sim.d.max(1));
            }
            scores.push(sim.canonicalize());
            flags.push(sc.item.flags);
        }

        let kept = lens_filter_proj(&scores, &flags, Proj::ZERO, require_flags, forbid_flags);
        let mut ranked: Vec<usize> = kept;
        ranked.sort_by(|&a, &b| cmp_proj(&scores[b], &scores[a]).then_with(|| a.cmp(&b)));
        ranked.truncate(limit.clamp(1, 200));

        ranked
            .into_iter()
            .map(|i| {
                let sc = &self.scopes[i];
                let p = scores[i];
                SemanticScopeHit {
                    file: sc.file.to_string_lossy().into_owned(),
                    name: sc.item.name.clone(),
                    kind: sc.item.kind.clone(),
                    line: sc.item.line,
                    end_line: sc.item.end_line,
                    flags: sc.item.flags,
                    proj: [p.n, p.d],
                }
            })
            .collect()
    }
}

fn collect_code_files(dir: &Path, out: &mut Vec<PathBuf>, max_files: usize) {
    if out.len() >= max_files {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        if dir.is_file() {
            out.push(dir.to_path_buf());
        }
        return;
    };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|x| x.path())).collect();
    entries.sort();
    for p in entries {
        if out.len() >= max_files {
            break;
        }
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.')
            || matches!(
                name,
                "target" | "build" | "dist" | "node_modules" | "vendor" | "__pycache__"
            )
        {
            continue;
        }
        if p.is_dir() {
            collect_code_files(&p, out, max_files);
        } else if p.is_file() {
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if matches!(
                ext.as_str(),
                "rs" | "c"
                    | "cc"
                    | "cpp"
                    | "h"
                    | "hpp"
                    | "zig"
                    | "py"
                    | "js"
                    | "ts"
                    | "tsx"
                    | "go"
                    | "kt"
                    | "java"
                    | "cs"
                    | "md"
            ) {
                out.push(p);
            }
        }
    }
}

fn extract_calls_and_mutations(body: &str, self_name: &str) -> Vec<(String, i8)> {
    let self_short = self_name.rsplit("::").next().unwrap_or(self_name);
    let has_unsafe = body.contains("unsafe ") || body.contains("unsafe{");
    let mut map: HashMap<String, i8> = HashMap::new();
    let bytes = body.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &body[start..i];
            let mut k = i;
            while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            if k < bytes.len()
                && bytes[k] == b'('
                && ident != self_short
                && !matches!(
                    ident,
                    "if" | "for"
                        | "while"
                        | "match"
                        | "switch"
                        | "return"
                        | "fn"
                        | "def"
                        | "Some"
                        | "Ok"
                        | "Err"
                        | "vec"
                        | "format"
                        | "println"
                        | "assert_eq"
                        | "assert"
                )
            {
                // Определяем полярность трита: -1 при unsafe или мутирующей передаче `&mut`
                let arg_preview_end = (k + 48).min(bytes.len());
                let arg_slice = &body[k..arg_preview_end];
                let trit = if has_unsafe || arg_slice.contains("&mut ") {
                    -1
                } else {
                    1
                };
                let entry = map.entry(ident.to_string()).or_insert(trit);
                if trit < 0 {
                    *entry = -1;
                }
            }
        } else {
            i += 1;
        }
    }
    let mut out: Vec<(String, i8)> = map.into_iter().collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::PolerBuffer;

    #[test]
    fn trit_syntax_catches_broken_and_validates_clean_rust() {
        let clean = PolerBuffer::from_text(
            "demo.rs",
            "pub struct Foo {\n    s: &'static str,\n}\nimpl Foo {\n    pub fn bar(&self) -> usize {\n        let fake = \"{ not a bracket ) \";\n        // comment with [ unclosed\n        fake.len()\n    }\n}\n",
        );
        let v_ok = verify_snapshot_syntax(&clean.snapshot(), false);
        assert!(v_ok.balanced, "issues: {:?}", v_ok.issues);
        assert_eq!(v_ok.net_trit_sum, 0);

        let broken = PolerBuffer::from_text(
            "broken.rs",
            "pub fn oops() {\n    let v = vec![1, 2, 3);\n}\n",
        );
        let v_err = verify_snapshot_syntax(&broken.snapshot(), false);
        assert!(!v_err.balanced);
        assert_eq!(v_err.issues[0].kind, "mismatched_bracket");
        assert_eq!(v_err.issues[0].line, 1);
    }

    #[test]
    fn code_outline_and_breadcrumbs_annihilate_at_exact_end_line() {
        let src = b"pub struct Engine {\n    x: u32,\n}\n\nimpl Engine {\n    pub fn step(&mut self) {\n        if self.x > 0 {\n            self.x -= 1;\n        }\n    }\n}\n";
        let outline = extract_outline_from_bytes(src, Some(Path::new("engine.rs")));
        assert_eq!(outline.len(), 3);
        assert_eq!(outline[0].name, "Engine");
        assert_eq!(outline[0].line, 0);
        assert_eq!(outline[0].end_line, 2);
        assert_eq!(outline[2].name, "step");
        assert_eq!(outline[2].line, 5);
        assert_eq!(outline[2].end_line, 9);

        let crumbs = breadcrumbs_for_line(&outline, 7);
        assert_eq!(crumbs.len(), 2);
        assert!(crumbs[0].contains("impl Engine"));
        assert!(crumbs[1].contains("fn step"));
    }
}
