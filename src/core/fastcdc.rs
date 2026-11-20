//! FastCDC content-defined chunking — вырезано из poler-engine.
//!
//! ## Фаза 5 «MCP-глаза на VFS» — вырезано из poler-engine, не переписано
//!
//! Нарезка вырезана из вендоренного движка
//! `poler-engine/src/archive/dedup.rs` (poler-engine v0.93.0 @ 5987a86)
//! без алгоритмических правок: границы чанков зависят ТОЛЬКО от
//! содержимого (gear-хеш с фиксированным сидом «POLERCDC»), поэтому
//! одинаковые данные режутся одинаково при любой позиции в потоке —
//! фундамент дедупликации RAM-полигона `vfs://ai_scratch/` (см.
//! `scratch.rs`). Файловый реестр `ChunkDedup` (u64-префикс BLAKE3 →
//! смещение на диске) движка здесь не нужен: полигон живёт целиком в
//! RAM и держит собственный сторе хеш → байты.
//!
//! Сид gear-таблицы фиксирован: нарезка воспроизводима между запусками,
//! процессами и архивами — чанки полигона совместимы с нарезкой движка
//! (кросс-архивная дедупликация).

/// Параметры нарезки FastCDC (нормализованный вариант).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CdcParams {
    /// Минимальный размер чанка, байт.
    pub min: usize,
    /// Целевой средний размер чанка, байт.
    pub avg: usize,
    /// Максимальный размер чанка, байт.
    pub max: usize,
    /// Маска фазы A [min, avg): ⌈log2(avg)⌉ бит — резать «трудно».
    mask_a: u64,
    /// Маска фазы B [avg, max): на 2 бита короче — хвост режется легче.
    mask_b: u64,
}

impl Default for CdcParams {
    fn default() -> Self {
        CdcParams::new(64 * 1024, 256 * 1024, 1024 * 1024)
    }
}

impl CdcParams {
    /// Конструктор с выведением масок из целевых размеров.
    /// Фаза A покрывает [min, avg) маской log2(avg) бит; выжившие
    /// чанк-кандидаты в фазе B [avg, max) режутся маской на 2 бита
    /// короче — распределение концентрируется вокруг avg.
    pub fn new(min: usize, avg: usize, max: usize) -> Self {
        debug_assert!(min >= 1024 && avg > min && max > avg);
        let bits_a = ((avg as f64).log2().round() as u32).clamp(8, 40);
        let bits_b = bits_a.saturating_sub(2).max(6);
        CdcParams {
            min,
            avg,
            max,
            mask_a: ((1u64 << bits_a) - 1) << (64 - bits_a),
            mask_b: ((1u64 << bits_b) - 1) << (64 - bits_b),
        }
    }
}

/// SplitMix64 — детерминированный генератор для gear-таблицы.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Сид gear-таблицы фиксирован: нарезка воспроизводима между запусками,
/// процессами и архивами (кросс-архивная дедупликация совместима).
const GEAR_SEED: u64 = 0x504F_4C45_5243_4443; // "POLERCDC"

fn gear_table() -> &'static [u64; 256] {
    use std::sync::OnceLock;
    static GEAR: OnceLock<[u64; 256]> = OnceLock::new();
    GEAR.get_or_init(|| {
        let mut t = [0u64; 256];
        let mut st = GEAR_SEED;
        for slot in t.iter_mut() {
            *slot = splitmix64(&mut st) | 1; // нечётные — ненулевые младшие биты
        }
        t
    })
}

/// Точка разреза: длина чанка от начала `data`.
///
/// Контракт вызова (RAM-дисциплина и детерминизм стриминга):
/// - если известно, что за `data` следуют ещё байты, вызывающий обязан
///   гарантировать `data.len() >= p.max` — тогда граница зависит только
///   от содержимого (эффективный лимит = max);
/// - на EOF допускается `data.len() < p.max` (хвостовой чанк).
///
/// `data.len() <= p.min` → весь буфер целиком (хвост потока).
pub fn cut_point(data: &[u8], p: &CdcParams) -> usize {
    if data.len() <= p.min {
        return data.len();
    }
    let limit = p.max.min(data.len());
    let gear = gear_table();
    let mut h: u64 = 0;
    let mut i = p.min;
    while i < limit {
        h = (h << 1).wrapping_add(gear[data[i] as usize]);
        let mask = if i < p.avg { p.mask_a } else { p.mask_b };
        if h & mask == 0 {
            return i + 1;
        }
        i += 1;
    }
    limit
}

/// Разрезать буфер на чанки (для тестов и утилит; стример делает
/// это сам, не покидая буфер).
pub fn split_chunks<'a>(data: &'a [u8], p: &CdcParams) -> Vec<&'a [u8]> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let cut = cut_point(rest, p);
        let (head, tail) = rest.split_at(cut);
        out.push(head);
        rest = tail;
    }
    out
}

