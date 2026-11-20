//! Sovereign Document Text Extractor for POLER Edit.
//! Extracts clean, searchable, editable plain text & AI-optimized Markdown from:
//! PDF, EPUB, FB2, DOCX, ODT, RTF, MOBI, AZW, AZW3.
//!
//! Standards-compliant & AI-first architecture:
//! - PDF: Full CMap / ToUnicode / font dictionary stream extraction via `pdf-extract`.
//! - EPUB: Full OPF spine ordering, metadata extraction, HTML-to-Markdown cleaning via `zip`.
//! - FB2: FictionBook 2.0 schema, windows-1251/UTF-8 decoding, complete base64 binary stripping,
//!        metadata header, notes body support.
//! - MOBI / AZW / AZW3: Kindle PalmDOC decompressed HTML extraction via `mobi`.
//! - DOCX / ODT: Central directory ZIP extraction with table & paragraph Markdown formatting.
//! - AI Token Hygiene: Strips soft-hyphens (\u{00AD}), zero-width chars, normalizes \u{00A0},
//!   collapses excess blanks, formats clean Markdown landmarks (#, ##, >, -, |).

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Cursor, Read};
use std::path::Path;

pub fn is_extractable_format(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "pdf" | "epub" | "fb2" | "docx" | "odt" | "rtf" | "mobi" | "azw" | "azw3"
    )
}

pub fn extract_text(path: &Path) -> io::Result<Option<String>> {
    extract_text_opts(path, false)
}

/// Фаза 1 «Книги з ілюстраціями»: извлечение с сохранением ссылок на
/// иллюстрации. EPUB: `<img src="../images/fig1.jpg">` превращается в
/// Markdown `![alt](OEBPS/images/fig1.jpg)` — путь ABSOLUTE внутри архива,
/// чтобы ReaderView мог переписать его в `vfs://<архив>/<путь>` и
/// подтянуть картинку через vfs_read. FB2: binary-элементы становятся
/// data:-URI прямо в тексте. Остальные форматы (PDF/DOCX/ODT/RTF/MOBI)
/// от keep_images не зависят.
pub fn extract_text_opts(path: &Path, keep_images: bool) -> io::Result<Option<String>> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => Ok(Some(extract_pdf(path)?)),
        "epub" => Ok(Some(extract_epub(path, keep_images)?)),
        "fb2" => Ok(Some(extract_fb2(path, keep_images)?)),
        "docx" => Ok(Some(extract_docx(path)?)),
        "odt" => Ok(Some(extract_odt(path)?)),
        "rtf" => Ok(Some(extract_rtf(path)?)),
        "mobi" | "azw" | "azw3" => Ok(Some(extract_mobi(path)?)),
        _ => Ok(None),
    }
}

// ============================================================================
// PDF (.pdf) Clean Text Extractor (Full ToUnicode / CMap via pdf-extract)
// ============================================================================

fn extract_pdf(path: &Path) -> io::Result<String> {
    let bytes = std::fs::read(path)?;
    match pdf_extract::extract_text_from_mem(&bytes) {
        Ok(text) => {
            // Form-feed (\x0C) -> page divider landmark for AI & humans
            let normalized = text.replace('\x0C', "\n\n---\n\n");
            let cleaned = clean_paragraphs(&normalized);
            if cleaned.trim().is_empty() {
                Ok(String::from(
                    "[PDF: Документ не містить текстового шару (сканований документ / графічний PDF) або захищений паролем]",
                ))
            } else {
                Ok(cleaned)
            }
        }
        Err(e) => Ok(format!(
            "[PDF: Не вдалося видобути текстовий шар: {} (документ може бути захищений паролем або сканований)]",
            e
        )),
    }
}

// ============================================================================
// MOBI / AZW / AZW3 Kindle Extractor via mobi
// ============================================================================

fn extract_mobi(path: &Path) -> io::Result<String> {
    let m = match mobi::Mobi::from_path(path) {
        Ok(m) => m,
        Err(e) => return Ok(format!("[MOBI: Не вдалося прочитати файл: {}]", e)),
    };

    let title = m.title();
    let author = m.author().unwrap_or_default();
    let desc = m.description().unwrap_or_default();
    let content = m.content_as_string_lossy();

    let mut doc = String::new();
    if !title.is_empty() || !author.is_empty() {
        doc.push_str(&format!("# {}\n\n", clean_inline_text(&title)));
        if !author.is_empty() {
            doc.push_str(&format!("> **Автор:** {}\n", clean_inline_text(&author)));
        }
        if !desc.is_empty() {
            let clean_desc = strip_html_tags(&desc);
            if !clean_desc.trim().is_empty() {
                doc.push_str(&format!("> **Анотація:** {}\n", clean_inline_text(&clean_desc)));
            }
        }
        doc.push_str("\n---\n\n");
    }

    doc.push_str(&strip_html_tags(&content));
    Ok(clean_paragraphs(&doc))
}

// ============================================================================
// EPUB (.epub) Clean Text Extractor (OPF Spine Order + Metadata via zip)
// ============================================================================

