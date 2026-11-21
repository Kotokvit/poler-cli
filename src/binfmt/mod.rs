//! BINFMT (v0.94.0, Phase 5++.C): формат-aware сегментация исполняемых
//! файлов ELF / PE(32+) для «закона семантики» squeeze.
//!
//! Идея — та же, что у TarObserver для tar: не переупаковывать байты,
//! а ЗНАТЬ границы смысловых единиц формата. Секции `.text`/`.rdata`/
//! `.data`/таблицы релоков режутся FastCDC от собственного начала
//! (gear-хеш сбрасывается на границе сегмента), поэтому:
//!
//! * содержимо-определённые границы чанков не «плывут» от билда к билду
//!   (сдвиг `.text` при изменении `.data` больше не обнуляет дедуп);
//! * регулярные таблицы (`.rela`, `.pdata`, `.reloc`) режутся мелко
//!   и дожимаются глубоким ярусом;
//! * сегменты ложатся в файловую таблицу `.poler` записями вида
//!   `pe:.text` / `elf:.rodata` — тот же механизм, что и tar-записи.
//!
//! Инвариант покрытия (центральный тест фазы): сегменты вместе с
//! Gap-зонами образуют ПОЛНОЕ непересекающееся покрытие файла
//! `[0, file_len)` — восстановление образа бит-в-бит возможно тогда
//! и только тогда, когда покрытие полное ([`verify_coverage`]).
//!
//! Fail-closed: любые аномалии (перекрытия секций, выход за пределы
//! файла, битые заголовки) возвращают `Err` — конвейер squeeze
//! откатывается на слепой RawStream-путь, как сегодня (глава 6.4 плана
//! Phase 5++). Виртуальный хвост `.bss` (virtual_size > raw_size,
//! SHT_NOBITS) файловых байтов не имеет — попадает только в метаданные.
//!
//! Задел, использованный здесь: `pe_scan::Section` (боевой парсер
//! PE32+, фикстуры 7za.exe / libssp-0.dll) и приватный ELF64-парсер
//! `crypto_scan` (e_shoff/shnum/shstrndx, SHF_EXECINSTR) — логика
//! ELF публикуется и расширяется ELF32 + program headers (PT_LOAD).

use std::io::{Read, Seek, SeekFrom};

use crate::archive::dedup::CdcParams;
use crate::archive::stream_writer::CompressTier;

// ────────────────────────── модель ──────────────────────────

/// Класс смысловой единицы бинарника — определяет политику чанков
/// и ярус сжатия (таблица 6 плана Phase 5++).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// Заголовки файла (DOS/COFF/optional/секции, ehdr/phdrs/shdrs).
    Headers,
    /// Исполняемый код (.text, .init, .fini, .plt…).
    Code,
    /// Только-читаемые данные (.rodata…).
    Rodata,
    /// Изменяемые данные (.data…).
    Data,
    /// Регулярные таблицы (релоки, .pdata, .symtab, .got…) —
    /// мелкие чанки, глубокий ярус.
    Reloc,
    /// Отладочные секции (.debug*, .zdebug*).
    Debug,
    /// Выравнивающий промежуток между секциями / хвост файла.
    Gap,
    /// SHT_NOBITS / virtual_size > raw_size: файловых байтов нет,
    /// только метаданные (VMA/размер).
    Bss,
}

impl SegmentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SegmentKind::Headers => "headers",
            SegmentKind::Code => "code",
            SegmentKind::Rodata => "rodata",
            SegmentKind::Data => "data",
            SegmentKind::Reloc => "reloc",
            SegmentKind::Debug => "debug",
            SegmentKind::Gap => "gap",
            SegmentKind::Bss => "bss",
        }
    }

    /// Политика чанков класса (min/avg/max, таблица 6):
    /// * Headers/Gap — крупные чанки (сегменты-выравнивания — тысячи
    ///   байт нулей, дожимаются почти в ноль и одним кадром);
    /// * Code/Rodata/Data/Debug — профиль по умолчанию 64К/256К/1М;
    /// * Reloc и прочие таблицы — мелко, 16К/64К/256К.
    pub fn cdc(&self) -> CdcParams {
        match self {
            SegmentKind::Headers | SegmentKind::Gap => {
                CdcParams::new(1024 * 1024, 2 * 1024 * 1024, 4 * 1024 * 1024)
            }
            SegmentKind::Reloc => CdcParams::new(16 * 1024, 64 * 1024, 256 * 1024),
            _ => CdcParams::new(64 * 1024, 256 * 1024, 1024 * 1024),
        }
    }

    /// Ярус сжатия класса: код — быстрый ярус (главное — дедуп-границы),
    /// данные — авто (по энтропии), таблицы — глубокий (регулярные
    /// паттерны дожимаются глубже).
    pub fn tier(&self) -> CompressTier {
        match self {
            SegmentKind::Code | SegmentKind::Debug | SegmentKind::Headers | SegmentKind::Gap => {
                CompressTier::Fast
            }
            SegmentKind::Reloc => CompressTier::Deep,
            SegmentKind::Rodata | SegmentKind::Data => CompressTier::Auto,
            SegmentKind::Bss => CompressTier::Fast,
        }
    }
}

/// Смысловая единица бинарника: непрерывный файловый диапазон
/// (или виртуальная BSS-зона с `file_len == 0`).
#[derive(Debug, Clone)]
pub struct Segment {
    /// Имя секции формата (".text", ".rodata"…); для служебных зон —
    /// "headers" / "gap@0x…".
    pub name: String,
    pub kind: SegmentKind,
    /// Файловое смещение диапазона.
    pub file_off: u64,
    /// Длина диапазона в файловых байтах (0 — виртуальная зона).
    pub file_len: u64,
    /// Виртуальный адрес загрузки (virtual_address / sh_addr / p_vaddr).
    pub vma: u64,
    /// Секция исполняемая (IMAGE_SCN_MEM_EXECUTE / SHF_EXECINSTR).
    pub exec: bool,
    /// Секция изменяемая (IMAGE_SCN_MEM_WRITE / SHF_WRITE).
    pub write: bool,
}

impl Segment {
    /// Имя записи файловой таблицы .poler: `pe:.text` / `elf:.rodata`.
    pub fn table_name(&self, prefix: &str) -> String {
        format!("{prefix}:{}", self.name)
    }

    /// Есть файловые байты (не виртуальная зона).
    pub fn file_backed(&self) -> bool {
        self.file_len > 0
    }

    fn end(&self) -> u64 {
        self.file_off + self.file_len
    }
}

/// Итог сегментации одного файла: сегменты + префикс имён таблицы
/// ("pe" / "elf").
#[derive(Debug, Clone)]
pub struct Segmentation {
    pub prefix: &'static str,
    pub segments: Vec<Segment>,
}

