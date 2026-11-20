//! poler-edit :: AI-Native транзакционные операции над `PolerBuffer`
//!
//! * `read_span` — чтение диапазона строк/байт БЕЗ усечения `VIEWPORT_MAX_LINE_BYTES` (8192).
//! * `replace_match` — контент-адресуемая замена (`old_text -> new_text`) поверх SIMD-сканера
//!   с авто-толерантностью CRLF/LF, разрешением неоднозначностей (`near_line` / `occurrence`)
//!   и мгновенным откатом по троичному балансу скобок (`check_syntax: true`, `𝕋=0`).
//! * `apply_batch` — атомарный пакет разнородных правок (один шаг Undo, один инкремент `rev`,
//!   применение строго снизу вверх без сдвига смещений).
//! * `apply_unified_diff` — прямое наложение Unified Diff (`@@ -l,c +l,c @@`) с окном `fuzz`.
//! * `piece_diff` — генерация Unified Diff за `O(pieces)` БЕЗ чтения нетронутых гигабайт `mmap`.
//! * `merge_three_way` — спекулятивное 3-стороннее слияние веток агентов на алгебре Клини `𝕋`.

use crate::buffer::{count_newlines, Piece, PolerBuffer, Src, MAX_PIECE_BYTES};
use crate::search;
use crate::trit_silicon::kleene_merge_trits;
use crate::trit_syntax::verify_snapshot_syntax;
use serde::{Deserialize, Serialize};
use std::io;
use std::ops::Range;
use std::sync::atomic::AtomicBool;

/// Максимальный размер одного чтения `read_span` (64 МиБ).
pub const MAX_READ_SPAN_BYTES: usize = 64 << 20;
/// Максимальный размер полезной нагрузки одной вставки/батча (64 МиБ).
pub const MAX_EDIT_PAYLOAD_BYTES: usize = 64 << 20;