fn extract_epub(path: &Path, keep_images: bool) -> io::Result<String> {
    let file = File::open(path)?;
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(e) => return Ok(format!("[EPUB: Помилка відкриття архіву: {}]", e)),
    };

    // Preload text-based entries into memory so archive borrow is completed immediately
    let mut entries: HashMap<String, String> = HashMap::new();
    for i in 0..archive.len() {
        if let Ok(mut entry) = archive.by_index(i) {
            let name = entry.name().to_string();
            let lower = name.to_ascii_lowercase();
            if lower.ends_with(".xhtml")
                || lower.ends_with(".html")
                || lower.ends_with(".htm")
                || lower.ends_with(".xml")
                || lower.ends_with(".opf")
                || lower.ends_with(".ncx")
            {
                let mut content = String::new();
                if entry.read_to_string(&mut content).is_ok() {
                    entries.insert(name, content);
                }
            }
        }
    }

    let get_entry = |target: &str| -> Option<&String> {
        if let Some(val) = entries.get(target) {
            return Some(val);
        }
        let norm = target.replace('\\', "/");
        let decoded = norm.replace("%20", " ");
        let leaf = norm.rsplit('/').next().unwrap_or(&norm);
        for (k, v) in &entries {
            if k == &norm || k == &decoded || k.ends_with(leaf) {
                return Some(v);
            }
        }
        None
    };

    // 1. Check META-INF/container.xml for rootfile OPF
    //    ВАЖНО: ищем именно ТЕГ <rootfile>, а не подстроку «rootfile»:
    //    «<rootfiles» тоже начинается с «<rootfile», и наивный find
    //    цепляется за родительский тег — метаданные книги молча терялись.
    let opf_path = entries
        .iter()
        .find(|(k, _)| k.ends_with("container.xml"))
        .and_then(|(_, xml)| {
            let mut pos = 0usize;
            while let Some(p) = xml[pos..].find("<rootfile") {
                let at = pos + p;
                let after = xml.as_bytes().get(at + "<rootfile".len()).copied().unwrap_or(b' ');
                if matches!(after, b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>') {
                    // Настоящий тег <rootfile …> (не <rootfiles>).
                    let start = at + "<rootfile".len();
                    let end = xml[start..].find('>').map(|e| start + e)?;
                    return attr_of(xml[start..end].trim(), "full-path");
                }
                pos = at + 1;
            }
            None
        });

    let mut title: Option<String> = None;
    let mut author: Option<String> = None;
    let mut description: Option<String> = None;
    let mut spine_files: Vec<String> = Vec::new();

    // 2. Parse OPF if found
    if let Some(ref opf) = opf_path {
        let opf_dir = Path::new(opf)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("");
        if let Some(opf_xml) = get_entry(opf) {
            title = extract_xml_tag_content(opf_xml, "dc:title")
                .or_else(|| extract_xml_tag_content(opf_xml, "title"));
            author = extract_xml_tag_content(opf_xml, "dc:creator")
                .or_else(|| extract_xml_tag_content(opf_xml, "creator"));
            description = extract_xml_tag_content(opf_xml, "dc:description")
                .or_else(|| extract_xml_tag_content(opf_xml, "description"));

            // Parse manifest items: id -> href
            let manifest = parse_manifest_items(opf_xml);
            // Parse spine itemrefs
            let itemrefs = parse_spine_itemrefs(opf_xml);

            for idref in itemrefs {
                if let Some(href) = manifest.get(&idref) {
                    let full_path = if opf_dir.is_empty() {
                        href.clone()
                    } else {
                        format!("{}/{}", opf_dir, href)
                    };
                    spine_files.push(full_path);
                }
            }
        }
    }

    // 3. Fallback: if spine_files is empty, find all xhtml/html/htm files
    if spine_files.is_empty() {
        let mut names: Vec<String> = entries
            .keys()
            .filter(|k| {
                let lower = k.to_ascii_lowercase();
                (lower.ends_with(".xhtml") || lower.ends_with(".html") || lower.ends_with(".htm"))
                    && !lower.contains("cover")
                    && !lower.contains("toc")
            })
            .cloned()
            .collect();
        names.sort();

        if names.is_empty() {
            names = entries
                .keys()
                .filter(|k| {
                    let lower = k.to_ascii_lowercase();
                    lower.ends_with(".xhtml") || lower.ends_with(".html") || lower.ends_with(".htm")
                })
                .cloned()
                .collect();
            names.sort();
        }
        spine_files = names;
    }

    // 4. Extract chapter texts in Markdown format
    let mut full_text = String::new();

    // Metadata header
    if title.is_some() || author.is_some() {
        if let Some(ref t) = title {
            full_text.push_str(&format!("# {}\n\n", clean_inline_text(t)));
        }
        if let Some(ref a) = author {
            full_text.push_str(&format!("> **Автор:** {}\n", clean_inline_text(a)));
        }
        if let Some(ref d) = description {
            let clean_desc = strip_html_tags(d);
            if !clean_desc.trim().is_empty() {
                full_text.push_str(&format!("> **Анотація:** {}\n", clean_inline_text(&clean_desc)));
            }
        }
        full_text.push_str("\n---\n\n");
    }

    let mut chapter_count = 0usize;
    for chapter_path in spine_files {
        if let Some(raw_html) = get_entry(&chapter_path) {
            // keep_images: относительные <img src> → абсолютные пути внутри
            // архива, чтобы читалка могла читать их через vfs://.
            let cleaned = if keep_images {
                let base_dir = Path::new(&chapter_path)
                    .parent()
                    .and_then(|p| p.to_str())
                    .unwrap_or("")
                    .to_string();
                strip_html_tags_keep_images(raw_html, &base_dir)
            } else {
                strip_html_tags(raw_html)
            };
            let trimmed = cleaned.trim();
            if !trimmed.is_empty() {
                if chapter_count > 0 {
                    full_text.push_str("\n\n---\n\n");
                }
                full_text.push_str(trimmed);
                chapter_count += 1;
            }
        }
    }

    if full_text.trim().is_empty() {
        Ok(String::from("[EPUB: Книга не містить текстових розділів або захищена DRM]"))
    } else {
        Ok(clean_paragraphs(&full_text))
    }
}

// ============================================================================
// FB2 (.fb2 / .fb2.zip) FictionBook 2.0 Standard Parser
// ============================================================================