impl Segmentation {
    /// Файловые (не виртуальные) сегменты в порядке смещений.
    pub fn file_backed(&self) -> impl Iterator<Item = &Segment> {
        self.segments.iter().filter(|s| s.file_backed())
    }
}

/// v0.94.0 BINFMT: пер-сегментная статистика упаковки — JSON-отчёт
/// `--seg-report` (сегменты, чанки, дедуп-хиты, энтропия).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SegmentStat {
    /// Имя записи таблицы (`pe:.text`).
    pub name: String,
    /// Класс сегмента ("code"/"rodata"/…).
    pub kind: &'static str,
    pub file_off: u64,
    pub file_len: u64,
    pub vma: u64,
    /// Чанков нарезано (включая хвостовой).
    pub chunks: u64,
    /// Дедуп-хитов внутри сегмента (BLAKE3-совпадения).
    pub dedup_hits: u64,
    /// Энтропия Шеннона, бит/байт.
    pub entropy: f64,
    pub exec: bool,
    pub write: bool,
}

/// Энтропия Шеннона по гистограмме (бит/байт).
pub(crate) fn entropy_bits(hist: &[u64; 256], total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let mut h = 0.0f64;
    for &c in hist {
        if c == 0 {
            continue;
        }
        let p = c as f64 / total as f64;
        h -= p * p.log2();
    }
    h
}

// ────────────────────────── инвариант покрытия ──────────────────────────

/// Полное непересекающееся покрытие `[0, file_len)`:
/// первая зона начинается в 0, зоны идут подряд без щелей и
/// перекрытий, последняя заканчивается в `file_len`. Выполнение
/// инварианта ⇔ восстановление образа из сегментов бит-в-бит.
pub fn verify_coverage(segs: &[Segment], file_len: u64) -> Result<(), String> {
    let mut covered: Vec<&Segment> = segs.iter().filter(|s| s.file_backed()).collect();
    covered.sort_by_key(|s| s.file_off);
    let mut pos = 0u64;
    for s in covered {
        if s.file_off < pos {
            return Err(format!(
                "перекрытие зон: {} [{}, {}) после {pos}",
                s.name, s.file_off, s.end()
            ));
        }
        if s.file_off > pos {
            return Err(format!("щель [{pos}, {}): потеряны байты", s.file_off));
        }
        pos = s.end();
        if pos > file_len {
            return Err(format!(
                "{} выходит за пределы файла: [{}, {}) > {file_len}",
                s.name, s.file_off, s.end()
            ));
        }
    }
    if pos != file_len {
        return Err(format!("хвост [{pos}, {file_len}) не покрыт"));
    }
    Ok(())
}

// ────────────────────────── PE(32/32+) ──────────────────────────

/// Сегментация PE по заголовочной части файла: `head` обязан содержать
/// всю таблицу секций (`pe_off + 24 + size_of_opt + nsec*40` байт от
/// начала файла). PE32 и PE32+ (таблица секций у них одинакова);
/// классификация — по имени и битам characteristics.
pub fn pe_segments(head: &[u8], file_len: u64) -> Result<Segmentation, String> {
    let u16_at = |o: usize| -> Result<u16, String> {
        head.get(o..o + 2)
            .map(|s| u16::from_le_bytes([s[0], s[1]]))
            .ok_or_else(|| format!("u16@{o:#x}: за пределами заголовка"))
    };
    let u32_at = |o: usize| -> Result<u32, String> {
        head.get(o..o + 4)
            .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
            .ok_or_else(|| format!("u32@{o:#x}: за пределами заголовка"))
    };

    if head.len() < 0x40 || &head[0..2] != b"MZ" {
        return Err("не MZ".into());
    }
    let pe_off = u32_at(0x3C)? as usize;
    if head.get(pe_off..pe_off + 4) != Some(b"PE\0\0") {
        return Err("не PE-сигнатура".into());
    }
    let nsec = u16_at(pe_off + 6)? as usize;
    let size_of_opt = u16_at(pe_off + 20)? as usize;
    if nsec > 96 {
        return Err(format!("nsec={nsec} > 96"));
    }
    let sec_off = pe_off + 24 + size_of_opt;
    let sec_end = sec_off + nsec * 40;
    if sec_end > head.len() {
        return Err(format!(
            "таблица секций [{sec_off}, {sec_end}) не влезла в заголовок ({} Б)",
            head.len()
        ));
    }

    let mut zones: Vec<Segment> = Vec::with_capacity(nsec * 2 + 2);
    // Заголовки: [0, конец таблицы секций) — DOS stub, COFF, optional,
    // сама таблица. Если файл короче (битый) — fail-closed ниже.
    zones.push(Segment {
        name: "headers".into(),
        kind: SegmentKind::Headers,
        file_off: 0,
        file_len: sec_end.min(file_len.min(usize::MAX as u64) as usize) as u64,
        vma: 0,
        exec: false,
        write: false,
    });

    for i in 0..nsec {
        let s = sec_off + i * 40;
        let raw8 = &head[s..s + 8];
        let n = raw8.iter().position(|&c| c == 0).unwrap_or(8);
        let name = String::from_utf8_lossy(&raw8[..n]).into_owned();
        let virtual_size = u32_at(s + 8)?;
        let virtual_address = u32_at(s + 12)?;
        let raw_size = u32_at(s + 16)?;
        let raw_pointer = u32_at(s + 20)?;
        let characteristics = u32_at(s + 36)?;

        if raw_size == 0 {
            // .bss-подобное: файловых байтов нет — только метаданные
            // (виртуальный хвост virtual_size > raw_size в покрытие
            // файловых байтов не входит по построению).
            if virtual_size > 0 {
                zones.push(Segment {
                    kind: SegmentKind::Bss,
                    name,
                    file_off: 0,
                    file_len: 0,
                    vma: virtual_address as u64,
                    exec: characteristics & 0x2000_0000 != 0,
                    write: characteristics & 0x8000_0000 != 0,
                });
            }
            continue;
        }
        let off = raw_pointer as u64;
        let len = raw_size as u64;
        if off >= file_len || off + len > file_len {
            return Err(format!(
                "секция {name}: [{off}, {}) выходит за пределы файла {file_len}",
                off + len
            ));
        }
        zones.push(Segment {
            kind: classify_pe_section(&name, characteristics),
            name,
            file_off: off,
            file_len: len,
            vma: virtual_address as u64,
            exec: characteristics & 0x2000_0000 != 0,
            write: characteristics & 0x8000_0000 != 0,
        });
    }

    let mut segs = fill_gaps(zones, file_len)?;
    segs.sort_by_key(|s| (s.file_off, s.file_len));
    verify_coverage(&segs, file_len)?;
    Ok(Segmentation { prefix: "pe", segments: segs })
}