#[derive(Debug, Clone, Serialize)]
pub struct SpanSlice {
    pub start_line: u64,
    pub end_line: u64,
    pub start_byte: u64,
    pub end_byte: u64,
    pub total_bytes: u64,
    pub byte_capped: bool,
    pub rev: u64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MutationReceipt {
    pub rev: u64,
    pub bytes: u64,
    pub lines: Option<u64>,
    pub replacements: usize,
    pub focus_line: u64,
    pub syntax_balanced: bool,
    pub net_trit_sum: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BatchEditOp {
    /// Контент-адресуемая замена подстроки.
    ReplaceMatch {
        old_text: String,
        new_text: String,
        #[serde(default)]
        near_line: Option<u64>,
        #[serde(default)]
        occurrence: Option<usize>,
        #[serde(default)]
        replace_all: bool,
    },
    /// Замена диапазона `(start_line, start_col) .. (end_line, end_col)` (0-based, символы).
    ReplaceRange {
        start_line: u64,
        #[serde(default)]
        start_col: u64,
        end_line: u64,
        #[serde(default)]
        end_col: u64,
        text: String,
    },
    /// Прямая байтовая замена `[start_byte, end_byte)`.
    ReplaceBytes {
        start_byte: u64,
        end_byte: u64,
        text: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct MergeConflict {
    pub base_start_line: usize,
    pub base_end_line: usize,
    pub ours_preview: String,
    pub theirs_preview: String,
}

pub struct MergeResult {
    pub buffer: PolerBuffer,
    pub clean: bool,
    pub applied_ours: usize,
    pub applied_theirs: usize,
    pub conflicts: Vec<MergeConflict>,
}

#[inline]
fn is_markdown_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| matches!(e.to_ascii_lowercase().as_str(), "md" | "markdown" | "txt" | "rst"))
        .unwrap_or(false)
}

impl PolerBuffer {
    /// Точное байтовое смещение `(line, col_chars)` БЕЗ усечения длинных строк на 8192 байтах.
    pub fn byte_of_line_col_exact(&mut self, line: u64, col_chars: u64) -> io::Result<u64> {
        let total = self.total_len();
        if total == 0 {
            return Ok(0);
        }
        let start = self.offset_of_line(line)?;
        if start >= total || col_chars == 0 {
            return Ok(start.min(total));
        }
        let line_end = self.next_newline_from(start).unwrap_or(total);
        // Для обычной строки спускаемся до символа перед `\r\n` / `\n`.
        let mut content_end = line_end;
        if content_end > start && self.byte_at(content_end - 1) == b'\r' {
            content_end -= 1;
        }
        let raw = self.read_range(start, (content_end - start) as usize);
        let s = String::from_utf8_lossy(&raw);
        let mut byte_col = raw.len() as u64;
        for (ci, (bi, _)) in s.char_indices().enumerate() {
            if ci as u64 == col_chars {
                byte_col = bi as u64;
                break;
            }
        }
        Ok((start + byte_col).min(content_end))
    }

    /// Точные `(line, col_chars)` по байтовому смещению без ограничения 8192 байт.
    pub fn line_col_of_byte_exact(&mut self, byte: u64) -> io::Result<(u64, u64)> {
        let total = self.total_len();
        let clamped = byte.min(total);
        let line = self.line_of_offset(clamped)?;
        let start = self.offset_of_line(line)?;
        if clamped <= start {
            return Ok((line, 0));
        }
        let raw = self.read_range(start, (clamped - start) as usize);
        let col = raw.iter().filter(|&&b| (b & 0xC0) != 0x80).count() as u64;
        Ok((line, col))
    }

    /// Чтение окна строк `[start_line .. start_line + line_count)` БЕЗ обрезки 8192 байт/строку.
    pub fn read_span(
        &mut self,
        start_line: u64,
        line_count: usize,
        max_bytes: usize,
    ) -> io::Result<SpanSlice> {
        let cap = max_bytes.clamp(1, MAX_READ_SPAN_BYTES);
        let total = self.total_len();
        if total == 0 || line_count == 0 {
            return Ok(SpanSlice {
                start_line,
                end_line: start_line,
                start_byte: 0,
                end_byte: 0,
                total_bytes: total,
                byte_capped: false,
                rev: self.rev,
                text: String::new(),
            });
        }
        let start_byte = self.offset_of_line(start_line)?.min(total);
        let target_end_line = start_line.saturating_add(line_count as u64);
        let natural_end = self.offset_of_line(target_end_line)?.min(total);
        let span_len = natural_end.saturating_sub(start_byte);
        let (end_byte, byte_capped) = if span_len > cap as u64 {
            (self.snap_to_char(start_byte + cap as u64), true)
        } else {
            (natural_end, false)
        };
        let raw = self.read_range(start_byte, (end_byte - start_byte) as usize);
        let end_line = if end_byte == start_byte {
            start_line
        } else {
            start_line + count_newlines(&raw) as u64
                + u64::from(*raw.last().unwrap_or(&b'\n') != b'\n')
        };
        Ok(SpanSlice {
            start_line,
            end_line,
            start_byte,
            end_byte,
            total_bytes: total,
            byte_capped,
            rev: self.rev,
            text: String::from_utf8_lossy(&raw).into_owned(),
        })
    }

    /// Определить преобладающий перевод строки в буфере (`true` = `\r\n`).
    fn uses_crlf(&self) -> bool {
        let sample = self.read_range(0, 8192);
        if let Some(pos) = memchr::memchr(b'\n', &sample) {
            pos > 0 && sample[pos - 1] == b'\r'
        } else {
            false
        }
    }

    /// Разрешить запрос `replace_match` в список непересекающихся байтовых замен `(start, end, bytes)`.
    fn resolve_match_spans(
        &self,
        old_text: &str,
        new_text: &str,
        near_line: Option<u64>,
        occurrence: Option<usize>,
        replace_all: bool,
    ) -> Result<Vec<(u64, u64, Vec<u8>, u64)>, String> {
        if old_text.is_empty() {
            return Err("invalid_argument: old_text не может быть пустым".to_string());
        }
        if new_text.len() > MAX_EDIT_PAYLOAD_BYTES {
            return Err("payload_too_large: new_text превышает лимит 64 МиБ".to_string());
        }
        let snap = self.snapshot();
        let cancel = AtomicBool::new(false);
        let mut run = search::run_once(&snap, old_text, true, 100_000, &cancel);
        let mut effective_new = new_text.to_string();

        // Авто-толерантность CRLF: если агент прислал LF, а файл на диске в CRLF.
        if run.hits.is_empty()
            && old_text.contains('\n')
            && !old_text.contains("\r\n")
            && self.uses_crlf()
        {
            let crlf_old = old_text.replace('\n', "\r\n");
            let crlf_run = search::run_once(&snap, &crlf_old, true, 100_000, &cancel);
            if !crlf_run.hits.is_empty() {
                run = crlf_run;
                if new_text.contains('\n') && !new_text.contains("\r\n") {
                    effective_new = new_text.replace('\n', "\r\n");
                }
            }
        }

        if run.hits.is_empty() {
            return Err("match_not_found: точное вхождение old_text не найдено в буфере".to_string());
        }

        let new_bytes = effective_new.into_bytes();
        if replace_all {
            return Ok(run
                .hits
                .into_iter()
                .map(|h| (h.byte, h.byte + h.len as u64, new_bytes.clone(), h.line))
                .collect());
        }

        if let Some(occ) = occurrence {
            if occ == 0 || occ > run.hits.len() {
                return Err(format!(
                    "occurrence_out_of_range: запрошено вхождение #{occ}, всего найдено {}",
                    run.hits.len()
                ));
            }
            let h = &run.hits[occ - 1];
            return Ok(vec![(h.byte, h.byte + h.len as u64, new_bytes, h.line)]);
        }

        if run.hits.len() == 1 {
            let h = &run.hits[0];
            return Ok(vec![(h.byte, h.byte + h.len as u64, new_bytes, h.line)]);
        }

        // Несколько совпадений: пробуем однозначно привязать по `near_line`.
        if let Some(target_line) = near_line {
            let mut by_dist: Vec<(u64, usize)> = run
                .hits
                .iter()
                .enumerate()
                .map(|(idx, h)| (h.line.abs_diff(target_line), idx))
                .collect();
            by_dist.sort_unstable();
            let (best_dist, best_idx) = by_dist[0];
            let second_dist = by_dist[1].0;
            if best_dist <= 20 && second_dist.saturating_sub(best_dist) >= 3 {
                let h = &run.hits[best_idx];
                return Ok(vec![(h.byte, h.byte + h.len as u64, new_bytes, h.line)]);
            }
        }

        let sample_lines: Vec<u64> = run.hits.iter().take(12).map(|h| h.line).collect();
        Err(format!(
            "ambiguous_match: найдено {} вхождений old_text (строки {:?}) — расширьте контекст или укажите near_line / occurrence",
            run.hits.len(),
            sample_lines
        ))
    }

    /// Низкоуровневый атомарный коммит набора замен `(start_byte, end_byte, new_bytes)`
    /// в один Undo-шаг с опциональным синтаксическим барьером `𝕋=0`.
    fn apply_resolved_spans(
        &mut self,
        mut spans: Vec<(u64, u64, Vec<u8>)>,
        check_syntax: bool,
    ) -> Result<MutationReceipt, String> {
        let total = self.total_len();
        for (s, e, rep) in &mut spans {
            if *s > *e || *e > total {
                return Err(format!(
                    "out_of_bounds: диапазон [{s}, {e}) выходит за пределы документа ({total} байт)"
                ));
            }
            if rep.len() > MAX_EDIT_PAYLOAD_BYTES {
                return Err("payload_too_large: замена превышает 64 МиБ".to_string());
            }
            *s = self.snap_to_char(*s);
            *e = self.snap_forward_char(*e);
        }

        spans.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        for pair in spans.windows(2) {
            if pair[0].1 > pair[1].0 {
                return Err(format!(
                    "overlapping_edits: пересекающиеся диапазоны правок [{}, {}) и [{}, {})",
                    pair[0].0, pair[0].1, pair[1].0, pair[1].1
                ));
            }
        }

        let is_md = is_markdown_path(&self.path);
        let pre_balanced = if check_syntax {
            verify_snapshot_syntax(&self.snapshot(), is_md).balanced
        } else {
            false
        };

        if spans.is_empty() {
            let v = verify_snapshot_syntax(&self.snapshot(), is_md);
            return Ok(MutationReceipt {
                rev: self.rev,
                bytes: self.total_len(),
                lines: self.lines_if_known(),
                replacements: 0,
                focus_line: 0,
                syntax_balanced: v.balanced,
                net_trit_sum: v.net_trit_sum,
            });
        }

        let first_start_byte = spans[0].0;
        let replacements = spans.len();
        let was_dirty = self.dirty;
        let prev_rev = self.rev;

        self.push_undo();

        // Применяем строго СНИЗУ ВВЕРХ (от старших байтовых смещений к младшим),
        // чтобы все предыдущие смещения оставались инвариантными.
        for (s, e, rep) in spans.into_iter().rev() {
            let mut added: Vec<Piece> = Vec::new();
            let mut o = 0usize;
            while o < rep.len() {
                let take = (rep.len() - o).min(MAX_PIECE_BYTES as usize);
                let (buf, start) = self.append_edit(&rep[o..o + take]);
                added.push(Piece {
                    src: Src::Edit { buf, start },
                    len: take as u32,
                    nl: Some(count_newlines(&rep[o..o + take]) as u32),
                });
                o += take;
            }
            let known_before = self.known_prefix;
            let a = self.split_at(s);
            let b = if e > s { self.split_at(e) } else { a };
            self.pieces.splice(a..b, added);
            self.rebuild_cum();
            self.reline_from(a.min(known_before).saturating_sub(1));
        }

        self.dirty = true;
        self.rev = self.rev.wrapping_add(1);

        let verdict = verify_snapshot_syntax(&self.snapshot(), is_md);
        if check_syntax && pre_balanced && !verdict.balanced {
            // Мгновенный zero-copy откат Piece-Table без загрязнения стека Redo!
            if let Some(snap) = self.undo.pop() {
                self.restore(snap);
            }
            self.dirty = was_dirty;
            self.rev = prev_rev;
            let detail = verdict
                .issues
                .first()
                .map(|iss| format!("строка {}, кол {}: {}", iss.line, iss.col, iss.message))
                .unwrap_or_else(|| format!("net_trit_sum={}", verdict.net_trit_sum));
            return Err(format!(
                "trit_syntax_broken: правка откатана атомарно — нарушен баланс скобок 𝕋≠0 ({detail})"
            ));
        }

        let focus_line = self
            .line_of_offset(first_start_byte.min(self.total_len()))
            .unwrap_or(0);

        Ok(MutationReceipt {
            rev: self.rev,
            bytes: self.total_len(),
            lines: self.lines_if_known(),
            replacements,
            focus_line,
            syntax_balanced: verdict.balanced,
            net_trit_sum: verdict.net_trit_sum,
        })
    }

    /// Контент-адресуемая замена подстроки для ИИ-агентов.
    pub fn replace_match(
        &mut self,
        old_text: &str,
        new_text: &str,
        near_line: Option<u64>,
        occurrence: Option<usize>,
        replace_all: bool,
        check_syntax: bool,
    ) -> Result<MutationReceipt, String> {
        let resolved =
            self.resolve_match_spans(old_text, new_text, near_line, occurrence, replace_all)?;
        let spans = resolved
            .into_iter()
            .map(|(s, e, b, _line)| (s, e, b))
            .collect();
        self.apply_resolved_spans(spans, check_syntax)
    }

    /// Атомарный пакет правок (все или ничего, один шаг Undo, один инкремент `rev`).
    pub fn apply_batch(
        &mut self,
        ops: &[BatchEditOp],
        check_syntax: bool,
    ) -> Result<MutationReceipt, String> {
        if ops.is_empty() {
            return self.apply_resolved_spans(Vec::new(), check_syntax);
        }
        let mut spans: Vec<(u64, u64, Vec<u8>)> = Vec::with_capacity(ops.len());
        for (idx, op) in ops.iter().enumerate() {
            match op {
                BatchEditOp::ReplaceMatch {
                    old_text,
                    new_text,
                    near_line,
                    occurrence,
                    replace_all,
                } => {
                    let sub = self
                        .resolve_match_spans(
                            old_text,
                            new_text,
                            *near_line,
                            *occurrence,
                            *replace_all,
                        )
                        .map_err(|e| format!("op[{idx}]: {e}"))?;
                    for (s, e, b, _) in sub {
                        spans.push((s, e, b));
                    }
                }
                BatchEditOp::ReplaceRange {
                    start_line,
                    start_col,
                    end_line,
                    end_col,
                    text,
                } => {
                    let s = self
                        .byte_of_line_col_exact(*start_line, *start_col)
                        .map_err(|e| format!("op[{idx}]: {e}"))?;
                    let e = self
                        .byte_of_line_col_exact(*end_line, *end_col)
                        .map_err(|e| format!("op[{idx}]: {e}"))?;
                    if s > e {
                        return Err(format!(
                            "op[{idx}]: invalid_range ({start_line}:{start_col}) > ({end_line}:{end_col})"
                        ));
                    }
                    spans.push((s, e, text.as_bytes().to_vec()));
                }
                BatchEditOp::ReplaceBytes { start_byte, end_byte, text } => {
                    spans.push((*start_byte, *end_byte, text.as_bytes().to_vec()));
                }
            }
        }
        self.apply_resolved_spans(spans, check_syntax)
    }

    /// Прямое атомарное применение стандартного Unified Diff (`@@ -a,b +c,d @@`) с допуском `±fuzz` строк.
    pub fn apply_unified_diff(
        &mut self,
        diff_text: &str,
        fuzz: usize,
        check_syntax: bool,
    ) -> Result<MutationReceipt, String> {
        let hunks = parse_unified_hunks(diff_text)?;
        if hunks.is_empty() {
            return Err("invalid_diff: в патче не найдено ни одного ханка @@ ... @@".to_string());
        }
        let crlf = self.uses_crlf();
        let line_sep = if crlf { "\r\n" } else { "\n" };

        let mut spans: Vec<(u64, u64, Vec<u8>)> = Vec::with_capacity(hunks.len());
        let mut min_next_line: u64 = 0;

        for (h_idx, hunk) in hunks.iter().enumerate() {
            // Номер строки в diff — 1-based; для чистого добавления `old_count == 0` вставляем после `old_start`.
            let expected_0 = if hunk.old_lines.is_empty() {
                hunk.old_start
            } else {
                hunk.old_start.saturating_sub(1)
            };

            let matched_line = self
                .locate_hunk_line(&hunk.old_lines, expected_0, min_next_line, fuzz)
                .ok_or_else(|| {
                    format!(
                        "diff_context_mismatch: ханк #{} (около строки {}) не совпадает с содержимым буфера",
                        h_idx + 1,
                        hunk.old_start
                    )
                })?;

            let start_byte = self
                .offset_of_line(matched_line)
                .map_err(|e| e.to_string())?;
            let end_line = matched_line + hunk.old_lines.len() as u64;
            let mut end_byte = self.offset_of_line(end_line).map_err(|e| e.to_string())?;

            // Если старый ханк доходил до самого конца файла без финального `\n`, не добавляем лишний перевод.
            let old_had_trailing_nl = if hunk.old_lines.is_empty() {
                true
            } else {
                end_byte > start_byte && self.byte_at(end_byte - 1) == b'\n'
            };

            let mut rep = String::new();
            for (li, l) in hunk.new_lines.iter().enumerate() {
                rep.push_str(l);
                let is_last = li + 1 == hunk.new_lines.len();
                if !is_last || (old_had_trailing_nl && !hunk.new_no_eof_nl) {
                    rep.push_str(line_sep);
                }
            }
            if hunk.old_lines.is_empty() && hunk.new_lines.is_empty() {
                end_byte = start_byte;
            }

            spans.push((start_byte, end_byte, rep.into_bytes()));
            min_next_line = end_line;
        }

        self.apply_resolved_spans(spans, check_syntax)
    }

    fn locate_hunk_line(
        &mut self,
        old_lines: &[String],
        expected_0: u64,
        min_line: u64,
        fuzz: usize,
    ) -> Option<u64> {
        if old_lines.is_empty() {
            return Some(expected_0.max(min_line));
        }
        // Генерируем кандидатов: 0, -1, +1, -2, +2, ... в пределах `fuzz`.
        let mut candidates = Vec::with_capacity(2 * fuzz + 1);
        if expected_0 >= min_line {
            candidates.push(expected_0);
        }
        for delta in 1..=(fuzz as u64) {
            if let Some(lo) = expected_0.checked_sub(delta) {
                if lo >= min_line {
                    candidates.push(lo);
                }
            }
            let hi = expected_0.saturating_add(delta);
            if hi >= min_line {
                candidates.push(hi);
            }
        }
        for cand in candidates {
            if self.lines_match_at(cand, old_lines) {
                return Some(cand);
            }
        }
        None
    }

    fn lines_match_at(&mut self, start_line: u64, expected: &[String]) -> bool {
        let Ok(slice) = self.read_span(start_line, expected.len(), 16 << 20) else {
            return false;
        };
        let actual: Vec<&str> = split_lines_stripped(&slice.text);
        if actual.len() != expected.len() {
            return false;
        }
        actual
            .iter()
            .zip(expected.iter())
            .all(|(a, b)| *a == b.as_str())
    }

    /// Сгенерировать стандартный Unified Diff относительно открытого на диске `mmap`
    /// за `O(pieces)` БЕЗ сканирования нетронутых гигабайт оригинала!
    pub fn piece_diff(&mut self, context_lines: usize) -> io::Result<String> {
        let display = self.path.to_string_lossy().replace('\\', "/");
        let Some(orig_mmap) = self.mmap.clone() else {
            if !self.dirty || self.total_len() == 0 {
                return Ok(String::new());
            }
            let new_bytes = self.read_range(0, (self.total_len() as usize).min(MAX_READ_SPAN_BYTES));
            let new_text = String::from_utf8_lossy(&new_bytes);
            let new_lines = split_lines_stripped(&new_text);
            let mut out = format!("--- a/{display}\n+++ b/{display}\n");
            out.push_str(&format!("@@ -0,0 +1,{} @@\n", new_lines.len()));
            for l in new_lines {
                out.push('+');
                out.push_str(l);
                out.push('\n');
            }
            return Ok(out);
        };

        let orig: &[u8] = &orig_mmap[..];
        let orig_total = orig.len() as u64;

        // 1. Выделяем грязные швы `(orig_range, new_range)` за O(pieces),
        //    пропуская нетронутые куски `Src::Orig` за O(1) без чтения памяти!
        let mut seams: Vec<(Range<u64>, Range<u64>)> = Vec::new();
        let mut orig_pos = 0u64;
        let mut new_pos = 0u64;
        let mut active_seam: Option<(u64, u64)> = None; // (orig_start, new_start)

        for p in &self.pieces {
            let plen = p.len as u64;
            match p.src {
                Src::Orig { start } if start >= orig_pos => {
                    if let Some((os, ns)) = active_seam.take() {
                        seams.push((os..start, ns..new_pos));
                    } else if start > orig_pos {
                        // Чистое удаление куска оригинала `[orig_pos .. start)`.
                        seams.push((orig_pos..start, new_pos..new_pos));
                    }
                    orig_pos = start + plen;
                    new_pos += plen;
                }
                _ => {
                    if active_seam.is_none() {
                        active_seam = Some((orig_pos, new_pos));
                    }
                    new_pos += plen;
                }
            }
        }
        if let Some((os, ns)) = active_seam.take() {
            seams.push((os..orig_total, ns..new_pos));
        } else if orig_pos < orig_total {
            seams.push((orig_pos..orig_total, new_pos..new_pos));
        }

        if seams.is_empty() {
            return Ok(String::new());
        }

        // 2. Расширяем каждый шов до границ строк ± `context_lines` в `orig`
        //    и синхронно в `new` (за пределами шва байты 100% идентичны!).
        let mut windows: Vec<(Range<u64>, Range<u64>)> = Vec::with_capacity(seams.len());
        for (orng, nrng) in seams {
            let exp_orig_lo = expand_lines_back(orig, orng.start, context_lines);
            let exp_orig_hi = expand_lines_fwd(orig, orng.end, context_lines);
            let prefix_len = orng.start - exp_orig_lo;
            let suffix_len = exp_orig_hi - orng.end;
            let exp_new_lo = nrng.start.saturating_sub(prefix_len);
            let exp_new_hi = (nrng.end + suffix_len).min(self.total_len());

            if let Some((prev_o, prev_n)) = windows.last_mut() {
                if exp_orig_lo <= prev_o.end {
                    prev_o.end = prev_o.end.max(exp_orig_hi);
                    prev_n.end = prev_n.end.max(exp_new_hi);
                    continue;
                }
            }
            windows.push((exp_orig_lo..exp_orig_hi, exp_new_lo..exp_new_hi));
        }

        // 3. Строим LCS-дифф только внутри крошечных локальных окон вокруг швов!
        let mut diff_out = String::new();
        let mut cum_nl_delta: i64 = 0;

        for (orng, nrng) in windows {
            let orig_slice = &orig[orng.start as usize..orng.end as usize];
            let new_vec = self.read_range(nrng.start, (nrng.end - nrng.start) as usize);

            let new_start_line_0 = self.line_of_offset(nrng.start)?;
            let orig_start_line_0 = (new_start_line_0 as i64 - cum_nl_delta).max(0) as u64;

            let orig_nl = count_newlines(orig_slice) as i64;
            let new_nl = count_newlines(&new_vec) as i64;
            cum_nl_delta += new_nl - orig_nl;

            if orig_slice == new_vec.as_slice() {
                continue;
            }

            let orig_str = String::from_utf8_lossy(orig_slice);
            let new_str = String::from_utf8_lossy(&new_vec);
            let orig_lines = split_lines_stripped(&orig_str);
            let new_lines = split_lines_stripped(&new_str);

            let ops = lcs_diff_lines(&orig_lines, &new_lines);
            if ops.iter().all(|op| matches!(op, DiffLineOp::Equal(_))) {
                continue;
            }

            if diff_out.is_empty() {
                diff_out.push_str(&format!("--- a/{display}\n+++ b/{display}\n"));
            }
            let o_start_1 = if orig_lines.is_empty() {
                orig_start_line_0
            } else {
                orig_start_line_0 + 1
            };
            let n_start_1 = if new_lines.is_empty() {
                new_start_line_0
            } else {
                new_start_line_0 + 1
            };
            diff_out.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                o_start_1,
                orig_lines.len(),
                n_start_1,
                new_lines.len()
            ));
            for op in ops {
                match op {
                    DiffLineOp::Equal(s) => {
                        diff_out.push(' ');
                        diff_out.push_str(s);
                        diff_out.push('\n');
                    }
                    DiffLineOp::Delete(s) => {
                        diff_out.push('-');
                        diff_out.push_str(s);
                        diff_out.push('\n');
                    }
                    DiffLineOp::Insert(s) => {
                        diff_out.push('+');
                        diff_out.push_str(s);
                        diff_out.push('\n');
                    }
                }
            }
        }

        Ok(diff_out)
    }
}