fn extract_fb2(path: &Path, keep_images: bool) -> io::Result<String> {
    let raw_bytes = std::fs::read(path)?;

    // Handle .fb2.zip or raw zip container
    let fb2_bytes = if raw_bytes.starts_with(b"PK\x03\x04") {
        let cursor = Cursor::new(&raw_bytes);
        if let Ok(mut archive) = zip::ZipArchive::new(cursor) {
            let mut found = None;
            for i in 0..archive.len() {
                if let Ok(mut f) = archive.by_index(i) {
                    if f.name().to_ascii_lowercase().ends_with(".fb2") {
                        let mut buf = Vec::new();
                        if f.read_to_end(&mut buf).is_ok() {
                            found = Some(buf);
                            break;
                        }
                    }
                }
            }
            found.unwrap_or(raw_bytes)
        } else {
            raw_bytes
        }
    } else {
        raw_bytes
    };

    let xml = decode_xml_encoding(&fb2_bytes);
    Ok(parse_fb2_standard(&xml, keep_images))
}

fn decode_xml_encoding(bytes: &[u8]) -> String {
    let header_len = bytes.len().min(500);
    let header_ascii = String::from_utf8_lossy(&bytes[..header_len]).to_ascii_lowercase();

    if header_ascii.contains("windows-1251") || header_ascii.contains("cp1251") {
        let (cow, _, _) = encoding_rs::WINDOWS_1251.decode(bytes);
        cow.into_owned()
    } else if header_ascii.contains("koi8-r") {
        let (cow, _, _) = encoding_rs::KOI8_R.decode(bytes);
        cow.into_owned()
    } else if header_ascii.contains("iso-8859-5") {
        let (cow, _, _) = encoding_rs::ISO_8859_5.decode(bytes);
        cow.into_owned()
    } else {
        let (cow, _, _) = encoding_rs::UTF_8.decode(bytes);
        cow.into_owned()
    }
}