/// Классификация секции PE по имени (основной канал) с фолбэком
/// на биты characteristics.
fn classify_pe_section(name: &str, characteristics: u32) -> SegmentKind {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        ".text" | ".icode" => return SegmentKind::Code,
        ".rdata" | ".rodata" => return SegmentKind::Rodata,
        ".data" | ".rsrc" => return SegmentKind::Data,
        ".pdata" | ".reloc" | ".idata" | ".didat" | ".crt" | ".tlb" | ".tls" => {
            return SegmentKind::Reloc
        }
        ".bss" => return SegmentKind::Bss,
        _ => {}
    }
    if n.starts_with(".debug") || n.starts_with(".zdebug") {
        return SegmentKind::Debug;
    }
    if n.starts_with(".text") {
        return SegmentKind::Code;
    }
    if characteristics & 0x2000_0000 != 0 {
        SegmentKind::Code
    } else if characteristics & 0x8000_0000 != 0 {
        SegmentKind::Data
    } else {
        SegmentKind::Rodata
    }
}

// ────────────────────────── ELF(32/64, LE) ──────────────────────────

const SHT_NOBITS: u32 = 8;
const SHT_NULL: u32 = 0;
const SHF_WRITE: u64 = 0x1;
const SHF_EXECINSTR: u64 = 0x4;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const SHN_UNDEF: u16 = 0;
const SHN_XINDEX: u16 = 0xFFFF;

/// Сегментация ELF: читает из файла только нужные регионы
/// (e_ident, ehdr, shdrs, shstrtab; для стрипнутых без shdr —
/// program headers PT_LOAD). Позиция файла не важна (seek с нуля).
pub fn elf_segments<R: Read + Seek>(f: &mut R, file_len: u64) -> Result<Segmentation, String> {
    let mut ident = [0u8; 16];
    read_exact_at(f, &mut ident, 0).map_err(|e| format!("ehdr: {e}"))?;
    if &ident[0..4] != b"\x7fELF" {
        return Err("не ELF".into());
    }
    let class = ident[4];
    let data = ident[5];
    if data != 1 {
        return Err("ELF не little-endian".into());
    }
    match class {
        1 => elf32_segments(f, file_len),
        2 => elf64_segments(f, file_len),
        other => Err(format!("EI_CLASS {other} не 32/64")),
    }
}

fn elf64_segments<R: Read + Seek>(f: &mut R, file_len: u64) -> Result<Segmentation, String> {
    let mut ehdr = [0u8; 64];
    read_exact_at(f, &mut ehdr, 0).map_err(|e| format!("ehdr64: {e}"))?;
    let u16_at = |o: usize| u16::from_le_bytes([ehdr[o], ehdr[o + 1]]);
    let u64_at = |o: usize| {
        let mut a = [0u8; 8];
        a.copy_from_slice(&ehdr[o..o + 8]);
        u64::from_le_bytes(a)
    };
    let e_phoff = u64_at(0x20);
    let e_phentsize = u16_at(0x36) as usize;
    let e_phnum = u16_at(0x38) as usize;
    let e_shoff = u64_at(0x28);
    let e_shentsize = u16_at(0x3A) as usize;
    let mut e_shnum = u16_at(0x3C) as usize;
    let mut e_shstrndx = u16_at(0x3E) as usize;

    if e_shoff == 0 || e_shnum == 0 {
        // Секции сняты (shdr-таблицы нет): сегментируем PT_LOAD.
        return phdr_segments(f, file_len, e_phoff, e_phentsize, e_phnum, true);
    }
    if e_shentsize != 64 {
        return Err(format!("shentsize={e_shentsize} != 64"));
    }
    if e_shoff >= file_len {
        return Err(format!("shoff={e_shoff:#x} за пределами файла"));
    }
    // Расширенная нумерация (e_shnum == 0, счётчик в shdr[0].sh_size;
    // shstrndx == SHN_XINDEX, индекс в shdr[0].sh_link)
    let first = read_shdr64(f, e_shoff)?;
    if e_shnum == 0 {
        e_shnum = first.sh_size as usize;
    }
    if e_shstrndx == SHN_XINDEX as usize {
        e_shstrndx = first.sh_link as usize;
    }
    if e_shnum > 65_536 || e_shstrndx == SHN_UNDEF as usize || e_shstrndx >= e_shnum {
        return Err(format!("shnum={e_shnum}, shstrndx={e_shstrndx}"));
    }
    let shdrs_end = e_shoff.saturating_add(e_shnum as u64 * 64);
    if shdrs_end > file_len {
        return Err("таблица секций выходит за пределы файла".into());
    }

    let mut shdrs = Vec::with_capacity(e_shnum);
    for i in 0..e_shnum {
        shdrs.push(read_shdr64(f, e_shoff + i as u64 * 64)?);
    }
    let strtab = read_range(f, shdrs[e_shstrndx].sh_offset, shdrs[e_shstrndx].sh_size)
        .map_err(|e| format!("shstrtab: {e}"))?;

    let mut zones: Vec<Segment> = Vec::with_capacity(e_shnum + 4);
    let headers_end = file_region_end(&shdrs, e_phoff, e_phnum, e_phentsize, shdrs_end);
    zones.push(Segment {
        name: "headers".into(),
        kind: SegmentKind::Headers,
        file_off: 0,
        file_len: headers_end.min(file_len),
        vma: 0,
        exec: false,
        write: false,
    });

    for sh in &shdrs {
        if sh.sh_type == SHT_NULL {
            continue;
        }
        let name = cstr(&strtab, sh.sh_name as usize);
        if sh.sh_type == SHT_NOBITS {
            // .bss: виртуальная зона, файловых байтов нет
            zones.push(Segment {
                kind: SegmentKind::Bss,
                name,
                file_off: 0,
                file_len: 0,
                vma: sh.sh_addr,
                exec: sh.sh_flags & SHF_EXECINSTR != 0,
                write: sh.sh_flags & SHF_WRITE != 0,
            });
            continue;
        }
        if sh.sh_size == 0 || sh.sh_offset == 0 {
            // нет файловых байтов (пустые/зонные секции)
            continue;
        }
        if sh.sh_offset >= file_len || sh.sh_offset + sh.sh_size > file_len {
            return Err(format!(
                "секция {name}: [{:#x}, {:#x}) выходит за пределы файла {file_len}",
                sh.sh_offset,
                sh.sh_offset + sh.sh_size
            ));
        }
        zones.push(Segment {
            kind: classify_elf_section(&name, sh.sh_flags),
            name,
            file_off: sh.sh_offset,
            file_len: sh.sh_size,
            vma: sh.sh_addr,
            exec: sh.sh_flags & SHF_EXECINSTR != 0,
            write: sh.sh_flags & SHF_WRITE != 0,
        });
    }

    let mut segs = fill_gaps(zones, file_len)?;
    segs.sort_by_key(|s| (s.file_off, s.file_len));
    verify_coverage(&segs, file_len)?;
    Ok(Segmentation { prefix: "elf", segments: segs })
}