/// Трёхстороннее спекулятивное слияние веток (`base`, `ours`, `theirs`)
/// на троичной алгебре Клини (`0` вакуум, `+1` детерминированное изменение, `-1` коллизия).
pub fn merge_three_way(
    base: &PolerBuffer,
    ours: &PolerBuffer,
    theirs: &PolerBuffer,
    insert_markers: bool,
) -> Result<MergeResult, String> {
    const MAX_MERGE_BYTES: usize = 32 << 20;
    if base.total_len() as usize > MAX_MERGE_BYTES
        || ours.total_len() as usize > MAX_MERGE_BYTES
        || theirs.total_len() as usize > MAX_MERGE_BYTES
    {
        return Err("merge_too_large: 3-way merge поддерживает буферы до 32 МиБ".to_string());
    }

    let base_raw = base.read_range(0, base.total_len() as usize);
    let ours_raw = ours.read_range(0, ours.total_len() as usize);
    let theirs_raw = theirs.read_range(0, theirs.total_len() as usize);

    let crlf = memchr::memchr(b'\n', &base_raw)
        .map(|i| i > 0 && base_raw[i - 1] == b'\r')
        .unwrap_or(false);
    let sep = if crlf { "\r\n" } else { "\n" };
    let trailing_nl = base_raw.last() == Some(&b'\n')
        || ours_raw.last() == Some(&b'\n')
        || theirs_raw.last() == Some(&b'\n');

    let base_s = String::from_utf8_lossy(&base_raw);
    let ours_s = String::from_utf8_lossy(&ours_raw);
    let theirs_s = String::from_utf8_lossy(&theirs_raw);

    let b_lines = split_lines_stripped(&base_s);
    let o_lines = split_lines_stripped(&ours_s);
    let t_lines = split_lines_stripped(&theirs_s);

    let o_edits = extract_line_replacements(&b_lines, &o_lines);
    let t_edits = extract_line_replacements(&b_lines, &t_lines);

    // Проецируем строки базы в троичные векторы изменения `delta_a`, `delta_b`
    // и прогоняем через аппаратное ядро `kleene_merge_trits` для быстрой диагностики.
    let mut delta_o = vec![0i8; b_lines.len().max(1)];
    let mut delta_t = vec![0i8; b_lines.len().max(1)];
    for e in &o_edits {
        for slot in &mut delta_o[e.base_range.start.min(b_lines.len())..e.base_range.end.min(b_lines.len())] {
            *slot = 1;
        }
    }
    for e in &t_edits {
        for slot in &mut delta_t[e.base_range.start.min(b_lines.len())..e.base_range.end.min(b_lines.len())] {
            *slot = -1;
        }
    }
    let _trit_probe = kleene_merge_trits(&delta_o, &delta_t);

    let mut out_lines: Vec<String> = Vec::with_capacity(b_lines.len().max(o_lines.len()));
    let mut conflicts: Vec<MergeConflict> = Vec::new();
    let mut applied_ours = 0usize;
    let mut applied_theirs = 0usize;

    let mut bi = 0usize;
    let mut oi = 0usize;
    let mut ti = 0usize;

    while bi <= b_lines.len() {
        let next_o = o_edits.get(oi);
        let next_t = t_edits.get(ti);

        let next_change_start = match (next_o, next_t) {
            (Some(o), Some(t)) => o.base_range.start.min(t.base_range.start),
            (Some(o), None) => o.base_range.start,
            (None, Some(t)) => t.base_range.start,
            (None, None) => break,
        };

        if bi < next_change_start {
            for &line in &b_lines[bi..next_change_start] {
                out_lines.push(line.to_string());
            }
            bi = next_change_start;
        }

        match (o_edits.get(oi), t_edits.get(ti)) {
            (Some(o), Some(t)) if ranges_conflict(&o.base_range, &t.base_range) => {
                // Объединяем каскадно все пересекающиеся правки обеих сторон в кластер.
                let mut cluster_start = o.base_range.start.min(t.base_range.start);
                let mut cluster_end = o.base_range.end.max(t.base_range.end);
                let o_start_idx = oi;
                let t_start_idx = ti;
                oi += 1;
                ti += 1;
                loop {
                    let mut grew = false;
                    while let Some(no) = o_edits.get(oi) {
                        if no.base_range.start < cluster_end {
                            cluster_start = cluster_start.min(no.base_range.start);
                            cluster_end = cluster_end.max(no.base_range.end);
                            oi += 1;
                            grew = true;
                        } else {
                            break;
                        }
                    }
                    while let Some(nt) = t_edits.get(ti) {
                        if nt.base_range.start < cluster_end {
                            cluster_start = cluster_start.min(nt.base_range.start);
                            cluster_end = cluster_end.max(nt.base_range.end);
                            ti += 1;
                            grew = true;
                        } else {
                            break;
                        }
                    }
                    if !grew {
                        break;
                    }
                }

                let ours_block = reconstruct_side(&b_lines, cluster_start..cluster_end, &o_edits[o_start_idx..oi]);
                let theirs_block = reconstruct_side(&b_lines, cluster_start..cluster_end, &t_edits[t_start_idx..ti]);

                if ours_block == theirs_block {
                    // Идентичная правка с обеих сторон — когнитивный резонанс (без конфликта!).
                    out_lines.extend(ours_block);
                    applied_ours += 1;
                } else {
                    conflicts.push(MergeConflict {
                        base_start_line: cluster_start,
                        base_end_line: cluster_end,
                        ours_preview: ours_block.join("\n"),
                        theirs_preview: theirs_block.join("\n"),
                    });
                    if insert_markers {
                        out_lines.push("<<<<<<< OURS".to_string());
                        out_lines.extend(ours_block);
                        out_lines.push("=======".to_string());
                        out_lines.extend(theirs_block);
                        out_lines.push(">>>>>>> THEIRS".to_string());
                    } else {
                        for &line in &b_lines[cluster_start..cluster_end] {
                            out_lines.push(line.to_string());
                        }
                    }
                }
                bi = cluster_end;
            }
            (Some(o), _) if o.base_range.start == bi => {
                out_lines.extend(o.replacement.iter().cloned());
                bi = o.base_range.end;
                oi += 1;
                applied_ours += 1;
            }
            (_, Some(t)) if t.base_range.start == bi => {
                out_lines.extend(t.replacement.iter().cloned());
                bi = t.base_range.end;
                ti += 1;
                applied_theirs += 1;
            }
            _ => break,
        }
    }

    if bi < b_lines.len() {
        for &line in &b_lines[bi..] {
            out_lines.push(line.to_string());
        }
    }

    let mut merged_text = out_lines.join(sep);
    if trailing_nl && !merged_text.is_empty() {
        merged_text.push_str(sep);
    }

    let mut merged_buf = base.fork();
    if merged_buf.total_len() > 0 {
        let _ = merged_buf.delete(0, merged_buf.total_len());
    }
    if !merged_text.is_empty() {
        let _ = merged_buf.insert(0, &merged_text);
    }
    merged_buf.undo.clear();
    merged_buf.redo.clear();
    merged_buf.rev = base.rev().max(ours.rev()).max(theirs.rev()).wrapping_add(1);

    Ok(MergeResult {
        clean: conflicts.is_empty(),
        buffer: merged_buf,
        applied_ours,
        applied_theirs,
        conflicts,
    })
}