fn parse_fb2_standard(xml: &str, keep_images: bool) -> String {
    let mut book_title = String::new();
    let mut authors: Vec<String> = Vec::new();
    let mut current_author_first = String::new();
    let mut current_author_middle = String::new();
    let mut current_author_last = String::new();
    let mut current_author_nick = String::new();
    let mut genres: Vec<String> = Vec::new();
    let mut annotation = String::new();
    let mut date = String::new();

    let mut body_text = String::new();
    let mut notes_text = String::new();

    // Фаза 1: binary-элементы собираются предпроходом (в документе они
    // идут после <body>, а ссылки <image l:href="#id"/> — раньше);
    // обложка из <coverpage> вставляется сразу после шапки книги.
    let binaries = if keep_images {
        collect_fb2_binaries(xml)
    } else {
        HashMap::new()
    };
    let mut cover_md = String::new();
    let mut in_coverpage = false;

    let mut tag_stack: Vec<String> = Vec::new();
    let mut in_binary = false;
    let mut in_notes_body = false;
    let mut in_main_body = false;

    let mut chars = xml.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '<' {
            let mut raw_tag = String::new();
            while let Some(&c) = chars.peek() {
                if c == '>' {
                    chars.next();
                    break;
                }
                raw_tag.push(chars.next().unwrap());
            }

            let trimmed = raw_tag.trim();
            if trimmed.is_empty() || trimmed.starts_with("!--") || trimmed.starts_with('?') {
                continue;
            }

            let is_closing = trimmed.starts_with('/');
            let is_self_closing = trimmed.ends_with('/');

            let name_part = if is_closing {
                &trimmed[1..]
            } else {
                trimmed.split_whitespace().next().unwrap_or("")
            };
            let tag_name = name_part.trim_end_matches('/').to_ascii_lowercase();

            if is_closing {
                if tag_name == "binary" {
                    in_binary = false;
                } else if tag_name == "coverpage" {
                    in_coverpage = false;
                } else if tag_name == "body" {
                    in_notes_body = false;
                    in_main_body = false;
                } else if tag_name == "author" {
                    let mut full_author = String::new();
                    if !current_author_first.is_empty() {
                        full_author.push_str(&current_author_first);
                    }
                    if !current_author_middle.is_empty() {
                        if !full_author.is_empty() { full_author.push(' '); }
                        full_author.push_str(&current_author_middle);
                    }
                    if !current_author_last.is_empty() {
                        if !full_author.is_empty() { full_author.push(' '); }
                        full_author.push_str(&current_author_last);
                    }
                    if full_author.is_empty() && !current_author_nick.is_empty() {
                        full_author.push_str(&current_author_nick);
                    }
                    let trimmed_author = full_author.trim().to_string();
                    if !trimmed_author.is_empty() && !authors.contains(&trimmed_author) {
                        authors.push(trimmed_author);
                    }
                    current_author_first.clear();
                    current_author_middle.clear();
                    current_author_last.clear();
                    current_author_nick.clear();
                } else if tag_name == "title" {
                    let target = if in_notes_body { &mut notes_text } else { &mut body_text };
                    target.push_str("\n\n");
                } else if tag_name == "subtitle" {
                    let target = if in_notes_body { &mut notes_text } else { &mut body_text };
                    target.push_str("\n\n");
                } else if tag_name == "epigraph" {
                    let target = if in_notes_body { &mut notes_text } else { &mut body_text };
                    target.push_str("\n\n");
                } else if tag_name == "p" || tag_name == "v" {
                    let target = if in_notes_body { &mut notes_text } else { &mut body_text };
                    target.push('\n');
                } else if tag_name == "tr" {
                    let target = if in_notes_body { &mut notes_text } else { &mut body_text };
                    target.push('\n');
                } else if tag_name == "td" || tag_name == "th" {
                    let target = if in_notes_body { &mut notes_text } else { &mut body_text };
                    target.push_str(" | ");
                }

                if let Some(pos) = tag_stack.iter().rposition(|t| t == &tag_name) {
                    tag_stack.truncate(pos);
                }
            } else {
                // Opening tag
                if tag_name == "binary" {
                    in_binary = true;
                } else if tag_name == "coverpage" {
                    in_coverpage = true;
                } else if tag_name == "image" {
                    // <image l:href="#id"/> — обложка (в coverpage) или
                    // внутритекстовая иллюстрация → data:-URI в Markdown.
                    if keep_images {
                        let href = attr_of(trimmed, "href").unwrap_or_default();
                        let id = href.trim_start_matches('#').trim();
                        if !id.is_empty() {
                            if let Some((ct, b64)) = binaries.get(id) {
                                let alt = id.replace('[', "(").replace(']', ")");
                                let md =
                                    format!("\n\n![{}](data:{};base64,{})\n\n", alt, ct, b64);
                                if in_coverpage {
                                    cover_md = md;
                                } else if in_main_body || in_notes_body {
                                    let target = if in_notes_body {
                                        &mut notes_text
                                    } else {
                                        &mut body_text
                                    };
                                    target.push_str(&md);
                                }
                            }
                        }
                    }
                } else if tag_name == "body" {
                    let lower_raw = trimmed.to_ascii_lowercase();
                    if lower_raw.contains("name=\"notes\"") || lower_raw.contains("name=\"comments\"") {
                        in_notes_body = true;
                        in_main_body = false;
                    } else {
                        in_main_body = true;
                        in_notes_body = false;
                    }
                } else if in_main_body || in_notes_body {
                    let target = if in_notes_body { &mut notes_text } else { &mut body_text };
                    match tag_name.as_str() {
                        "title" => target.push_str("\n\n## "),
                        "subtitle" => target.push_str("\n\n### "),
                        "epigraph" => target.push_str("\n\n> "),
                        "p" => target.push('\n'),
                        "v" => target.push_str("\n> "),
                        "empty-line" => target.push('\n'),
                        "section" => target.push_str("\n\n"),
                        "tr" => target.push_str("\n| "),
                        _ => {}
                    }
                }

                if !is_self_closing {
                    tag_stack.push(tag_name);
                }
            }
            continue;
        }

        // Text character
        if in_binary {
            // Strictly drop all base64 / binary payload
            continue;
        }

        // Check current tag context
        if let Some(current_tag) = tag_stack.last() {
            let in_desc = tag_stack.iter().any(|t| t == "description" || t == "title-info");
            if in_desc {
                match current_tag.as_str() {
                    "book-title" => book_title.push(ch),
                    "first-name" => current_author_first.push(ch),
                    "middle-name" => current_author_middle.push(ch),
                    "last-name" => current_author_last.push(ch),
                    "nickname" => current_author_nick.push(ch),
                    "genre" => {
                        if genres.is_empty() || genres.last().map_or(false, |g| g.ends_with(',')) {
                            genres.push(String::new());
                        }
                        if let Some(last) = genres.last_mut() {
                            last.push(ch);
                        }
                    }
                    "date" => date.push(ch),
                    _ => {
                        if tag_stack.iter().any(|t| t == "annotation") {
                            annotation.push(ch);
                        }
                    }
                }
            } else if in_main_body {
                body_text.push(ch);
            } else if in_notes_body {
                notes_text.push(ch);
            }
        }
    }

    // Format final standard Markdown document
    let mut doc = String::new();

    let clean_title = clean_inline_text(&unescape_xml_entities(&book_title));
    let clean_annotation = clean_paragraphs(&unescape_xml_entities(&annotation));

    if !clean_title.is_empty() {
        doc.push_str(&format!("# {}\n\n", clean_title));
    }
    if !authors.is_empty() {
        doc.push_str(&format!("> **Автор:** {}\n", authors.join(", ")));
    }
    if !genres.is_empty() {
        let clean_genres: Vec<String> = genres
            .into_iter()
            .map(|g| clean_inline_text(&unescape_xml_entities(&g)))
            .filter(|g| !g.is_empty())
            .collect();
        if !clean_genres.is_empty() {
            doc.push_str(&format!("> **Жанр:** {}\n", clean_genres.join(", ")));
        }
    }
    if !date.trim().is_empty() {
        doc.push_str(&format!("> **Дата:** {}\n", clean_inline_text(&date)));
    }

    if !clean_annotation.trim().is_empty() {
        doc.push_str("\n### Анотація\n\n");
        doc.push_str(clean_annotation.trim());
        doc.push_str("\n\n---\n\n");
    } else if !doc.is_empty() {
        doc.push_str("\n---\n\n");
    }

    // Обложка книги — сразу после шапки, чтобы первая же страница
    // встречала пользователя графикой, а не текстовым шумом.
    if !cover_md.is_empty() {
        doc.push_str(&cover_md);
    }

    // Main Body
    let clean_body = clean_paragraphs(&unescape_xml_entities(&body_text));
    doc.push_str(&clean_body);

    // Notes Body
    let clean_notes = clean_paragraphs(&unescape_xml_entities(&notes_text));
    if !clean_notes.trim().is_empty() {
        doc.push_str("\n\n---\n\n## Примітки / Notes\n\n");
        doc.push_str(&clean_notes);
    }

    doc
}

// ============================================================================
// DOCX (.docx) Clean Text & Markdown Extractor
// ============================================================================

fn extract_docx(path: &Path) -> io::Result<String> {
    let file = File::open(path)?;
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(e) => return Ok(format!("[DOCX: Помилка відкриття архіву: {}]", e)),
    };

    let mut doc_xml = String::new();
    if let Ok(mut entry) = archive.by_name("word/document.xml") {
        entry.read_to_string(&mut doc_xml)?;
    } else {
        return Ok(String::from("[DOCX: Не знайдено word/document.xml]"));
    }

    Ok(clean_paragraphs(&parse_docx_xml(&doc_xml)))
}