fn elf32_segments<R: Read + Seek>(f: &mut R, file_len: u64) -> Result<Segmentation, String> {
    let mut ehdr = [0u8; 52];
    read_exact_at(f, &mut ehdr, 0).map_err(|e| format!("ehdr32: {e}"))?;
    let u16_at = |o: usize| u16::from_le_bytes([ehdr[o], ehdr[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes([ehdr[o], ehdr[o + 1], ehdr[o + 2], ehdr[o + 3]]);
    let e_phoff = u32_at(0x1C) as u64;
    let e_phentsize = u16_at(0x2A) as usize;
    let e_phnum = u16_at(0x2C) as usize;
    let e_shoff = u32_at(0x20) as u64;
    let e_shentsize = u16_at(0x2E) as usize;
    let mut e_shnum = u16_at(0x30) as usize;
    let mut e_shstrndx = u16_at(0x32) as usize;

    if e_shoff == 0 || e_shnum == 0 {
        return phdr_segments(f, file_len, e_phoff, e_phentsize, e_phnum, false);
    }
    if e_shentsize != 40 {
        return Err(format!("shentsize={e_shentsize} != 40"));
    }
    if e_shoff >= file_len {
        return Err(format!("shoff={e_shoff:#x} за пределами файла"));
    }
    let first = read_shdr32(f, e_shoff)?;
    if e_shnum == 0 {
        e_shnum = first.sh_size as usize;
    }
    if e_shstrndx == SHN_XINDEX as usize {
        e_shstrndx = first.sh_link as usize;
    }
    if e_shnum > 65_536 || e_shstrndx == SHN_UNDEF as usize || e_shstrndx >= e_shnum {
        return Err(format!("shnum={e_shnum}, shstrndx={e_shstrndx}"));
    }
    let shdrs_end = e_shoff.saturating_add(e_shnum as u64 * 40);
    if shdrs_end > file_len {
        return Err("таблица секций выходит за пределы файла".into());
    }

    let mut shdrs = Vec::with_capacity(e_shnum);
    for i in 0..e_shnum {
        shdrs.push(read_shdr32(f, e_shoff + i as u64 * 40)?);
    }
    let strtab = read_range(f, shdrs[e_shstrndx].sh_offset as u64, shdrs[e_shstrndx].sh_size as u64)
        .map_err(|e| format!("shstrtab: {e}"))?;

    let mut zones: Vec<Segment> = Vec::with_capacity(e_shnum + 4);
    let headers_end = file_region_end32(&shdrs, e_phoff, e_phnum, shdrs_end);
    zones.push(Segment {
        name: "headers".into(),
        kind: SegmentKind::Headers,
        file_off: 0,
        file_len: headers_end.min(file_len),
        vma: 0,
        exec: false,
        write: false,
    });

    for sh in &shdrs {
        if sh.sh_type == SHT_NULL {
            continue;
        }
        let name = cstr(&strtab, sh.sh_name as usize);
        if sh.sh_type == SHT_NOBITS {
            zones.push(Segment {
                kind: SegmentKind::Bss,
                name,
                file_off: 0,
                file_len: 0,
                vma: sh.sh_addr as u64,
                exec: sh.sh_flags as u64 & SHF_EXECINSTR != 0,
                write: sh.sh_flags as u64 & SHF_WRITE != 0,
            });
            continue;
        }
        if sh.sh_size == 0 || sh.sh_offset == 0 {
            continue;
        }
        let off = sh.sh_offset as u64;
        let len = sh.sh_size as u64;
        if off >= file_len || off + len > file_len {
            return Err(format!(
                "секция {name}: [{off:#x}, {:#x}) выходит за пределы файла {file_len}",
                off + len
            ));
        }
        zones.push(Segment {
            kind: classify_elf_section(&name, sh.sh_flags as u64),
            name,
            file_off: off,
            file_len: len,
            vma: sh.sh_addr as u64,
            exec: sh.sh_flags as u64 & SHF_EXECINSTR != 0,
            write: sh.sh_flags as u64 & SHF_WRITE != 0,
        });
    }

    let mut segs = fill_gaps(zones, file_len)?;
    segs.sort_by_key(|s| (s.file_off, s.file_len));
    verify_coverage(&segs, file_len)?;
    Ok(Segmentation { prefix: "elf", segments: segs })
}

/// Стрипнутый ELF без секций: PT_LOAD-сегменты (p_offset/p_filesz),
/// классификация по флагам PF_X/PF_W, имена load0…loadN.
fn phdr_segments<R: Read + Seek>(
    f: &mut R,
    file_len: u64,
    phoff: u64,
    phentsize: usize,
    phnum: usize,
    is64: bool,
) -> Result<Segmentation, String> {
    if phoff == 0 || phnum == 0 || phnum > 1024 {
        return Err("нет ни секций, ни program headers".into());
    }
    let want = if is64 { 56 } else { 32 };
    if phentsize < want {
        return Err(format!("phentsize={phentsize} < {want}"));
    }
    let mut zones: Vec<Segment> = Vec::with_capacity(phnum + 2);
    let mut loads = 0usize;
    let mut first_load_off: Option<u64> = None;
    for i in 0..phnum {
        let mut ph = vec![0u8; phentsize];
        read_exact_at(f, &mut ph, phoff + i as u64 * phentsize as u64)
            .map_err(|e| format!("phdr[{i}]: {e}"))?;
        let p_type = u32::from_le_bytes([ph[0], ph[1], ph[2], ph[3]]);
        if p_type != PT_LOAD {
            continue;
        }
        let (p_offset, p_filesz, p_vaddr, flags) = if is64 {
            let o = u64le(&ph[8..16]);
            let v = u64le(&ph[16..24]);
            let fsz = u64le(&ph[32..40]);
            let fl = u32::from_le_bytes([ph[4], ph[5], ph[6], ph[7]]);
            (o, fsz, v, fl)
        } else {
            let o = u32le(&ph[4..8]) as u64;
            let v = u32le(&ph[8..12]) as u64;
            let fsz = u32le(&ph[16..20]) as u64;
            let fl = u32::from_le_bytes([ph[24], ph[25], ph[26], ph[27]]);
            (o, fsz, v, fl)
        };
        if p_filesz == 0 {
            continue;
        }
        if p_offset + p_filesz > file_len {
            return Err(format!(
                "PT_LOAD[{i}]: [{p_offset}, {}) за пределами файла",
                p_offset + p_filesz
            ));
        }
        if first_load_off.is_none() {
            first_load_off = Some(p_offset);
        }
        let kind = if flags & PF_X != 0 {
            SegmentKind::Code
        } else if flags & PF_W != 0 {
            SegmentKind::Data
        } else {
            SegmentKind::Rodata
        };
        zones.push(Segment {
            name: format!("load{loads}"),
            kind,
            file_off: p_offset,
            file_len: p_filesz,
            vma: p_vaddr,
            exec: flags & PF_X != 0,
            write: flags & PF_W != 0,
        });
        loads += 1;
    }
    if zones.is_empty() {
        return Err("PT_LOAD-сегментов с байтами нет".into());
    }
    // Заголовки: [0, старт первого PT_LOAD). Если первый LOAD стартует
    // с 0 (типовой случай) — отдельной зоны нет: load0 покрывает и
    // заголовки, щели между LOAD-ами закроют Gap-зоны.
    if let Some(start) = first_load_off.filter(|&o| o > 0) {
        zones.insert(
            0,
            Segment {
                name: "headers".into(),
                kind: SegmentKind::Headers,
                file_off: 0,
                file_len: start.min(file_len),
                vma: 0,
                exec: false,
                write: false,
            },
        );
    }
    let mut segs = fill_gaps(zones, file_len)?;
    segs.sort_by_key(|s| (s.file_off, s.file_len));
    verify_coverage(&segs, file_len)?;
    Ok(Segmentation { prefix: "elf", segments: segs })
}

#[derive(Debug, Clone, Copy)]
struct Shdr64 {
    sh_name: u32,
    sh_type: u32,
    sh_flags: u64,
    sh_addr: u64,
    sh_offset: u64,
    sh_size: u64,
    sh_link: u32,
}

#[derive(Debug, Clone, Copy)]
struct Shdr32 {
    sh_name: u32,
    sh_type: u32,
    sh_flags: u32,
    sh_addr: u32,
    sh_offset: u32,
    sh_size: u32,
    sh_link: u32,
}

fn read_shdr64<R: Read + Seek>(f: &mut R, off: u64) -> Result<Shdr64, String> {
    let mut b = [0u8; 64];
    read_exact_at(f, &mut b, off).map_err(|e| format!("shdr64@{off:#x}: {e}"))?;
    Ok(Shdr64 {
        sh_name: u32le(&b[0..4]),
        sh_type: u32le(&b[4..8]),
        sh_flags: u64le(&b[8..16]),
        sh_addr: u64le(&b[16..24]),
        sh_offset: u64le(&b[24..32]),
        sh_size: u64le(&b[32..40]),
        sh_link: u32le(&b[40..44]),
    })
}

fn read_shdr32<R: Read + Seek>(f: &mut R, off: u64) -> Result<Shdr32, String> {
    let mut b = [0u8; 40];
    read_exact_at(f, &mut b, off).map_err(|e| format!("shdr32@{off:#x}: {e}"))?;
    Ok(Shdr32 {
        sh_name: u32le(&b[0..4]),
        sh_type: u32le(&b[4..8]),
        sh_flags: u32le(&b[8..12]),
        sh_addr: u32le(&b[12..16]),
        sh_offset: u32le(&b[16..20]),
        sh_size: u32le(&b[20..24]),
        sh_link: u32le(&b[24..28]),
    })
}

/// Классификация секции ELF по имени с фолбэком на sh_flags.
fn classify_elf_section(name: &str, flags: u64) -> SegmentKind {
    let n = name;
    if n == ".text"
        || n == ".init"
        || n == ".fini"
        || n == ".plt"
        || n == ".plt.got"
        || n == ".plt.sec"
        || n.starts_with(".text")
    {
        return SegmentKind::Code;
    }
    if n.starts_with(".rodata") || n.starts_with(".note") || n == ".comment" {
        return SegmentKind::Rodata;
    }
    if n.starts_with(".data") {
        return SegmentKind::Data;
    }
    if n.starts_with(".rela") || n.starts_with(".rel.") {
        return SegmentKind::Reloc;
    }
    if n.starts_with(".debug") || n.starts_with(".zdebug") {
        return SegmentKind::Debug;
    }
    match n {
        ".symtab" | ".strtab" | ".shstrtab" | ".dynsym" | ".dynstr" | ".gnu.hash" | ".hash"
        | ".gnu.version" | ".gnu.version_r" | ".interp" | ".dynamic" | ".got" | ".got.plt"
        | ".eh_frame" | ".eh_frame_hdr" | ".init_array" | ".fini_array" | ".stab" | ".stabstr" => {
            return SegmentKind::Reloc
        }
        ".bss" | ".tbss" => return SegmentKind::Bss,
        _ => {}
    }
    if flags & SHF_EXECINSTR != 0 {
        SegmentKind::Code
    } else if flags & SHF_WRITE != 0 {
        SegmentKind::Data
    } else {
        SegmentKind::Rodata
    }
}

// ────────────────────────── общие помощники ──────────────────────────

/// Заполнить щели между зонами Gap-сегментами; результат отсортирован
/// по file_off. Виртуальные зоны (file_len == 0) переносятся в конец —
/// в покрытие они не входят, но остаются в метаданных сегментации.
fn fill_gaps(zones: Vec<Segment>, file_len: u64) -> Result<Vec<Segment>, String> {
    let mut covered: Vec<Segment> = zones.iter().filter(|s| s.file_backed()).cloned().collect();
    let virtuals: Vec<Segment> = zones.into_iter().filter(|s| !s.file_backed()).collect();
    covered.sort_by_key(|s| s.file_off);
    let mut out: Vec<Segment> = Vec::with_capacity(covered.len() * 2 + 2);
    let mut pos = 0u64;
    for s in covered {
        if s.file_off < pos {
            return Err(format!(
                "перекрытие секций: {} [{}, {}) против {pos}",
                s.name, s.file_off,
                s.file_off + s.file_len
            ));
        }
        if s.file_off > pos {
            out.push(Segment {
                name: format!("gap@{pos:#x}"),
                kind: SegmentKind::Gap,
                file_off: pos,
                file_len: s.file_off - pos,
                vma: 0,
                exec: false,
                write: false,
            });
        }
        let end = s.file_off + s.file_len;
        out.push(s);
        pos = end;
    }
    if pos < file_len {
        out.push(Segment {
            name: format!("gap@{pos:#x}"),
            kind: SegmentKind::Gap,
            file_off: pos,
            file_len: file_len - pos,
            vma: 0,
            exec: false,
            write: false,
        });
    }
    out.extend(virtuals);
    Ok(out)
}

/// Конец «заголовочной» зоны ELF: максимум из конца таблицы секций
/// и конца таблицы program headers, но не дальше первой секции
/// с файловыми байтами (обе таблицы живут в головной части файла
/// типовой сборки; иначе хвост закроет Gap).
fn file_region_end(shdrs: &[Shdr64], phoff: u64, phnum: usize, phentsize: usize, shdrs_end: u64) -> u64 {
    let mut end = shdrs_end;
    if phoff > 0 && phnum > 0 && phnum < 4096 {
        end = end.max(phoff + phnum as u64 * phentsize as u64);
    }
    if let Some(first) = shdrs
        .iter()
        .find(|s| s.sh_type != SHT_NULL && s.sh_size > 0 && s.sh_offset > 0)
    {
        end = end.min(first.sh_offset);
    }
    end
}

fn file_region_end32(shdrs: &[Shdr32], phoff: u64, phnum: usize, shdrs_end: u64) -> u64 {
    let mut end = shdrs_end;
    if phoff > 0 && phnum > 0 && phnum < 4096 {
        end = end.max(phoff + phnum as u64 * 32);
    }
    if let Some(first) = shdrs
        .iter()
        .find(|s| s.sh_type != SHT_NULL && s.sh_size > 0 && s.sh_offset > 0)
    {
        end = end.min(first.sh_offset as u64);
    }
    end
}

fn read_exact_at<R: Read + Seek>(f: &mut R, buf: &mut [u8], off: u64) -> std::io::Result<()> {
    f.seek(SeekFrom::Start(off))?;
    std::io::Read::read_exact(f, buf)
}

fn read_range<R: Read + Seek>(f: &mut R, off: u64, len: u64) -> std::io::Result<Vec<u8>> {
    let mut v = vec![0u8; len as usize];
    read_exact_at(f, &mut v, off)?;
    Ok(v)
}

fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn u64le(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

fn cstr(tab: &[u8], off: usize) -> String {
    if off >= tab.len() {
        return String::new();
    }
    let end = tab[off..]
        .iter()
        .position(|&c| c == 0)
        .map(|p| p + off)
        .unwrap_or(tab.len());
    String::from_utf8_lossy(&tab[off..end]).into_owned()
}

// ────────────────────────── тесты ──────────────────────────

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;

    const SEVEN_ZA: &[u8] = include_bytes!("../../tests/fixtures/pe/7za.exe");
    const LIBSSP: &[u8] = include_bytes!("../../tests/fixtures/pe/libssp-0.dll");

    // ── синтетика: минимальный ELF64 с секциями ──
    // раскладка: ehdr(64) | .text | .rodata | .data | shstrtab | shdrs
    // (.bss — SHT_NOBITS, файловых байтов нет)
    pub(crate) fn synth_elf64(text_len: usize, data_len: usize) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"\x7fELF");
        buf.extend_from_slice(&[2, 1, 1, 0]); // ELF64, LE, CURRENT, SYSV
        buf.extend_from_slice(&[0u8; 8]);
        buf.extend_from_slice(&[0u8; 48]); // хвост ehdr до 64 Б
        let text_off = buf.len() as u64;
        let mut text: Vec<u8> = Vec::with_capacity(text_len);
        let mut st = 0xABCDEF01u64;
        while text.len() < text_len {
            st = st.wrapping_mul(6364136223846793005).wrapping_add(1);
            text.extend_from_slice(format!("mov rax, {st:#x}; xor rbx, rbx\n").as_bytes());
        }
        text.truncate(text_len);
        buf.extend_from_slice(&text);
        let rodata_off = buf.len() as u64;
        let rodata: Vec<u8> = "POLER sovereign engine\0".repeat(64).into_bytes();
        buf.extend_from_slice(&rodata);
        let data_off = buf.len() as u64;
        let mut data: Vec<u8> = Vec::with_capacity(data_len);
        let mut d = 0x5555_5555_5555_5555u64;
        while data.len() < data_len {
            d = d.wrapping_mul(2862933555777941757).wrapping_add(3037000493);
            data.extend_from_slice(&d.to_le_bytes());
        }
        data.truncate(data_len);
        buf.extend_from_slice(&data);
        let shstr_off = buf.len() as u64;
        let shstr: Vec<u8> = b"\0.text\0.rodata\0.data\0.bss\0.shstrtab\0".to_vec();
        buf.extend_from_slice(&shstr);
        while buf.len() % 8 != 0 {
            buf.push(0);
        }
        let shoff = buf.len() as u64;
        let nsec = 6usize; // null, .text, .rodata, .data, .bss, .shstrtab
        let no = |n: &str| -> u32 {
            let mut i = 0usize;
            for part in shstr.split(|&c| c == 0) {
                if part == n.as_bytes() {
                    return i as u32;
                }
                i += part.len() + 1;
            }
            unreachable!("имя {n} обязано быть в shstrtab")
        };
        let mut shb = Vec::new();
        let mut push = |name: &str, ty: u32, flags: u64, addr: u64, off: u64, size: u64| {
            shb.extend_from_slice(&no(name).to_le_bytes());
            shb.extend_from_slice(&ty.to_le_bytes());
            shb.extend_from_slice(&flags.to_le_bytes());
            shb.extend_from_slice(&addr.to_le_bytes());
            shb.extend_from_slice(&off.to_le_bytes());
            shb.extend_from_slice(&size.to_le_bytes());
            shb.extend_from_slice(&0u32.to_le_bytes()); // link
            shb.extend_from_slice(&0u32.to_le_bytes()); // info
            shb.extend_from_slice(&0u64.to_le_bytes()); // addralign
            shb.extend_from_slice(&0u64.to_le_bytes()); // entsize
        };
        push("", 0, 0, 0, 0, 0); // SHN_UNDEF
        push(".text", 1, 0x6, 0x401000, text_off, text.len() as u64);
        push(".rodata", 1, 0x2, 0x402000, rodata_off, rodata.len() as u64);
        push(".data", 1, 0x3, 0x403000, data_off, data.len() as u64);
        push(".bss", 8, 0x3, 0x403800, 0, 0x400); // NOBITS — виртуальная
        push(".shstrtab", 3, 0, 0, shstr_off, shstr.len() as u64);
        buf.extend_from_slice(&shb);
        buf[0x28..0x30].copy_from_slice(&shoff.to_le_bytes()); // e_shoff
        buf[0x3A..0x3C].copy_from_slice(&(64u16).to_le_bytes()); // e_shentsize
        buf[0x3C..0x3E].copy_from_slice(&(nsec as u16).to_le_bytes()); // e_shnum
        buf[0x3E..0x40].copy_from_slice(&(5u16).to_le_bytes()); // e_shstrndx
        buf
    }

    #[test]
    fn elf64_coverage_and_classification() {
        let img = synth_elf64(300 * 1024, 4 * 1024);
        let mut cur = Cursor::new(img.clone());
        let seg = elf_segments(&mut cur, img.len() as u64).expect("сегментация синтетики");
        verify_coverage(&seg.segments, img.len() as u64).unwrap();
        assert_eq!(seg.prefix, "elf");
        let by_name = |n: &str| seg.segments.iter().find(|s| s.name == n).unwrap();
        assert_eq!(by_name(".text").kind, SegmentKind::Code);
        assert!(by_name(".text").exec);
        assert_eq!(by_name(".rodata").kind, SegmentKind::Rodata);
        assert_eq!(by_name(".data").kind, SegmentKind::Data);
        assert!(by_name(".data").write);
        // .bss — виртуальная зона: 0 файловых байтов, но в метаданных
        let bss = by_name(".bss");
        assert_eq!(bss.kind, SegmentKind::Bss);
        assert_eq!(bss.file_len, 0);
        assert_eq!(bss.vma, 0x403800);
        // заголовки — единая головная зона до .text
        assert_eq!(by_name("headers").file_off, 0);
        assert_eq!(by_name("headers").file_len, 64);
        // хвостовой gap до конца файла (за shdrs ничего нет — gap до конца)
        assert!(seg.segments.iter().any(|s| s.kind == SegmentKind::Gap));
    }

    #[test]
    fn elf64_roundtrip_layout_bitexact() {
        // Сегменты в порядке file_off покрывают файл целиком: конкатенация
        // диапазонов == исходный образ (ядро инварианта покрытия).
        let img = synth_elf64(120 * 1024, 2 * 1024);
        let mut cur = Cursor::new(img.clone());
        let seg = elf_segments(&mut cur, img.len() as u64).unwrap();
        let mut rebuilt = Vec::new();
        let mut sorted: Vec<&Segment> = seg.file_backed().collect();
        sorted.sort_by_key(|s| s.file_off);
        for s in sorted {
            rebuilt.extend_from_slice(&img[s.file_off as usize..(s.file_off + s.file_len) as usize]);
        }
        assert_eq!(rebuilt, img, "покрытие обязано восстанавливать образ бит-в-бит");
    }

    #[test]
    fn elf64_stripped_falls_back_to_phdrs() {
        // shoff=0: PT_LOAD-сегменты. ehdr(64) | phdr(2×56) | code | rodata
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"\x7fELF");
        buf.extend_from_slice(&[2, 1, 1, 0]);
        buf.extend_from_slice(&[0u8; 8]);
        buf.extend_from_slice(&[0u8; 48]);
        let phoff = 64u64;
        buf[0x20..0x28].copy_from_slice(&phoff.to_le_bytes());
        buf[0x36..0x38].copy_from_slice(&(56u16).to_le_bytes()); // phentsize
        buf[0x38..0x3A].copy_from_slice(&(2u16).to_le_bytes()); // phnum
        buf[0x28..0x30].copy_from_slice(&0u64.to_le_bytes()); // e_shoff = 0
        let mut ph = vec![0u8; 2 * 56];
        // PT_LOAD RX
        ph[0..4].copy_from_slice(&1u32.to_le_bytes());
        ph[4..8].copy_from_slice(&5u32.to_le_bytes()); // R+X
        ph[8..16].copy_from_slice(&(0x80u64).to_le_bytes()); // offset
        ph[16..24].copy_from_slice(&0x401000u64.to_le_bytes()); // vaddr
        ph[32..40].copy_from_slice(&(1024u64).to_le_bytes()); // filesz
        // PT_LOAD RW — сразу за первым (offset 0x80+1024)
        ph[56..60].copy_from_slice(&1u32.to_le_bytes());
        ph[60..64].copy_from_slice(&6u32.to_le_bytes()); // R+W
        ph[64..72].copy_from_slice(&(0x480u64).to_le_bytes());
        ph[72..80].copy_from_slice(&0x402000u64.to_le_bytes());
        ph[88..96].copy_from_slice(&(512u64).to_le_bytes());
        buf.extend_from_slice(&ph);
        buf.extend((0..1024u32).map(|i| i as u8));
        buf.extend([0x77u8; 512]);
        let total = buf.len() as u64;
        let mut cur = Cursor::new(buf.clone());
        let seg = elf_segments(&mut cur, total).expect("phdr-фолбэк");
        verify_coverage(&seg.segments, total).unwrap();
        assert!(seg.segments.iter().any(|s| s.name == "load0" && s.kind == SegmentKind::Code));
        assert!(seg.segments.iter().any(|s| s.name == "load1" && s.kind == SegmentKind::Data));
    }

    #[test]
    fn elf32_segments_parse() {
        // минимальный ELF32: ehdr(52) | .text | .data | shstrtab | shdrs
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"\x7fELF");
        buf.extend_from_slice(&[1, 1, 1, 0]); // 32-bit LE
        buf.extend_from_slice(&[0u8; 8]);
        buf.extend_from_slice(&[0u8; 36]);
        assert_eq!(buf.len(), 52);
        let text_off = buf.len();
        let text = b"x86 code bytes here\0".repeat(64);
        buf.extend_from_slice(&text);
        let data_off = buf.len();
        let data = [0x11u8; 128];
        buf.extend_from_slice(&data);
        let shstr_off = buf.len();
        let shstr: Vec<u8> = b"\0.text\0.data\0.bss\0.shstrtab\0".to_vec();
        buf.extend_from_slice(&shstr);
        while buf.len() % 4 != 0 {
            buf.push(0);
        }
        let shoff = buf.len();
        let no = |n: &str| -> u32 {
            let mut i = 0usize;
            for part in shstr.split(|&c| c == 0) {
                if part == n.as_bytes() {
                    return i as u32;
                }
                i += part.len() + 1;
            }
            unreachable!()
        };
        let mut shb = Vec::new();
        let mut push = |name: &str, ty: u32, fl: u32, addr: u32, off: u32, size: u32| {
            shb.extend_from_slice(&no(name).to_le_bytes());
            shb.extend_from_slice(&ty.to_le_bytes());
            shb.extend_from_slice(&fl.to_le_bytes());
            shb.extend_from_slice(&addr.to_le_bytes());
            shb.extend_from_slice(&off.to_le_bytes());
            shb.extend_from_slice(&size.to_le_bytes());
            shb.extend_from_slice(&0u32.to_le_bytes()); // link
            shb.extend_from_slice(&0u32.to_le_bytes()); // info
            shb.extend_from_slice(&4u32.to_le_bytes()); // align
            shb.extend_from_slice(&0u32.to_le_bytes()); // entsize
        };
        push("", 0, 0, 0, 0, 0);
        push(".text", 1, 0x6, 0x1000, text_off as u32, text.len() as u32);
        push(".data", 1, 0x3, 0x2000, data_off as u32, data.len() as u32);
        push(".bss", 8, 0x3, 0x2100, 0, 0x100);
        push(".shstrtab", 3, 0, 0, shstr_off as u32, shstr.len() as u32);
        buf.extend_from_slice(&shb);
        let total = buf.len() as u64;
        buf[0x20..0x24].copy_from_slice(&(shoff as u32).to_le_bytes());
        buf[0x2E..0x30].copy_from_slice(&(40u16).to_le_bytes());
        buf[0x30..0x32].copy_from_slice(&(5u16).to_le_bytes()); // shnum
        buf[0x32..0x34].copy_from_slice(&(4u16).to_le_bytes()); // shstrndx

        let mut cur = Cursor::new(buf.clone());
        let seg = elf_segments(&mut cur, total).expect("ELF32 парсится");
        verify_coverage(&seg.segments, total).unwrap();
        assert_eq!(seg.prefix, "elf");
        assert!(seg.segments.iter().any(|s| s.name == ".text" && s.kind == SegmentKind::Code));
        assert!(seg.segments.iter().any(|s| s.name == ".bss" && s.file_len == 0));
    }

    #[test]
    fn pe_real_fixtures_coverage() {
        for (img, name) in [(SEVEN_ZA, "7za.exe"), (LIBSSP, "libssp-0.dll")] {
            let seg = pe_segments(img, img.len() as u64)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            verify_coverage(&seg.segments, img.len() as u64)
                .unwrap_or_else(|e| panic!("{name}: покрытие: {e}"));
            assert_eq!(seg.prefix, "pe");
            let names: Vec<&str> = seg.segments.iter().map(|s| s.name.as_str()).collect();
            assert!(names.contains(&".text"), "{name}: нет .text: {names:?}");
            assert!(
                names.iter().any(|n| n.starts_with("gap@")),
                "{name}: нет gap-зон"
            );
            let text = seg.segments.iter().find(|s| s.name == ".text").unwrap();
            assert_eq!(text.kind, SegmentKind::Code);
            assert!(text.exec);
        }
    }

    #[test]
    fn pe_rejects_broken_headers() {
        assert!(pe_segments(b"not a pe", 8).is_err());
        let mut mz = vec![0u8; 0x100];
        mz[0] = b'M';
        mz[1] = b'Z';
        assert!(pe_segments(&mz, 0x100).is_err());
    }

    #[test]
    fn coverage_rejects_holes_and_overlaps() {
        let mk = |off: u64, len: u64| Segment {
            name: "x".into(),
            kind: SegmentKind::Data,
            file_off: off,
            file_len: len,
            vma: 0,
            exec: false,
            write: false,
        };
        // щель в середине
        assert!(verify_coverage(&[mk(0, 10), mk(12, 8)], 20).is_err());
        // хвост не покрыт
        assert!(verify_coverage(&[mk(0, 10), mk(10, 8)], 20).is_err());
        // перекрытие
        assert!(verify_coverage(&[mk(0, 12), mk(10, 10)], 20).is_err());
        // полное покрытие — ок (виртуальные зоны не мешают)
        let mut bss = mk(0, 0);
        bss.kind = SegmentKind::Bss;
        bss.vma = 0x4000;
        assert!(verify_coverage(&[mk(0, 10), mk(10, 10), bss], 20).is_ok());
    }

    // ── синтетика PE для конвейерных тестов (ingest.rs) ──
    // раскладка: MZ/PE-заголовки | .text | .rdata | .data | .reloc
    pub(crate) fn synth_pe(text_len: usize, data_len: usize) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        // DOS header (64 Б) + stub до 0x80
        buf.extend_from_slice(&[0x4D, 0x5A, 0x90, 0x00, 0x03, 0x00, 0x00, 0x00]);
        buf.extend_from_slice(&[0u8; 0x34]);
        const PE_OFF: usize = 0x80;
        buf.resize(PE_OFF, 0);
        buf[0x3C..0x40].copy_from_slice(&(PE_OFF as u32).to_le_bytes());
        // COFF: PE32+ AMD64, 4 секции
        buf.extend_from_slice(b"PE\0\0");
        buf.extend_from_slice(&(0x8664u16).to_le_bytes()); // machine
        buf.extend_from_slice(&(4u16).to_le_bytes()); // nsec
        buf.extend_from_slice(&[0u8; 12]); // timestamp/syms/size
        buf.extend_from_slice(&(240u16).to_le_bytes()); // size_of_opt
        buf.extend_from_slice(&0x22u16.to_le_bytes()); // chars
        // optional header PE32+ (240 Б): магия + нули
        buf.extend_from_slice(&(0x20Bu16).to_le_bytes());
        buf.extend_from_slice(&[0u8; 238]);
        // смещения тел секций: после таблицы секций (4 записи × 40 Б)
        let align = 0x200usize;
        let sec_table_end = buf.len() + 4 * 40;
        let text_off = align_len(sec_table_end, align);
        let rdata_off = align_len(text_off + text_len, align);
        let data_off = align_len(rdata_off + 256, align);
        let reloc_off = align_len(data_off + data_len, align);
        // таблица секций
        let mut push_sec =
            |buf: &mut Vec<u8>, name: &str, vsize: u32, vaddr: u32, raw_size: u32, raw_ptr: u32, chars: u32| {
                let mut nb = [0u8; 8];
                nb[..name.len()].copy_from_slice(name.as_bytes());
                buf.extend_from_slice(&nb);
                buf.extend_from_slice(&vsize.to_le_bytes());
                buf.extend_from_slice(&vaddr.to_le_bytes());
                buf.extend_from_slice(&raw_size.to_le_bytes());
                buf.extend_from_slice(&raw_ptr.to_le_bytes());
                buf.extend_from_slice(&[0u8; 12]); // relo/lineno ptrs + счётчики
                buf.extend_from_slice(&chars.to_le_bytes());
            };
        push_sec(&mut buf, ".text", text_len as u32, 0x1000, text_len as u32, text_off as u32, 0x6000_0020);
        push_sec(&mut buf, ".rdata", 256, 0x2000, 256, rdata_off as u32, 0x4000_0040);
        push_sec(&mut buf, ".data", data_len as u32, 0x3000, data_len as u32, data_off as u32, 0xC000_0040);
        push_sec(&mut buf, ".reloc", 128, 0x4000, 128, reloc_off as u32, 0x4200_0040);
        // тела секций
        pad_to(&mut buf, text_off);
        let mut text: Vec<u8> = Vec::with_capacity(text_len);
        let mut st = 0x1234_5678u64;
        while text.len() < text_len {
            st = st.wrapping_mul(6364136223846793005).wrapping_add(1);
            text.extend_from_slice(
                format!("lea rdi, [rip+{st:#x}]; call qword ptr [rax]\n").as_bytes(),
            );
        }
        text.truncate(text_len);
        buf.extend_from_slice(&text);
        pad_to(&mut buf, rdata_off);
        buf.extend_from_slice(b"PE synthetic rodata zone\0".repeat(9).as_slice());
        pad_to(&mut buf, data_off);
        let mut d = 0x9999_9999_9999_9999u64;
        let mut data: Vec<u8> = Vec::with_capacity(data_len);
        while data.len() < data_len {
            d = d.wrapping_mul(6364136223846793005).wrapping_add(7);
            data.extend_from_slice(&d.to_le_bytes());
        }
        data.truncate(data_len);
        buf.extend_from_slice(&data);
        pad_to(&mut buf, reloc_off);
        let reloc: Vec<u8> = (0..32u32).flat_map(|i| (i * 0x1000 + 0x3).to_le_bytes()).collect();
        buf.extend_from_slice(&reloc);
        buf
    }

    pub(crate) fn align_len(v: usize, align: usize) -> usize {
        (v + align - 1) / align * align
    }

    fn pad_to(buf: &mut Vec<u8>, target: usize) {
        while buf.len() < target {
            buf.push(0);
        }
    }
}