// =====================================================================
// Внутренние алгоритмы Unified Diff & LCS
// =====================================================================

struct ParsedHunk {
    old_start: u64,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    new_no_eof_nl: bool,
}

fn parse_unified_hunks(diff_text: &str) -> Result<Vec<ParsedHunk>, String> {
    let mut hunks: Vec<ParsedHunk> = Vec::new();
    let mut cur: Option<ParsedHunk> = None;
    let mut last_side: u8 = 0; // b'-', b'+', b' '

    for raw_line in diff_text.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            if cur.is_none() {
                continue;
            }
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            if let Some(prev) = cur.take() {
                hunks.push(prev);
            }
            let header_end = rest
                .find(" @@")
                .ok_or_else(|| format!("invalid_diff_header: {line}"))?;
            let coords = &rest[..header_end];
            let mut parts = coords.split_whitespace();
            let old_spec = parts
                .next()
                .and_then(|s| s.strip_prefix('-'))
                .ok_or_else(|| format!("invalid_diff_old_range: {line}"))?;
            let old_start: u64 = old_spec
                .split(',')
                .next()
                .unwrap_or("1")
                .parse()
                .map_err(|_| format!("invalid_diff_line_num: {old_spec}"))?;
            cur = Some(ParsedHunk {
                old_start,
                old_lines: Vec::new(),
                new_lines: Vec::new(),
                new_no_eof_nl: false,
            });
            last_side = 0;
            continue;
        }

        if let Some(h) = cur.as_mut() {
            if line.starts_with('\\') {
                if last_side == b'+' || last_side == b' ' {
                    h.new_no_eof_nl = true;
                }
                continue;
            }
            if let Some(s) = line.strip_prefix(' ') {
                h.old_lines.push(s.to_string());
                h.new_lines.push(s.to_string());
                last_side = b' ';
            } else if let Some(s) = line.strip_prefix('-') {
                h.old_lines.push(s.to_string());
                last_side = b'-';
            } else if let Some(s) = line.strip_prefix('+') {
                h.new_lines.push(s.to_string());
                last_side = b'+';
            } else if line.is_empty() {
                // Некоторые генераторы diff опускают пробел на пустых контекстных строках.
                h.old_lines.push(String::new());
                h.new_lines.push(String::new());
                last_side = b' ';
            }
        }
    }
    if let Some(prev) = cur.take() {
        hunks.push(prev);
    }
    Ok(hunks)
}