fn parse_docx_xml(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len() / 2);
    let mut in_t = false;
    let mut in_tbl = false;

    let mut chars = xml.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '<' {
            let mut tag = String::new();
            while let Some(&c) = chars.peek() {
                if c == '>' {
                    chars.next();
                    break;
                }
                tag.push(chars.next().unwrap());
            }
            let t = tag.trim().to_ascii_lowercase();
            if t == "w:tbl" || t.starts_with("w:tbl ") {
                in_tbl = true;
                out.push_str("\n\n");
            } else if t == "/w:tbl" {
                in_tbl = false;
                out.push_str("\n\n");
            } else if t == "w:tr" || t.starts_with("w:tr ") {
                out.push_str("\n| ");
            } else if t == "/w:tr" {
                out.push('\n');
            } else if t == "/w:tc" {
                out.push_str(" | ");
            } else if t == "w:p" || t.starts_with("w:p ") {
                if !in_tbl {
                    out.push('\n');
                }
            } else if t == "/w:p" {
                if !in_tbl {
                    out.push('\n');
                }
            } else if t == "w:t" || t.starts_with("w:t ") {
                in_t = true;
            } else if t == "/w:t" {
                in_t = false;
            } else if t == "w:tab" || t.starts_with("w:tab/") {
                out.push('\t');
            } else if t == "w:br" || t.starts_with("w:br/") {
                out.push('\n');
            }
            continue;
        }
        if in_t {
            out.push(ch);
        }
    }

    unescape_xml_entities(&out)
}

// ============================================================================
// ODT (.odt) Clean Text Extractor
// ============================================================================

fn extract_odt(path: &Path) -> io::Result<String> {
    let file = File::open(path)?;
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(e) => return Ok(format!("[ODT: Помилка відкриття архіву: {}]", e)),
    };

    let mut content_xml = String::new();
    if let Ok(mut entry) = archive.by_name("content.xml") {
        entry.read_to_string(&mut content_xml)?;
    } else {
        return Ok(String::from("[ODT: Не знайдено content.xml]"));
    }

    Ok(clean_paragraphs(&strip_html_tags(&content_xml)))
}

// ============================================================================
// RTF (.rtf) Clean Text Extractor
// ============================================================================

fn extract_rtf(path: &Path) -> io::Result<String> {
    let mut f = File::open(path)?;
    let mut s = String::new();
    f.read_to_string(&mut s)?;

    let mut out = String::new();
    let mut skip_group = 0usize;
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '{' {
            continue;
        }
        if ch == '}' {
            if skip_group > 0 {
                skip_group -= 1;
            }
            continue;
        }
        if skip_group > 0 {
            continue;
        }

        if ch == '\\' {
            let mut word = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_alphabetic() {
                    word.push(chars.next().unwrap());
                } else {
                    break;
                }
            }
            match word.as_str() {
                "par" | "line" => out.push('\n'),
                "tab" => out.push('\t'),
                "fonttbl" | "colortbl" | "stylesheet" | "info" => {
                    skip_group += 1;
                }
                "u" => {
                    let mut num = String::new();
                    while let Some(&c) = chars.peek() {
                        if c.is_ascii_digit() || c == '-' {
                            num.push(chars.next().unwrap());
                        } else {
                            break;
                        }
                    }
                    if let Ok(n) = num.parse::<i32>() {
                        let code = if n < 0 { (n + 65536) as u32 } else { n as u32 };
                        if let Some(c) = char::from_u32(code) {
                            out.push(c);
                        }
                    }
                    if chars.peek() == Some(&'?') {
                        chars.next();
                    }
                }
                "'" => {
                    let mut hex = String::new();
                    if let Some(c1) = chars.next() { hex.push(c1); }
                    if let Some(c2) = chars.next() { hex.push(c2); }
                    if let Ok(b) = u8::from_str_radix(&hex, 16) {
                        match b {
                            0xC0..=0xFF => {
                                let code = 0x0410 + (b - 0xC0) as u32;
                                out.push(char::from_u32(code).unwrap_or('?'));
                            }
                            0xA8 => out.push('Ё'),
                            0xB8 => out.push('ё'),
                            0x20..=0x7E => out.push(b as char),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            continue;
        }

        out.push(ch);
    }

    Ok(clean_paragraphs(&out))
}

// ============================================================================
// HTML / XML Utilities & Paragraph Normalization (AI-first Markdown)
// ============================================================================

fn strip_html_tags(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_tag = false;
    let mut skip_block = false;
    let mut current_tag = String::new();

    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '<' {
            in_tag = true;
            current_tag.clear();
            continue;
        }
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let tag_lower = current_tag.trim().to_ascii_lowercase();
                if tag_lower.starts_with("script") || tag_lower.starts_with("style") || tag_lower.starts_with("head") {
                    skip_block = true;
                } else if tag_lower.starts_with("/script") || tag_lower.starts_with("/style") || tag_lower.starts_with("/head") {
                    skip_block = false;
                } else if tag_lower == "p" || tag_lower == "/p"
                    || tag_lower == "div" || tag_lower == "/div"
                {
                    out.push('\n');
                } else if tag_lower == "br" || tag_lower == "br/" || tag_lower == "br /" {
                    out.push('\n');
                } else if tag_lower.starts_with("h1") {
                    out.push_str("\n\n# ");
                } else if tag_lower.starts_with("/h1") {
                    out.push_str("\n\n");
                } else if tag_lower.starts_with("h2") {
                    out.push_str("\n\n## ");
                } else if tag_lower.starts_with("/h2") {
                    out.push_str("\n\n");
                } else if tag_lower.starts_with("h3") || tag_lower.starts_with("h4") || tag_lower.starts_with("h5") || tag_lower.starts_with("h6") {
                    out.push_str("\n\n### ");
                } else if tag_lower.starts_with("/h3") || tag_lower.starts_with("/h4") || tag_lower.starts_with("/h5") || tag_lower.starts_with("/h6") {
                    out.push_str("\n\n");
                } else if tag_lower == "li" {
                    out.push_str("\n- ");
                } else if tag_lower == "blockquote" {
                    out.push_str("\n\n> ");
                } else if tag_lower == "/blockquote" {
                    out.push_str("\n\n");
                } else if tag_lower == "hr" || tag_lower == "hr/" || tag_lower == "hr /" {
                    out.push_str("\n\n---\n\n");
                } else if tag_lower == "tr" || tag_lower.starts_with("tr ") {
                    out.push_str("\n| ");
                } else if tag_lower == "/tr" {
                    out.push('\n');
                } else if tag_lower == "td" || tag_lower == "th" || tag_lower.starts_with("td ") || tag_lower.starts_with("th ") {
                    // cell content follows
                } else if tag_lower == "/td" || tag_lower == "/th" {
                    out.push_str(" | ");
                }
            } else {
                current_tag.push(ch);
            }
            continue;
        }
        if !skip_block {
            out.push(ch);
        }
    }
    unescape_xml_entities(&out)
}