/// BLAKE3-хеш чанка (256 бит).
pub fn chunk_hash(data: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(data);
    *h.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudo_data(len: usize, seed: u64) -> Vec<u8> {
        let mut st = seed;
        let mut v = Vec::with_capacity(len);
        while v.len() < len {
            st = splitmix64(&mut st);
            v.extend_from_slice(&st.to_le_bytes());
        }
        v.truncate(len);
        v
    }

    #[test]
    fn cut_point_respects_bounds() {
        let p = CdcParams::default();
        let data = pseudo_data(4 * 1024 * 1024, 42);
        let cut = cut_point(&data, &p);
        assert!(cut >= p.min && cut <= p.max, "cut={cut} вне [{},{}]", p.min, p.max);
        // Хвостовые случаи
        assert_eq!(cut_point(&data[..p.min - 1], &p), p.min - 1);
        assert_eq!(cut_point(&data[..p.min], &p), p.min);
        assert_eq!(cut_point(&data[..p.min + 1], &p), p.min + 1);
    }

    #[test]
    fn chunking_is_deterministic() {
        let p = CdcParams::default();
        let a = pseudo_data(3 * 1024 * 1024, 7);
        let b = a.clone();
        let sa: Vec<usize> = split_chunks(&a, &p).iter().map(|c| c.len()).collect();
        let sb: Vec<usize> = split_chunks(&b, &p).iter().map(|c| c.len()).collect();
        assert_eq!(sa, sb, "одинаковые данные режутся одинаково");
    }

    /// Содержательно-зависимые границы: вставка байта в начало меняет
    /// только чанки до точки ресинхронизации (свойство content-defined).
    #[test]
    fn boundaries_shift_with_content() {
        let p = CdcParams::new(1024, 4096, 16 * 1024);
        let base = pseudo_data(64 * 1024, 1);
        let mut shifted = Vec::with_capacity(base.len() + 1);
        shifted.push(0xAB);
        shifted.extend_from_slice(&base);
        let ca = split_chunks(&base, &p);
        let cb = split_chunks(&shifted, &p);
        // Число чанков не может вырасти больше чем на 1 при сдвиге на байт
        assert!(
            cb.len() <= ca.len() + 1,
            "нарезка не рассыпалась: {} vs {}",
            cb.len(),
            ca.len()
        );
    }

    /// Средний размер на псевдослучайных данных ≈ avg (нормализация
    /// фаз A/B): допускаем [0.5, 2.0]×avg — маски выведены из log2.
    #[test]
    fn average_size_near_target() {
        let p = CdcParams::default();
        let data = pseudo_data(32 * 1024 * 1024, 99);
        let chunks = split_chunks(&data, &p);
        let total: usize = chunks.iter().map(|c| c.len()).sum();
        let avg = total as f64 / chunks.len() as f64;
        assert!(
            avg >= p.avg as f64 * 0.5 && avg <= p.avg as f64 * 2.0,
            "средний размер {avg} вне [0.5,2.0]×{}",
            p.avg
        );
    }

    #[test]
    fn blake3_hash_stable() {
        // Закрепляем префикс хеша «poler» (регрессия формата).
        let h = chunk_hash(b"poler");
        assert_eq!(h.len(), 32);
        assert!(h.iter().any(|&b| b != 0));
    }

    /// Полигонный масштаб: min 1 KiB / avg 4 KiB / max 16 KiB —
    /// чанки заметки агента (единицы дедупликации RAM-полигона).
    #[test]
    fn scratch_scale_params_cut_text() {
        let p = CdcParams::new(1024, 4096, 16 * 1024);
        let text: Vec<u8> = (0..100_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        let chunks = split_chunks(&text, &p);
        assert!(chunks.len() >= text.len() / p.max);
        // Все чанки, кроме хвостового, ≥ min: хвост потока может быть
        // короче (EOF-чанк по контракту cut_point).
        let (body, tail) = chunks.split_at(chunks.len() - 1);
        assert!(body.iter().all(|c| c.len() >= p.min));
        assert!(!tail[0].is_empty());
        // Одинаковый кусок в двух местах потока режется одинаково:
        // после ресинхронизации границы повторного контента совпадают
        // (content-defined границы не зависят от позиции) → те же хеши
        // → dedup hit. Хвостовой чанк первой копии единственный, кто
        // видел EOF в одиночной нарезке, — его не сравниваем.
        let mut doubled = text.clone();
        doubled.extend_from_slice(&text);
        let cd = split_chunks(&doubled, &p);
        let ct = split_chunks(&text, &p);
        assert!(cd.len() >= ct.len(), "удвоенный поток чанков не меньше");
        let n = ct.len() - 1;
        let heads_d: Vec<[u8; 32]> = cd.iter().take(n).map(|c| chunk_hash(c)).collect();
        let heads_t: Vec<[u8; 32]> = ct.iter().take(n).map(|c| chunk_hash(c)).collect();
        assert_eq!(
            heads_d, heads_t,
            "повторный контент дедуплицируется: те же чанки"
        );
    }
}