fn split_lines_stripped(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    let trimmed = s.strip_suffix('\n').unwrap_or(s);
    trimmed
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect()
}

fn expand_lines_back(buf: &[u8], off: u64, context_lines: usize) -> u64 {
    let pos = (off as usize).min(buf.len());
    if pos == 0 {
        return 0;
    }
    // Сначала доходим до начала текущей строки (сразу после ближайшего `\n` слева),
    // затем отступаем ещё на `context_lines` переводов строки назад.
    let mut need = context_lines + 1;
    for nl_idx in memchr::memrchr_iter(b'\n', &buf[..pos]) {
        need -= 1;
        if need == 0 {
            return (nl_idx + 1) as u64;
        }
    }
    0
}

fn expand_lines_fwd(buf: &[u8], off: u64, context_lines: usize) -> u64 {
    let pos = (off as usize).min(buf.len());
    if pos >= buf.len() {
        return buf.len() as u64;
    }
    // Если `off` уже стоит сразу после `\n`, нам нужно `context_lines` строк вперёд,
    // иначе сначала закрываем текущую затронутую строку (+1).
    let at_line_start = pos == 0 || buf[pos - 1] == b'\n';
    let mut need = context_lines + usize::from(!at_line_start);
    if need == 0 {
        return pos as u64;
    }
    for rel in memchr::memchr_iter(b'\n', &buf[pos..]) {
        need -= 1;
        if need == 0 {
            return (pos + rel + 1) as u64;
        }
    }
    buf.len() as u64
}