fn unescape_xml_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '&' {
            let mut ent = String::new();
            while let Some(&c) = chars.peek() {
                if c == ';' {
                    chars.next();
                    break;
                }
                if c.is_alphanumeric() || c == '#' {
                    ent.push(chars.next().unwrap());
                } else {
                    break;
                }
            }
            match ent.as_str() {
                "quot" => out.push('"'),
                "amp" => out.push('&'),
                "apos" => out.push('\''),
                "lt" => out.push('<'),
                "gt" => out.push('>'),
                "nbsp" => out.push(' '),
                "mdash" => out.push('—'),
                "ndash" => out.push('–'),
                "hellip" => out.push('…'),
                "laquo" => out.push('«'),
                "raquo" => out.push('»'),
                "copy" => out.push('©'),
                "reg" => out.push('®'),
                "trade" => out.push('™'),
                _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                    if let Ok(code) = u32::from_str_radix(&ent[2..], 16) {
                        if let Some(c) = char::from_u32(code) {
                            out.push(c);
                            continue;
                        }
                    }
                    out.push('&');
                    out.push_str(&ent);
                    out.push(';');
                }
                _ if ent.starts_with('#') => {
                    if let Ok(code) = ent[1..].parse::<u32>() {
                        if let Some(c) = char::from_u32(code) {
                            out.push(c);
                            continue;
                        }
                    }
                    out.push('&');
                    out.push_str(&ent);
                    out.push(';');
                }
                _ => {
                    out.push('&');
                    out.push_str(&ent);
                    out.push(';');
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn extract_xml_tag_content(xml: &str, tag: &str) -> Option<String> {
    let open_pat = format!("<{}", tag);
    let close_pat = format!("</{}>", tag);

    let start = xml.find(&open_pat)?;
    let content_start = xml[start..].find('>')? + start + 1;
    let end = xml[content_start..].find(&close_pat)? + content_start;

    Some(strip_html_tags(&xml[content_start..end]).trim().to_string())
}

fn extract_attr_value(xml: &str, tag: &str, attr: &str) -> Option<String> {
    let tag_pos = xml.find(tag)?;
    let slice = &xml[tag_pos..];
    let end_tag = slice.find('>')?;
    let tag_slice = &slice[..end_tag];

    let attr_pat = format!("{}=\"", attr);
    if let Some(val_pos) = tag_slice.find(&attr_pat) {
        let val_start = val_pos + attr_pat.len();
        let val_slice = &tag_slice[val_start..];
        let val_end = val_slice.find('"')?;
        return Some(val_slice[..val_end].to_string());
    }

    let attr_pat_sq = format!("{}='", attr);
    if let Some(val_pos) = tag_slice.find(&attr_pat_sq) {
        let val_start = val_pos + attr_pat_sq.len();
        let val_slice = &tag_slice[val_start..];
        let val_end = val_slice.find('\'')?;
        return Some(val_slice[..val_end].to_string());
    }

    None
}

fn parse_manifest_items(opf_xml: &str) -> HashMap<String, String> {
    let mut manifest = HashMap::new();
    let mut cursor = 0usize;

    while let Some(pos) = opf_xml[cursor..].find("<item ") {
        let start = cursor + pos;
        let end = match opf_xml[start..].find('>') {
            Some(e) => start + e + 1,
            None => break,
        };
        let slice = &opf_xml[start..end];

        let id = extract_attr_value(slice, "item", "id");
        let href = extract_attr_value(slice, "item", "href");

        if let (Some(id), Some(href)) = (id, href) {
            manifest.insert(id, href);
        }

        cursor = end;
    }

    manifest
}

fn parse_spine_itemrefs(opf_xml: &str) -> Vec<String> {
    let mut itemrefs = Vec::new();
    let mut cursor = 0usize;

    while let Some(pos) = opf_xml[cursor..].find("<itemref ") {
        let start = cursor + pos;
        let end = match opf_xml[start..].find('>') {
            Some(e) => start + e + 1,
            None => break,
        };
        let slice = &opf_xml[start..end];

        if let Some(idref) = extract_attr_value(slice, "itemref", "idref") {
            itemrefs.push(idref);
        }

        cursor = end;
    }

    itemrefs
}

fn clean_inline_text(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ============================================================================
// Фаза 1: сохранение иллюстраций (EPUB img + FB2 binary)
// ============================================================================

/// Атрибут из сырого текста тега (без «>»): `src="…"`, `alt='…'`, `l:href="…"`.
fn attr_of(tag_src: &str, attr: &str) -> Option<String> {
    let dq = format!("{}=\"", attr);
    let sq = format!("{}='", attr);
    for pat in [dq, sq] {
        if let Some(pos) = tag_src.find(&pat) {
            let start = pos + pat.len();
            let rest = &tag_src[start..];
            if let Some(end) = rest.find(['\"', '\'']) {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

/// Склейка и нормализация пути внутри архива: `OEBPS/text` + `../images/a.jpg`
/// → `OEBPS/images/a.jpg` (сегменты «.» и «..» сворачиваются).
fn resolve_archive_path(base_dir: &str, href: &str) -> String {
    let h = href.trim();
    if h.starts_with("data:") || h.contains("://") || h.starts_with('/') {
        return norm_path(h);
    }
    let joined = if base_dir.is_empty() {
        h.to_string()
    } else {
        format!("{}/{}", base_dir, h)
    };
    norm_path(&joined)
}

fn norm_path(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

/// strip_html_tags с сохранением `<img>`: тег превращается в Markdown
/// `![alt](<путь-внутри-архива>)` — путь уже разрешён относительно каталога
/// главы, так что ReaderView может сразу читать его через vfs://.
fn strip_html_tags_keep_images(raw: &str, base_dir: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_tag = false;
    let mut skip_block = false;
    let mut current_tag = String::new();

    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '<' {
            in_tag = true;
            current_tag.clear();
            continue;
        }
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let tag_lower = current_tag.trim().to_ascii_lowercase();
                if tag_lower.starts_with("script") || tag_lower.starts_with("style") || tag_lower.starts_with("head") {
                    skip_block = true;
                } else if tag_lower.starts_with("/script") || tag_lower.starts_with("/style") || tag_lower.starts_with("/head") {
                    skip_block = false;
                } else if tag_lower.starts_with("img") {
                    // Иллюстрация: <img src="../images/fig1.jpg" alt="..."/>
                    // → ![...](OEBPS/images/fig1.jpg)
                    let src = attr_of(current_tag.trim(), "src").unwrap_or_default();
                    if !src.is_empty() {
                        let alt = attr_of(current_tag.trim(), "alt")
                            .unwrap_or_default()
                            .replace('[', "(")
                            .replace(']', ")");
                        let resolved = resolve_archive_path(base_dir, &src);
                        out.push_str(&format!("\n\n![{}]({})\n\n", alt, resolved));
                    }
                } else if tag_lower == "p" || tag_lower == "/p"
                    || tag_lower == "div" || tag_lower == "/div"
                {
                    out.push('\n');
                } else if tag_lower == "br" || tag_lower == "br/" || tag_lower == "br /" {
                    out.push('\n');
                } else if tag_lower.starts_with("h1") {
                    out.push_str("\n\n# ");
                } else if tag_lower.starts_with("/h1") {
                    out.push_str("\n\n");
                } else if tag_lower.starts_with("h2") {
                    out.push_str("\n\n## ");
                } else if tag_lower.starts_with("/h2") {
                    out.push_str("\n\n");
                } else if tag_lower.starts_with("h3") || tag_lower.starts_with("h4") || tag_lower.starts_with("h5") || tag_lower.starts_with("h6") {
                    out.push_str("\n\n### ");
                } else if tag_lower.starts_with("/h3") || tag_lower.starts_with("/h4") || tag_lower.starts_with("/h5") || tag_lower.starts_with("/h6") {
                    out.push_str("\n\n");
                } else if tag_lower == "li" {
                    out.push_str("\n- ");
                } else if tag_lower == "blockquote" {
                    out.push_str("\n\n> ");
                } else if tag_lower == "/blockquote" {
                    out.push_str("\n\n");
                } else if tag_lower == "hr" || tag_lower == "hr/" || tag_lower == "hr /" {
                    out.push_str("\n\n---\n\n");
                } else if tag_lower == "tr" || tag_lower.starts_with("tr ") {
                    out.push_str("\n| ");
                } else if tag_lower == "/tr" {
                    out.push('\n');
                } else if tag_lower == "td" || tag_lower == "th" || tag_lower.starts_with("td ") || tag_lower.starts_with("th ") {
                    // cell content follows
                } else if tag_lower == "/td" || tag_lower == "/th" {
                    out.push_str(" | ");
                }
            } else {
                current_tag.push(ch);
            }
            continue;
        }
        if !skip_block {
            out.push(ch);
        }
    }
    unescape_xml_entities(&out)
}

/// Сбор всех binary-элементов FB2: id → (content-type, base64 без пробелов).
/// Выполняется отдельным проходом ДО основного парсинга: в документе
/// binary-элементы идут ПОСЛЕ <body>, а ссылки <image l:href="#id"/>
/// встречаются раньше.
fn collect_fb2_binaries(xml: &str) -> HashMap<String, (String, String)> {
    let mut out: HashMap<String, (String, String)> = HashMap::new();
    let mut cursor = 0usize;
    while let Some(pos) = xml[cursor..].find("<binary") {
        let start = cursor + pos;
        let Some(tag_end_rel) = xml[start..].find('>') else { break };
        let tag_end = start + tag_end_rel;
        let tag_src = &xml[start + 1..tag_end];
        let id = attr_of(tag_src, "id").unwrap_or_default();
        if id.is_empty() {
            cursor = tag_end + 1;
            continue;
        }
        let mut ct = attr_of(tag_src, "content-type").unwrap_or_default();
        if ct.is_empty() {
            let lower = id.to_ascii_lowercase();
            ct = if lower.ends_with(".png") {
                "image/png".to_string()
            } else if lower.ends_with(".gif") {
                "image/gif".to_string()
            } else if lower.ends_with(".svg") {
                "image/svg+xml".to_string()
            } else {
                "image/jpeg".to_string()
            };
        }
        let close = xml[tag_end..].find("</binary>").map(|c| tag_end + c);
        let b64 = match close {
            Some(c) => xml[tag_end + 1..c]
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>(),
            None => String::new(),
        };
        if !b64.is_empty() {
            out.insert(id, (ct, b64));
        }
        cursor = match close {
            Some(c) => c + "</binary>".len(),
            None => tag_end + 1,
        };
    }
    out
}

/// Normalizes text and strips invisible AI token poisons (soft-hyphens, zero-width chars)
pub fn clean_paragraphs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_count = 0usize;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            blank_count += 1;
            if blank_count <= 2 {
                out.push('\n');
            }
        } else {
            blank_count = 0;
            for ch in trimmed.chars() {
                match ch {
                    // Soft hyphen (\u{00AD}) breaks tokenization in AI LLMs & embeddings -> drop!
                    '\u{00AD}' | '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}' => {}
                    // Normalize non-breaking spaces (\u{00A0}) to regular spaces
                    '\u{00A0}' => out.push(' '),
                    // Filter stray control chars except tab
                    c if c.is_control() && c != '\t' => {}
                    c => out.push(c),
                }
            }
            out.push('\n');
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_extractable_format() {
        assert!(is_extractable_format(Path::new("book.pdf")));
        assert!(is_extractable_format(Path::new("book.epub")));
        assert!(is_extractable_format(Path::new("book.fb2")));
        assert!(is_extractable_format(Path::new("book.docx")));
        assert!(is_extractable_format(Path::new("book.odt")));
        assert!(is_extractable_format(Path::new("book.rtf")));
        assert!(is_extractable_format(Path::new("book.mobi")));
        assert!(is_extractable_format(Path::new("book.azw3")));
        assert!(!is_extractable_format(Path::new("code.rs")));
        assert!(!is_extractable_format(Path::new("data.bin")));
    }

    #[test]
    fn test_strip_html_tags_and_markdown() {
        let html = "<h2>Глава 1</h2><p>Привет, &laquo;мир&raquo;! &mdash; 2026</p><table><tr><td>А</td><td>Б</td></tr></table>";
        let stripped = strip_html_tags(html);
        assert!(stripped.contains("## Глава 1"));
        assert!(stripped.contains("Привет, «мир»! — 2026"));
        assert!(stripped.contains("| А | Б |"));
    }

    #[test]
    fn test_clean_paragraphs_soft_hyphens() {
        // Soft hyphen: \u{00AD}
        let dirty = "тер\u{00AD}мо\u{00AD}ди\u{00AD}на\u{00AD}мика\u{00A0}и\u{00A0}магия";
        let cleaned = clean_paragraphs(dirty);
        assert_eq!(cleaned.trim(), "термодинамика и магия");
    }

    #[test]
    fn test_parse_fb2_standard() {
        let fb2 = r#"<?xml version="1.0" encoding="utf-8"?>
<FictionBook>
<description>
  <title-info>
    <genre>sci-fi</genre>
    <author><first-name>Arthur</first-name><last-name>Clarke</last-name></author>
    <book-title>2001</book-title>
    <annotation><p>Classic space story</p></annotation>
  </title-info>
</description>
<body>
  <title><p>Part 1</p></title>
  <p>The dawn of man.</p>
</body>
<binary id="img.jpg">SGVsbG8gV29ybGQ=</binary>
</FictionBook>"#;

        let parsed = parse_fb2_standard(fb2, false);
        assert!(parsed.contains("# 2001"));
        assert!(parsed.contains("Arthur Clarke"));
        assert!(parsed.contains("sci-fi"));
        assert!(parsed.contains("Classic space story"));
        assert!(parsed.contains("Part 1"));
        assert!(parsed.contains("The dawn of man."));
        // Binary payload must be strictly dropped
        assert!(!parsed.contains("SGVsbG8gV29ybGQ="));
    }

    #[test]
    fn test_fb2_keep_images_data_uri() {
        let fb2 = r##"<?xml version="1.0" encoding="utf-8"?>
<FictionBook>
<description>
  <title-info>
    <book-title>З обкладинкою</book-title>
    <coverpage><image l:href="#cover.jpg"/></coverpage>
  </title-info>
</description>
<body>
  <section><p>Текст до ілюстрації.</p><image l:href="#img1.png"/><p>Після.</p></section>
</body>
<binary id="cover.jpg" content-type="image/jpeg">SGVsbG8gV29ybGQ=</binary>
<binary id="img1.png" content-type="image/png">Zm9vYmFy</binary>
</FictionBook>"##;

        let parsed = parse_fb2_standard(fb2, true);
        // Обложка вставлена после шапки
        assert!(parsed.contains("![cover.jpg](data:image/jpeg;base64,SGVsbG8gV29ybGQ=)"));
        // Внутритекстовая иллюстрация как data:-URI
        assert!(parsed.contains("![img1.png](data:image/png;base64,Zm9vYmFy)"));
        // Текст главы не потерялся
        assert!(parsed.contains("Текст до ілюстрації."));
        assert!(parsed.contains("Після."));
    }

    #[test]
    fn test_epub_img_resolved_to_archive_path() {
        let html = "<html><body><h2>Глава 1</h2><p>Текст <img src=\"../images/fig1.jpg\" alt=\"Рисунок 1\"/> после.</p></body></html>";
        let md = strip_html_tags_keep_images(html, "OEBPS/text");
        assert!(md.contains("## Глава 1"));
        assert!(md.contains("![Рисунок 1](OEBPS/images/fig1.jpg)"));
        assert!(md.contains("Текст"));
        // без keep_images — картинка исчезает
        let plain = strip_html_tags(html);
        assert!(!plain.contains("fig1.jpg"));
    }

    #[test]
    fn test_resolve_archive_path() {
        assert_eq!(resolve_archive_path("OEBPS/text", "../images/a.jpg"), "OEBPS/images/a.jpg");
        assert_eq!(resolve_archive_path("", "images/a.jpg"), "images/a.jpg");
        assert_eq!(resolve_archive_path("OEBPS", "./images/a.jpg"), "OEBPS/images/a.jpg");
        assert_eq!(resolve_archive_path("OEBPS/text/sub", "../../cover.png"), "OEBPS/cover.png");
        assert_eq!(resolve_archive_path("OEBPS/text/sub", "../../../cover.png"), "cover.png");
    }

    #[test]
    fn test_attr_of() {
        assert_eq!(
            attr_of("img src=\"../images/fig1.jpg\" alt=\"Рисунок\"", "src"),
            Some("../images/fig1.jpg".to_string())
        );
        assert_eq!(
            attr_of("img src='../images/fig1.jpg'", "alt"),
            None
        );
        assert_eq!(
            attr_of("image l:href=\"#cover.jpg\"", "href"),
            Some("#cover.jpg".to_string())
        );
    }
}