enum DiffLineOp<'a> {
    Equal(&'a str),
    Delete(&'a str),
    Insert(&'a str),
}

fn lcs_diff_lines<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<DiffLineOp<'a>> {
    // Быстрое отсечение общих префикса и суффикса.
    let mut pre = 0usize;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0usize;
    while suf < (a.len() - pre)
        && suf < (b.len() - pre)
        && a[a.len() - 1 - suf] == b[b.len() - 1 - suf]
    {
        suf += 1;
    }

    let mut out: Vec<DiffLineOp<'a>> = Vec::with_capacity(a.len() + b.len());
    for &s in &a[..pre] {
        out.push(DiffLineOp::Equal(s));
    }

    let am = &a[pre..a.len() - suf];
    let bm = &b[pre..b.len() - suf];

    if am.is_empty() {
        for &s in bm {
            out.push(DiffLineOp::Insert(s));
        }
    } else if bm.is_empty() {
        for &s in am {
            out.push(DiffLineOp::Delete(s));
        }
    } else if am.len().saturating_mul(bm.len()) <= 4_000_000 {
        // Точный DP LCS на хешах строк.
        let ah: Vec<u64> = am.iter().map(|s| fnv1a(s.as_bytes())).collect();
        let bh: Vec<u64> = bm.iter().map(|s| fnv1a(s.as_bytes())).collect();
        let cols = bm.len() + 1;
        let mut dp = vec![0u32; (am.len() + 1) * cols];
        for i in (0..am.len()).rev() {
            for j in (0..bm.len()).rev() {
                dp[i * cols + j] = if ah[i] == bh[j] && am[i] == bm[j] {
                    dp[(i + 1) * cols + (j + 1)] + 1
                } else {
                    dp[(i + 1) * cols + j].max(dp[i * cols + (j + 1)])
                };
            }
        }
        let (mut i, mut j) = (0usize, 0usize);
        while i < am.len() && j < bm.len() {
            if ah[i] == bh[j] && am[i] == bm[j] {
                out.push(DiffLineOp::Equal(am[i]));
                i += 1;
                j += 1;
            } else if dp[(i + 1) * cols + j] >= dp[i * cols + (j + 1)] {
                out.push(DiffLineOp::Delete(am[i]));
                i += 1;
            } else {
                out.push(DiffLineOp::Insert(bm[j]));
                j += 1;
            }
        }
        while i < am.len() {
            out.push(DiffLineOp::Delete(am[i]));
            i += 1;
        }
        while j < bm.len() {
            out.push(DiffLineOp::Insert(bm[j]));
            j += 1;
        }
    } else {
        for &s in am {
            out.push(DiffLineOp::Delete(s));
        }
        for &s in bm {
            out.push(DiffLineOp::Insert(s));
        }
    }

    for &s in &a[a.len() - suf..] {
        out.push(DiffLineOp::Equal(s));
    }
    out
}

#[derive(Debug, Clone)]
struct LineEdit {
    base_range: Range<usize>,
    replacement: Vec<String>,
}

fn extract_line_replacements(base: &[&str], side: &[&str]) -> Vec<LineEdit> {
    let ops = lcs_diff_lines(base, side);
    let mut edits: Vec<LineEdit> = Vec::new();
    let mut bi = 0usize;
    let mut cur_start: Option<usize> = None;
    let mut cur_rep: Vec<String> = Vec::new();

    for op in ops {
        match op {
            DiffLineOp::Equal(_) => {
                if let Some(st) = cur_start.take() {
                    edits.push(LineEdit {
                        base_range: st..bi,
                        replacement: std::mem::take(&mut cur_rep),
                    });
                }
                bi += 1;
            }
            DiffLineOp::Delete(_) => {
                if cur_start.is_none() {
                    cur_start = Some(bi);
                }
                bi += 1;
            }
            DiffLineOp::Insert(s) => {
                if cur_start.is_none() {
                    cur_start = Some(bi);
                }
                cur_rep.push(s.to_string());
            }
        }
    }
    if let Some(st) = cur_start.take() {
        edits.push(LineEdit {
            base_range: st..bi,
            replacement: cur_rep,
        });
    }
    edits
}

fn ranges_conflict(a: &Range<usize>, b: &Range<usize>) -> bool {
    if a.is_empty() && b.is_empty() {
        a.start == b.start
    } else if a.is_empty() {
        a.start >= b.start && a.start <= b.end
    } else if b.is_empty() {
        b.start >= a.start && b.start <= a.end
    } else {
        a.start < b.end && b.start < a.end
    }
}

fn reconstruct_side(base: &[&str], cluster: Range<usize>, edits: &[LineEdit]) -> Vec<String> {
    let mut out = Vec::new();
    let mut pos = cluster.start;
    for e in edits {
        if e.base_range.start > pos {
            for &l in &base[pos..e.base_range.start] {
                out.push(l.to_string());
            }
        }
        out.extend(e.replacement.iter().cloned());
        pos = e.base_range.end;
    }
    if pos < cluster.end {
        for &l in &base[pos..cluster.end] {
            out.push(l.to_string());
        }
    }
    out
}

#[inline]
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpfile(name: &str, content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(name);
        std::fs::write(&p, content).unwrap();
        (dir, p)
    }

    #[test]
    fn replace_match_crlf_and_trit_syntax_rollback() {
        let (_d, p) = tmpfile(
            "demo.rs",
            "pub fn calc(x: i64) -> i64 {\r\n    let y = x + 1;\r\n    y * 2\r\n}\r\n",
        );
        let mut b = PolerBuffer::open(&p).unwrap();
        let rev0 = b.rev();

        // 1. Агент шлёт LF-шаблон в CRLF-файл с проверкой баланса скобок `𝕋=0`:
        let rec = b
            .replace_match("    let y = x + 1;\n    y * 2", "    (x + 1) * 3", None, None, false, true)
            .unwrap();
        assert_eq!(rec.rev, rev0 + 1);
        assert!(rec.syntax_balanced);
        assert_eq!(rec.net_trit_sum, 0);
        let full = String::from_utf8(b.read_range(0, usize::MAX)).unwrap();
        assert!(full.contains("\r\n    (x + 1) * 3\r\n"));

        // 2. Агент пытается внести сломанную скобку `((x + 1) * 3` при `check_syntax = true`:
        let prev_rev = b.rev();
        let bad = b.replace_match("(x + 1) * 3", "((x + 1) * 3", None, None, false, true);
        assert!(bad.is_err());
        assert!(bad.unwrap_err().starts_with("trit_syntax_broken:"));
        // Буфер и ревизия откатились атомарно!
        assert_eq!(b.rev(), prev_rev);
        let after = String::from_utf8(b.read_range(0, usize::MAX)).unwrap();
        assert_eq!(after, full);
    }

    #[test]
    fn apply_batch_bottom_up_single_undo_and_overlap_guard() {
        let (_d, p) = tmpfile(
            "batch.rs",
            "fn a() -> i32 { 1 }\nfn b() -> i32 { 2 }\nfn c() -> i32 { 3 }\n",
        );
        let mut b = PolerBuffer::open(&p).unwrap();
        let rev0 = b.rev();

        let ops = vec![
            BatchEditOp::ReplaceMatch {
                old_text: "{ 1 }".into(),
                new_text: "{ 100 }".into(),
                near_line: None,
                occurrence: None,
                replace_all: false,
            },
            BatchEditOp::ReplaceMatch {
                old_text: "{ 3 }".into(),
                new_text: "{\n    let z = 300;\n    z\n}".into(),
                near_line: None,
                occurrence: None,
                replace_all: false,
            },
        ];
        let rec = b.apply_batch(&ops, true).unwrap();
        assert_eq!(rec.replacements, 2);
        assert_eq!(rec.rev, rev0 + 1);
        assert_eq!(b.stats().undo_depth, 1);

        let text = b.read_span(0, 20, 65536).unwrap().text;
        assert!(text.contains("fn a() -> i32 { 100 }"));
        assert!(text.contains("let z = 300;"));

        // Один Undo откатывает ВЕСЬ батч целиком!
        assert!(b.undo());
        assert_eq!(
            String::from_utf8(b.read_range(0, usize::MAX)).unwrap(),
            "fn a() -> i32 { 1 }\nfn b() -> i32 { 2 }\nfn c() -> i32 { 3 }\n"
        );
    }

    #[test]
    fn piece_diff_and_apply_unified_diff_roundtrip() {
        let mut orig = String::new();
        for i in 0..200 {
            orig.push_str(&format!("line {i:03}: invariant value\n"));
        }
        let (_d, p) = tmpfile("big.txt", &orig);
        let mut b = PolerBuffer::open_with_cap(&p, 128).unwrap();

        b.replace_match(
            "line 010: invariant value",
            "line 010: ALPHA_AGENT",
            None,
            None,
            false,
            false,
        )
        .unwrap();
        b.replace_match(
            "line 180: invariant value",
            "line 180: OMEGA_AGENT\nline 180b: EXTRA",
            None,
            None,
            false,
            false,
        )
        .unwrap();

        let patch = b.piece_diff(2).unwrap();
        assert!(patch.contains("--- a/"));
        assert!(patch.contains("-line 010: invariant value\n+line 010: ALPHA_AGENT\n"));
        assert!(patch.contains("+line 180b: EXTRA\n"));

        // Применяем сгенерированный diff к чистой копии файла и проверяем 100% совпадение байтов!
        let mut clean = PolerBuffer::open(&p).unwrap();
        clean.apply_unified_diff(&patch, 2, false).unwrap();
        assert_eq!(
            clean.read_range(0, usize::MAX),
            b.read_range(0, usize::MAX)
        );
    }

    #[test]
    fn speculative_fork_and_kleene_three_way_merge() {
        let (_d, p) = tmpfile(
            "shared.rs",
            "fn alpha() -> i32 {\n    1\n}\n\nfn beta() -> i32 {\n    2\n}\n",
        );
        let base = PolerBuffer::open(&p).unwrap();
        let mut agent_a = base.fork();
        let mut agent_b = base.fork();

        // Агент A меняет alpha(), Агент B параллельно меняет beta():
        agent_a
            .replace_match("    1", "    10 + 1", Some(1), None, false, true)
            .unwrap();
        agent_b
            .replace_match("    2", "    20 * 2", Some(5), None, false, true)
            .unwrap();

        let merged = merge_three_way(&base, &agent_a, &agent_b, false).unwrap();
        assert!(merged.clean);
        assert_eq!(merged.applied_ours, 1);
        assert_eq!(merged.applied_theirs, 1);
        let out = String::from_utf8(merged.buffer.read_range(0, usize::MAX)).unwrap();
        assert!(out.contains("    10 + 1"));
        assert!(out.contains("    20 * 2"));

        // А теперь оба агента меняют ОДНУ строку по-разному -> детектор коллизии Клини (-1):
        let mut c1 = base.fork();
        let mut c2 = base.fork();
        c1.replace_match("    1", "    111", Some(1), None, false, true).unwrap();
        c2.replace_match("    1", "    999", Some(1), None, false, true).unwrap();
        let collision = merge_three_way(&base, &c1, &c2, true).unwrap();
        assert!(!collision.clean);
        assert_eq!(collision.conflicts.len(), 1);
        let marked = String::from_utf8(collision.buffer.read_range(0, usize::MAX)).unwrap();
        assert!(marked.contains("<<<<<<< OURS"));
        assert!(marked.contains(">>>>>>> THEIRS"));
    }

    #[test]
    fn external_disk_conflict_guard_and_reload() {
        let (_d, p) = tmpfile("disk.txt", "v1\n");
        let mut b = PolerBuffer::open(&p).unwrap();
        b.insert(0, "agent_edit\n").unwrap();

        // Внешний процесс (git checkout / форматтер / другой агент) атомарно заменяет файл:
        let ext_tmp = p.with_extension("ext_tmp");
        std::fs::write(&ext_tmp, "external_override_longer\n").unwrap();
        std::fs::rename(&ext_tmp, &p).unwrap();
        assert!(b.disk_changed_externally());

        // `save_checked(false)` блокирует перезапись чужих изменений!
        let err = b.save_checked(false).unwrap_err();
        assert!(err.to_string().contains("disk_conflict"));

        // `reload()` подхватывает актуальную версию с диска и инкрементирует `rev`:
        let prev_rev = b.rev();
        b.reload().unwrap();
        assert!(b.rev() > prev_rev);
        assert!(!b.disk_changed_externally());
        assert_eq!(
            String::from_utf8(b.read_range(0, usize::MAX)).unwrap(),
            "external_override_longer\n"
        );
    }
}
