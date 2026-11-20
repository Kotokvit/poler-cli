//! Кристалл Знаний — троично-квантованная языковая модель (.t5c).
//!
//! ## Фаза 5 «MCP-глаза на VFS» — вырезано из poler-engine, не переписано
//!
//! Модуль вырезан из вендоренного движка
//! `poler-engine/src/triune/crystal.rs` (poler-engine v0.93.0 @ 5987a86)
//! без алгоритмических правок и собран в один самодостаточный файл
//! (единственный импорт — `std::collections`): вместе с кристаллом
//! перенесены его четыре зависимости — `fnv1a64` (literary/qualia.rs),
//! SHA-256 (pqc/sha256.rs, FIPS 180-4), кодек Trit5 (pqc/tensor.rs) и
//! CSE-энкодер золотой фазы (ssn/cse.rs). Отличие от движка ровно одно:
//! `dot_trit5_f32` идёт скалярной веткой (в движке — AVX2-диспетчер);
//! сама No-Mul-схема (умножение заменено знаковым сложением/вычитанием)
//! сохранена. Зашитый кристалл `crystal_ru.t5c` скопирован из движка
//! побитово.
//!
//! ## Роль в Фазе 5
//!
//! Кристалл — ассоциативная RAM-память агента в полигоне `vfs://ai_scratch/`
//! (см. `scratch.rs`): заметки агента прошиваются в биграммную топологию
//! Хеббовскими сдвигами ([`Crystal::bigram_bump`]), новые понятия получают
//! id за O(1) через [`Crystal::expand_vocab`] (Dynamic Vocab Expansion),
//! а резонансный запрос «что с этим связано» — [`Crystal::assoc_neighbors`]
//! (семантика CSE + совместная встречаемость). Файл `.t5c` (20 КБ – 2 МБ)
//! сохраняется/монтируется одним вызовом — сессия агента переживает
//! перезапуск IDE.
//!
//! ## Что внутри кристалла (докамвайг движка, сохранён дословно)
//!
//! 1. **Словарь** — топ слов корпуса (детерминированный порядок:
//!    частота ↓, лексика ↑ — никакого HashMap-порядка в артефакте).
//! 2. **Биграммная топология** — знак отношения правдоподобия:
//!    `r = P(next|prev)/P(next)`; трит = +1 при `r ≥ θ_hi`, −1 при
//!    `r ≤ θ_lo` (и пара встречалась), 0 — нейтраль/вакуум. Это
//!    знаковая квантизация PMI: синтаксис живёт в знаках, не в величинах.
//! 3. **Семантика НЕ хранится** — вложение токена выводится на лету из
//!    золотой фазы CSE (F6: только `c·φ mod 2π` даёт зазор 1.35) и
//!    квантуется в триты. Сенсорика и память делят один код — как
//!    биологический мозг использует одну кортикальную карту для
//!    восприятия и воспоминаний.
//!
//! ## Формат .t5c (Trit5 Crystal v1, little-endian)
//!
//! ```text
//! СМЕЩЕНИЕ  РАЗМЕР  ПОЛЕ
//! 0x00      8       magic "T5CRYS\0\0"
//! 0x08      4       version u32 = 1
//! 0x0C      4       vocab V
//! 0x10      4       flags u32 (bit0 = есть биграммная секция)
//! 0x14      4       token_off u32 (абс.)
//! 0x18      4       bigram_off u32 (абс.)
//! 0x1C      8       corpus_words u64
//! 0x24      8       corpus_chars u64
//! 0x2C      4       theta_hi_milli u32 (θ_hi × 1000)
//! 0x30      32      sha256(тело: 0x50..EOF)
//! 0x50      ·       таблица токенов: V × (u8 len | utf8)
//! …         ·       биграммы: V строк × ceil(V/5) байт Trit5
//! ```
//!
//! Сборка полностью детерминирована (никаких таймстампов, никакого
//! состояния ОС): тот же корпус → побитово тот же файл.

use std::collections::HashMap;
use std::path::Path;

// ─────────────────────────────────────────────────────────────────────────────
// Зависимость 1/4: fnv1a64 (poler-engine/src/literary/qualia.rs)
// ─────────────────────────────────────────────────────────────────────────────

/// FNV-1a 64: якорь архетипа токена (mod 12) и ось фазового пространства.
#[inline]
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ─────────────────────────────────────────────────────────────────────────────
// Зависимость 2/4: SHA-256 (poler-engine/src/pqc/sha256.rs, FIPS 180-4)
// ─────────────────────────────────────────────────────────────────────────────

/// Константы раундов SHA-256 (FIPS 180-4).
const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1,
    0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
    0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
    0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147,
    0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
    0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
    0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
    0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Инкрементальный SHA-256 (FIPS 180-4). Суверенный стек: хэш — часть
/// формата, а не чужая крейта.
pub struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total_bits: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    /// Новое хэширование (IV по спецификации).
    pub fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f,
                0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
            ],
            buf: [0u8; 64],
            buf_len: 0,
            total_bits: 0,
        }
    }

    /// Поглощает байты (потоково, без копирования лишних кусков).
    pub fn update(&mut self, mut data: &[u8]) {
        self.total_bits = self.total_bits.wrapping_add((data.len() as u64) * 8);
        if self.buf_len > 0 {
            let need = 64 - self.buf_len;
            let take = need.min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let (block, rest) = data.split_at(64);
            let mut b = [0u8; 64];
            b.copy_from_slice(block);
            self.compress(&b);
            data = rest;
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    /// Финализирует и возвращает дайджест (big-endian слова).
    pub fn finalize(mut self) -> [u8; 32] {
        let pad_len = {
            let rem = (self.total_bits / 8) % 64;
            if rem < 56 {
                56 - rem
            } else {
                120 - rem
            }
        };
        let mut tail = Vec::with_capacity(pad_len as usize + 8);
        tail.push(0x80);
        tail.extend(std::iter::repeat(0u8).take(pad_len as usize - 1));
        tail.extend_from_slice(&self.total_bits.to_be_bytes());
        self.update(&tail);
        debug_assert_eq!(self.buf_len, 0);
        let mut out = [0u8; 32];
        for (i, w) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }
}

/// Одноразовое хэширование среза байтов (тело .t5c).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize()
}

/// Hex-представление дайджеста (для отчётов).
pub fn hex(digest: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ─────────────────────────────────────────────────────────────────────────────
// Зависимость 3/4: Trit5-кодек (poler-engine/src/pqc/tensor.rs)
// ─────────────────────────────────────────────────────────────────────────────

/// Кодек «пять тритов в байт» (EQ-D17: 3⁵ = 243 ≤ 256 — 1.58 бита/синапс).
pub struct Trit5Codec;

impl Trit5Codec {
    /// Упаковывает 5 тритов t_i ∈ {−1, 0, +1} в 1 байт.
    /// Возвращает None, если хотя бы один трит выходит за пределы {−1, 0, +1}.
    #[inline(always)]
    pub fn pack_5(trits: &[i8; 5]) -> Option<u8> {
        let mut byte_val: u16 = 0;
        let mut mul: u16 = 1;

        for &t in trits.iter() {
            if !(-1..=1).contains(&t) {
                return None;
            }
            let u = (t + 1) as u16; // Map {-1, 0, +1} -> {0, 1, 2}
            byte_val += u * mul;
            mul *= 3;
        }

        Some(byte_val as u8)
    }

    /// Распаковывает байт B ∈ [0, 242] в 5 тритов t_i ∈ {−1, 0, +1}.
    /// Байты ≥ 243 (хвост упаковки в движке) — вакуум.
    #[inline(always)]
    pub fn unpack_5(byte: u8) -> [i8; 5] {
        if byte >= 243 {
            return [0; 5];
        }
        let mut v = byte as u32;
        let mut out = [0i8; 5];
        for slot in out.iter_mut() {
            let u = (v % 3) as i8;
            *slot = u - 1;
            v /= 3;
        }
        out
    }
}

/// Скалярное произведение Trit5-строки весов с fp32-вектором активаций.
///
/// No-Mul исполнение (скалярная ветка движка): веса в {−1, 0, +1},
/// умножение заменено условным знаковым сложением/вычитанием.
pub fn dot_trit5_f32(packed: &[u8], x: &[f32], n: usize) -> f32 {
    debug_assert_eq!(packed.len(), n.div_ceil(5), "dot_trit5_f32: размер упаковки");
    let mut s = 0f32;
    let mut x_idx = 0;
    for &b in packed {
        if b >= 243 {
            x_idx = (x_idx + 5).min(n);
            continue;
        }
        for t in Trit5Codec::unpack_5(b) {
            if x_idx >= n {
                break;
            }
            let xi = x[x_idx];
            if t == 1 {
                s += xi;
            } else if t == -1 {
                s -= xi;
            }
            x_idx += 1;
        }
    }
    s
}

// ─────────────────────────────────────────────────────────────────────────────
// Зависимость 4/4: CSE — Contextual Semantic Encoder (ssn/cse.rs)
// ─────────────────────────────────────────────────────────────────────────────

/// Золотое сечение (минус 1): 1/φ = φ − 1 = 0.618…
pub const PHI: f64 = 0.618_033_988_749_894_9;

/// Число бит-частот на символ.
pub const CSE_BITS: usize = 16;

/// Кодировать текст в D-мерный единичный вектор через золотую фазу.
///
/// Доказано (V8, proofs/ssn_verify3.py): посимвольная фаза, выведенная из
/// кода символа через золотое сечение φ, даёт колоссальную разделимость:
/// для канонического триплета cos(a,b) = 0.999 против cos(a,c) = −0.355 —
/// зазор 1.353 (без золотой фазы: 0.02). Ноль ГПСЧ: один текст → один
/// вектор, побитово воспроизводимо.
pub fn cse_encode(text: &str, d: usize) -> Vec<f64> {
    let mut vec = vec![0.0f64; d];
    if d == 0 {
        return vec;
    }
    let tau = 2.0 * std::f64::consts::PI;
    for (i, ch) in text.chars().enumerate() {
        let code = ch as u32;
        let wp = 1.0 / (1.0 + i as f64);
        let cphase = (code as f64 * PHI) % tau;
        let nbits = CSE_BITS.min(d);
        for j in 0..nbits {
            let bit = (code >> j) & 1;
            let freq = 1.0 + j as f64 * 0.3;
            let phase = i as f64 * 0.01 + cphase;
            if bit == 1 {
                for (k, v) in vec.iter_mut().enumerate() {
                    *v += (freq * k as f64 + phase).sin() * wp;
                }
            } else {
                for (k, v) in vec.iter_mut().enumerate() {
                    *v += (freq * k as f64 + phase).cos() * 0.5 * wp;
                }
            }
        }
    }
    // центрирование + нормировка
    let mean = vec.iter().sum::<f64>() / d as f64;
    for v in vec.iter_mut() {
        *v -= mean;
    }
    let n = cse_norm(&vec);
    if n > 0.0 {
        for v in vec.iter_mut() {
            *v /= n;
        }
    }
    vec
}

/// Евклидова норма.
fn cse_norm(v: &[f64]) -> f64 {
    v.iter().map(|x| x * x).sum::<f64>().sqrt()
}

// ─────────────────────────────────────────────────────────────────────────────
// Троичное состояние (poler-engine/src/literary/trit.rs)
// ─────────────────────────────────────────────────────────────────────────────

/// Троичное латентное состояние: упакованные триты + масштаб.
///
/// Схема per-vector: масштаб s = max|p_i|; трит t_i = sign(p_i), если
/// |p_i| ≥ θ·s (мёртвая зона отсекает шум), иначе 0. Восстановление
/// p̂_i = t_i·s — детерминированное.
#[derive(Clone, Debug)]
pub struct TritState {
    packed: Vec<u8>,
    /// Масштаб s = max|p_i| на момент квантования.
    pub scale: f32,
    /// Число осей.
    pub dims: usize,
    /// Число ненулевых тритов (плотность состояния).
    pub nonzeros: usize,
}

/// Мёртвая зона по умолчанию: 5% масштаба.
pub const DEFAULT_THETA: f32 = 0.05;

/// Квантование p → TritState. Пустой/нулевой вектор — легальное
/// состояние (все триты 0).
pub fn quantize(p: &[f32], theta: f32) -> TritState {
    let dims = p.len();
    let scale = p.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
    let dead = theta.max(0.0) * scale;
    let mut trits = vec![0i8; dims];
    let mut nonzeros = 0usize;
    for (i, &v) in p.iter().enumerate() {
        if v.abs() > dead && v.abs() >= scale * 1e-6 {
            trits[i] = if v > 0.0 { 1 } else { -1 };
            nonzeros += 1;
        }
    }
    // Упаковка по 5 тритов в байт.
    let packed_cols = dims.div_ceil(5);
    let mut packed = vec![0u8; packed_cols];
    for (chunk_idx, chunk) in trits.chunks(5).enumerate() {
        let mut five = [0i8; 5];
        five[..chunk.len()].copy_from_slice(chunk);
        if let Some(b) = Trit5Codec::pack_5(&five) {
            packed[chunk_idx] = b;
        }
    }
    TritState {
        packed,
        scale,
        dims,
        nonzeros,
    }
}

impl TritState {
    /// Восстановление f32-вектора: p̂_i = t_i·s.
    pub fn dequantize(&self) -> Vec<f32> {
        let mut out = vec![0.0f32; self.dims];
        let mut i = 0usize;
        for &b in &self.packed {
            for t in Trit5Codec::unpack_5(b) {
                if i >= self.dims {
                    break;
                }
                out[i] = t as f32 * self.scale;
                i += 1;
            }
        }
        out
    }

    /// No-Mul-скалярное произведение с f32-вектором (без f32-математики
    /// на трит-стороне).
    pub fn dot(&self, x: &[f32]) -> f32 {
        debug_assert_eq!(x.len(), self.dims);
        dot_trit5_f32(&self.packed, x, self.dims) * self.scale
    }

    /// Плотность состояния: доля ненулевых тритов.
    pub fn density(&self) -> f32 {
        if self.dims == 0 {
            return 0.0;
        }
        self.nonzeros as f32 / self.dims as f32
    }

    /// Упакованные байты (сырой вид для сериализации).
    pub fn packed_bytes(&self) -> &[u8] {
        &self.packed
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Кристалл Знаний (poler-engine/src/triune/crystal.rs)
// ─────────────────────────────────────────────────────────────────────────────

/// Магия формата.
pub const MAGIC: [u8; 8] = *b"T5CRYS\0\0";
/// Версия формата.
pub const VERSION: u32 = 1;
/// Размер заголовка (sha256 заканчивается ровно на 0x50).
pub const HEADER: usize = 0x50;
/// Число архетипов поля (якорь токена = FNV-хэш mod 12, как у касты мухи).
pub const ARCHETYPES: usize = 12;
/// Размерность CSE-вложений по умолчанию.
pub const DEFAULT_DIMS: usize = 96;
/// Словарь по умолчанию.
pub const DEFAULT_VOCAB: usize = 384;
/// Порог знаковой квантизации PMI: притяжение.
pub const DEFAULT_THETA_HI: f64 = 1.7;
/// Порог знаковой квантизации PMI: отталкивание.
pub const DEFAULT_THETA_LO: f64 = 0.5;
/// Зашитый в бинарник кристалл (побитовая копия
/// poler-engine/src/triune/crystal_ru.t5c, собран из corpus_ru.txt).
pub const EMBEDDED_CRYSTAL: &[u8] = include_bytes!("crystal_ru.t5c");

/// Токенизация корпуса/речи: lowercase, слова = цепочки букв/цифр.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                cur.push(lc);
            }
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Разбивка на предложения (граница: . ! ? ; : и перевод строки).
fn sentences(text: &str) -> Vec<&str> {
    text.split(['.', '!', '?', ';', ':', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Кристалл Знаний: словарь + троичная биграммная топология +
/// производные CSE-вложения.
#[derive(Clone)]
pub struct Crystal {
    /// Токены словаря в каноническом порядке (порядок = id).
    pub tokens: Vec<String>,
    /// Обратный индекс: токен → id.
    index: HashMap<String, u32>,
    /// Биграммы, Trit5-упакованные: строка prev, stride байт на строку.
    /// stride ≥ ceil(V/5): запас под динамическое расширение словаря
    /// (expand_vocab) без перезапаковки всей матрицы на каждое слово.
    bigram: Vec<u8>,
    /// Выделено байт на строку биграмм (может быть больше ceil(V/5)).
    stride: usize,
    /// Статистика корпуса.
    pub corpus_words: u64,
    pub corpus_chars: u64,
    /// Число ненулевых биграмм (плотность топологии).
    pub bigram_nonzeros: u64,
    /// Пороги квантизации (милли-доли).
    theta_hi_milli: u32,
    /// Производные вложения: триты (No-Mul) + f64 (NMDA-гейт).
    embeds_trit: Vec<TritState>,
    embeds_f64: Vec<Vec<f64>>,
    /// Якоря архетипов.
    anchors: Vec<u8>,
    /// Размерность CSE-вложений.
    pub dims: usize,
}

impl Crystal {
    /// Сборка кристалла из корпуса. Полностью детерминирована.
    pub fn build(
        corpus: &str,
        vocab: usize,
        dims: usize,
        theta_hi: f64,
        theta_lo: f64,
    ) -> Result<Crystal, String> {
        if !(0.5..=8.0).contains(&theta_hi) || !(0.05..=0.95).contains(&theta_lo) {
            return Err(format!("пороги вне диапазона: θ_hi={theta_hi}, θ_lo={theta_lo}"));
        }
        if theta_hi <= theta_lo {
            return Err("θ_hi должен быть больше θ_lo".into());
        }
        let dims = dims.clamp(16, 512);

        // ── Частоты слов (канонический порядок: частота ↓, слово ↑) ──
        let words = tokenize(corpus);
        if words.len() < 16 {
            return Err(format!("корпус слишком мал: {} слов (нужно ≥ 16)", words.len()));
        }
        let mut counts: HashMap<&str, u64> = HashMap::new();
        for w in &words {
            *counts.entry(w.as_str()).or_insert(0) += 1;
        }
        let mut ranked: Vec<(&str, u64)> = counts.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        ranked.retain(|(w, _)| w.len() <= u8::MAX as usize); // u8-длина в .t5c
        // clamp(32, n) паникует при n < 32 — короткий корпус получает
        // свой фактический словарь (паритет с потоковым ingest::finalize)
        let vocab = if ranked.len() < 32 {
            ranked.len()
        } else {
            vocab.clamp(32, ranked.len())
        };
        let tokens: Vec<String> = ranked[..vocab].iter().map(|(w, _)| w.to_string()).collect();
        let index: HashMap<String, u32> =
            tokens.iter().enumerate().map(|(i, t)| (t.clone(), i as u32)).collect();

        // ── Поток в пределах словаря (предложения не склеиваются) ──
        let mut stream_ids: Vec<Vec<u32>> = Vec::new();
        let mut total: u64 = 0;
        for sent in sentences(corpus) {
            let ids: Vec<u32> = tokenize(sent)
                .into_iter()
                .filter_map(|w| index.get(&w).copied())
                .collect();
            total += ids.len() as u64;
            if !ids.is_empty() {
                stream_ids.push(ids);
            }
        }
        if total < 8 {
            return Err("после фильтра словаря поток пуст".into());
        }

        // ── Биграммные счётчики ──
        let mut uni = vec![0u64; vocab];
        let mut bi: HashMap<(u32, u32), u64> = HashMap::new();
        for ids in &stream_ids {
            for &w in ids {
                uni[w as usize] += 1;
            }
            for pair in ids.windows(2) {
                *bi.entry((pair[0], pair[1])).or_insert(0) += 1;
            }
        }
        let cols_packed = vocab.div_ceil(5);
        // ⚠ Байт 0x00 в Trit5 = пять тритов −1 (цифра 0 = трит −1).
        // Вакуум кодируется байтом 121 = pack_5([0,0,0,0,0]).
        let zero_byte = Trit5Codec::pack_5(&[0i8; 5]).unwrap_or(121);
        let mut bigram = vec![zero_byte; vocab * cols_packed];
        let mut bigram_nonzeros: u64 = 0;
        // Детерминизм: обход отсортированных ключей, не HashMap.
        let mut pairs: Vec<((u32, u32), u64)> =
            bi.iter().map(|(k, &v)| (*k, v)).collect();
        pairs.sort_by_key(|&(p, n)| (p, n));
        for ((prev, next), c) in pairs {
            let (prev, next) = (prev as usize, next as usize);
            let p_next = uni[next] as f64 / total as f64;
            let p_cond = c as f64 / uni[prev].max(1) as f64;
            let r = p_cond / p_next.max(1e-12);
            let trit: i8 = if r >= theta_hi {
                1
            } else if r <= theta_lo {
                -1
            } else {
                0
            };
            if trit != 0 {
                bigram_nonzeros += 1;
                let byte = &mut bigram[prev * cols_packed + next / 5];
                let mut five = Trit5Codec::unpack_5(*byte);
                five[next % 5] = trit;
                if let Some(b) = Trit5Codec::pack_5(&five) {
                    *byte = b;
                }
            } else {
                // Явная запись вакуума (иначе останется корректный 121 —
                // но перезапись гарантирует согласованность при смене кодека).
                let byte = &mut bigram[prev * cols_packed + next / 5];
                let mut five = Trit5Codec::unpack_5(*byte);
                five[next % 5] = 0;
                if let Some(b) = Trit5Codec::pack_5(&five) {
                    *byte = b;
                }
            }
        }

        let mut crystal = Crystal {
            tokens,
            index,
            bigram,
            stride: cols_packed,
            corpus_words: words.len() as u64,
            corpus_chars: corpus.chars().count() as u64,
            bigram_nonzeros,
            theta_hi_milli: (theta_hi * 1000.0).round() as u32,
            embeds_trit: Vec::new(),
            embeds_f64: Vec::new(),
            anchors: Vec::new(),
            dims,
        };
        crystal.derive_embeddings();
        Ok(crystal)
    }

    /// Производные данные: CSE-вложения → триты + f64, якоря архетипов.
    fn derive_embeddings(&mut self) {
        let dims = self.dims;
        let mut trits = Vec::with_capacity(self.tokens.len());
        let mut full = Vec::with_capacity(self.tokens.len());
        let mut anchors = Vec::with_capacity(self.tokens.len());
        for t in &self.tokens {
            let v = cse_encode(t, dims);
            let v32: Vec<f32> = v.iter().map(|&x| x as f32).collect();
            trits.push(quantize(&v32, 0.05));
            let q = trits.last().unwrap().dequantize();
            full.push(q.iter().map(|&x| x as f64).collect());
            anchors.push((fnv1a64(t.as_bytes()) % ARCHETYPES as u64) as u8);
        }
        self.embeds_trit = trits;
        self.embeds_f64 = full;
        self.anchors = anchors;
    }

    /// Трит биграммы (prev → next): −1/0/+1. No-Mul: прямой lookup.
    pub fn bigram_trit(&self, prev: u32, next: u32) -> i8 {
        let v = self.tokens.len();
        if (prev as usize) >= v || (next as usize) >= v {
            return 0;
        }
        let byte = self.bigram[prev as usize * self.stride + next as usize / 5];
        Trit5Codec::unpack_5(byte)[next as usize % 5]
    }

    /// Строка синапсов нейрона `prev`: Trit5-упакованные триты
    /// `prev → *` (stride байт; столбцы ≥ V — вакуум). Горячий путь
    /// нейронной популяции: скан без распаковки всей матрицы.
    pub fn bigram_row(&self, prev: u32) -> &[u8] {
        let from = prev as usize * self.stride;
        &self.bigram[from..from + self.stride]
    }

    /// Шаг упаковки (байт на строку синапсов).
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// Хеббовский сдвиг синапса (v0.38.0): трит `prev → next` ползёт к
    /// ±1, насыщаясь на краях решётки {−1, 0, +1}. Возврат — новый трит.
    ///
    /// Это и есть пластичность «вес зашит в синапс»: пара (prev, next)
    /// совпала (NMDA-гейт открыт, энергия удивления высока) — синапс
    /// укрепляется на +1; промах — ослабляется на −1. Один проход, без
    /// эпох градиентного спуска — потому данных нужно меньше.
    pub fn bigram_bump(&mut self, prev: u32, next: u32, delta: i8) -> i8 {
        let v = self.tokens.len();
        if (prev as usize) >= v || (next as usize) >= v || delta == 0 {
            return self.bigram_trit(prev, next);
        }
        let byte_idx = prev as usize * self.stride + next as usize / 5;
        let mut five = Trit5Codec::unpack_5(self.bigram[byte_idx]);
        let cur = five[next as usize % 5];
        let new = (cur + delta).clamp(-1, 1);
        if new != cur {
            if cur != 0 {
                self.bigram_nonzeros -= 1;
            }
            if new != 0 {
                self.bigram_nonzeros += 1;
            }
        }
        five[next as usize % 5] = new;
        if let Some(b) = Trit5Codec::pack_5(&five) {
            self.bigram[byte_idx] = b;
        }
        new
    }

    /// Троичное вложение токена (No-Mul-сторона).
    pub fn embed_trit(&self, idx: u32) -> &TritState {
        &self.embeds_trit[idx as usize]
    }

    /// Вложение токена в f64 (сторона NMDA-гейта).
    pub fn embed_f64(&self, idx: u32) -> &[f64] {
        &self.embeds_f64[idx as usize]
    }

    /// Якорь архетипа токена (0..12).
    pub fn anchor(&self, idx: u32) -> usize {
        self.anchors[idx as usize] as usize
    }

    /// Топ-k семантических соседей токена — ассоциативные
    /// (симметричные) синапсы популяции: взаимная поддержка
    /// тематического кластера. Ранжирование по трит-скалярному
    /// произведению CSE-вложений (No-Mul).
    pub fn semantic_neighbors(&self, idx: u32, k: usize) -> Vec<u32> {
        let v = self.tokens.len();
        if idx as usize >= v || k == 0 {
            return Vec::new();
        }
        let probe = self.embeds_trit[idx as usize].dequantize();
        let mut sims: Vec<(u32, f32)> = (0..v as u32)
            .filter(|&j| j != idx)
            .map(|j| (j, self.embeds_trit[j as usize].dot(&probe)))
            .collect();
        sims.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        sims.into_iter().take(k).map(|(j, _)| j).collect()
    }

    /// Ассоциативные соседи нейрона — симметричное Хеббовское
    /// замыкание синапсов для резонансного бассейна популяции:
    ///
    /// - семантические соседи (топ `sem_k` по трит-скалярному
    ///   произведению CSE — морфология/форма слова);
    /// - партнёры по совместной встречаемости: `bigram(i→j) = +1`
    ///   ИЛИ `bigram(j→i) = +1` («срабатывали вместе — связаны»),
    ///   до `bi_k` в каждую сторону.
    ///
    /// Направленность синтаксиса живёт в волне (step популяции);
    /// здесь — только взаимная поддержка темы.
    pub fn assoc_neighbors(&self, idx: u32, sem_k: usize, bi_k: usize) -> Vec<u32> {
        let v = self.tokens.len();
        if idx as usize >= v {
            return Vec::new();
        }
        let mut out: Vec<u32> = Vec::with_capacity(sem_k + 2 * bi_k);
        let push_unique = |j: u32, out: &mut Vec<u32>| {
            if j != idx && !out.contains(&j) {
                out.push(j);
            }
        };
        // 1. Семантика.
        for j in self.semantic_neighbors(idx, sem_k) {
            push_unique(j, &mut out);
        }
        // 2. Прямые партнёры (+1 в строке).
        let row = self.bigram_row(idx);
        let mut found = 0usize;
        for (byte_idx, &byte) in row.iter().enumerate() {
            if byte == 121 {
                continue; // вакуум
            }
            let five = Trit5Codec::unpack_5(byte);
            for (slot, &w) in five.iter().enumerate() {
                if w != 1 {
                    continue;
                }
                let j = byte_idx * 5 + slot;
                if j >= v {
                    break;
                }
                push_unique(j as u32, &mut out);
                found += 1;
                if found >= bi_k {
                    break;
                }
            }
            if found >= bi_k {
                break;
            }
        }
        // 3. Обратные партнёры (+1 в столбце): встречались рядом —
        //    связь симметрична для памяти, какой бы ни была стрелка.
        found = 0;
        for j in 0..v as u32 {
            if self.bigram_trit(j, idx) == 1 {
                push_unique(j, &mut out);
                found += 1;
                if found >= bi_k {
                    break;
                }
            }
        }
        out
    }

    /// id токена или None (вне словаря).
    pub fn id_of(&self, token: &str) -> Option<u32> {
        self.index.get(token).copied()
    }

    /// Размер словаря.
    pub fn vocab(&self) -> usize {
        self.tokens.len()
    }

    /// Загрузка из байтов .t5c (dims — размерность CSE-вложений).
    pub fn load(bytes: &[u8], dims: usize) -> Result<Crystal, String> {
        if bytes.len() < HEADER {
            return Err("файл меньше заголовка".into());
        }
        if bytes[..8] != MAGIC {
            return Err("не .t5c: чужая магия".into());
        }
        let rd_u32 = |off: usize| -> u32 {
            u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap())
        };
        let rd_u64 = |off: usize| -> u64 {
            let lo = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as u64;
            let hi = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as u64;
            lo | (hi << 32)
        };
        if rd_u32(0x08) != VERSION {
            return Err(format!("версия {} не поддерживается", rd_u32(0x08)));
        }
        // sha256 тела.
        let digest = sha256(&bytes[HEADER..]);
        if digest[..] != bytes[0x30..0x50] {
            return Err("sha256 тела не сходится: кристалл повреждён".into());
        }
        let vocab = rd_u32(0x0C) as usize;
        let flags = rd_u32(0x10);
        let token_off = rd_u32(0x14) as usize;
        let bigram_off = rd_u32(0x18) as usize;
        let corpus_words = rd_u64(0x1C);
        let corpus_chars = rd_u64(0x24);
        let theta_hi_milli = rd_u32(0x2C);
        if vocab == 0 || vocab > 1_000_000 {
            return Err(format!("странный словарь: {vocab}"));
        }
        // Таблица токенов.
        let mut pos = token_off;
        let mut tokens = Vec::with_capacity(vocab);
        for _ in 0..vocab {
            if pos >= bytes.len() {
                return Err("таблица токенов оборвана".into());
            }
            let len = bytes[pos] as usize;
            pos += 1;
            if pos + len > bytes.len() {
                return Err("токен оборван".into());
            }
            tokens.push(String::from_utf8(bytes[pos..pos + len].to_vec()).map_err(|e| e.to_string())?);
            pos += len;
        }
        if pos != bigram_off && (flags & 1) == 1 {
            return Err("bigram_off не совпадает с концом таблицы токенов".into());
        }
        let cols = vocab.div_ceil(5);
        let bigram = if flags & 1 == 1 {
            let need = vocab * cols;
            if bigram_off + need != bytes.len() {
                return Err("биграммная секция не до конца файла".into());
            }
            bytes[bigram_off..bigram_off + need].to_vec()
        } else {
            if bigram_off != bytes.len() {
                return Err("лишние байты после таблицы токенов".into());
            }
            Vec::new()
        };
        let index: HashMap<String, u32> =
            tokens.iter().enumerate().map(|(i, t)| (t.clone(), i as u32)).collect();
        let bigram_nonzeros = bigram
            .chunks_exact(cols)
            .flat_map(|row| row.iter())
            .map(|&b| Trit5Codec::unpack_5(b))
            .map(|five| five.iter().filter(|&&t| t != 0).count() as u64)
            .sum();
        let mut crystal = Crystal {
            tokens,
            index,
            bigram,
            stride: cols,
            corpus_words,
            corpus_chars,
            bigram_nonzeros,
            theta_hi_milli,
            embeds_trit: Vec::new(),
            embeds_f64: Vec::new(),
            anchors: Vec::new(),
            dims: dims.clamp(16, 512),
        };
        crystal.derive_embeddings();
        Ok(crystal)
    }

    /// Сериализация в байты .t5c.
    ///
    /// Строки биграмм обрезаются до ceil(V/5) — запас stride не утекает
    /// в артефакт: тот же словарь + та же топология → те же байты,
    /// независимо от ёмкости в памяти.
    pub fn to_bytes(&self) -> Vec<u8> {
        let vocab = self.tokens.len();
        let cols = vocab.div_ceil(5);
        let token_off = HEADER;
        let mut token_table = Vec::new();
        for t in &self.tokens {
            token_table.push(t.len() as u8);
            token_table.extend_from_slice(t.as_bytes());
        }
        let bigram_off = token_off + token_table.len();
        let mut out = Vec::with_capacity(bigram_off + vocab * cols);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(vocab as u32).to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes()); // flags: bigram present
        out.extend_from_slice(&(token_off as u32).to_le_bytes());
        out.extend_from_slice(&(bigram_off as u32).to_le_bytes());
        out.extend_from_slice(&self.corpus_words.to_le_bytes());
        out.extend_from_slice(&self.corpus_chars.to_le_bytes());
        out.extend_from_slice(&self.theta_hi_milli.to_le_bytes());
        let digest = {
            let mut body = Vec::with_capacity(token_table.len() + vocab * cols);
            body.extend_from_slice(&token_table);
            for row in 0..vocab {
                let from = row * self.stride;
                body.extend_from_slice(&self.bigram[from..from + cols]);
            }
            sha256(&body)
        };
        out.extend_from_slice(&digest);
        debug_assert_eq!(out.len(), HEADER, "заголовок .t5c ровно 0x50 байт");
        out.extend_from_slice(&token_table);
        for row in 0..vocab {
            let from = row * self.stride;
            out.extend_from_slice(&self.bigram[from..from + cols]);
        }
        out
    }

    /// Сохранение на диск.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        std::fs::write(path, self.to_bytes()).map_err(|e| format!("запись {path:?}: {e}"))
    }

    /// Зашитый в бинарник кристалл (собран из corpus_ru.txt движка).
    pub fn embedded() -> Result<Crystal, String> {
        Crystal::load(EMBEDDED_CRYSTAL, DEFAULT_DIMS)
    }

    /// θ_hi сборки (восстановление из милли-долей).
    pub fn theta_hi(&self) -> f64 {
        self.theta_hi_milli as f64 / 1000.0
    }

    /// Динамическое расширение словаря (Dynamic Vocab Expansion):
    /// новое слово мгновенно получает id, CSE-вложение и якорь архетипа —
    /// и становится полноценным участником речи.
    ///
    /// Матрица биграмм растёт с запасом stride (+64 столбца за рост),
    /// поэтому амортизированная стоимость добавления — O(1), а не
    /// O(V·cols) перезапаковка на каждое слово.
    pub fn expand_vocab(&mut self, word: &str) -> Option<u32> {
        if let Some(&id) = self.index.get(word) {
            return Some(id);
        }
        if word.is_empty() || word.len() > u8::MAX as usize {
            return None; // u8-длина в .t5c не вместит это слово
        }
        if self.tokens.len() >= u32::MAX as usize {
            return None;
        }
        let id = self.tokens.len() as u32;
        // Вакуум-байт Trit5: pack_5([0,0,0,0,0]) = 121 (не 0x00 — тот
        // декодируется как пять тритов −1!).
        let vacuum = Trit5Codec::pack_5(&[0i8; 5]).unwrap_or(121);
        // 1. Ёмкость матрицы: нужна строка для нового prev + столбец next.
        let need_cols = (self.tokens.len() + 1).div_ceil(5);
        if need_cols > self.stride {
            let new_stride = need_cols + 64;
            let mut grown = vec![vacuum; self.tokens.len() * new_stride];
            for row in 0..self.tokens.len() {
                let (from, to) = (row * self.stride, row * new_stride);
                grown[to..to + self.stride]
                    .copy_from_slice(&self.bigram[from..from + self.stride]);
            }
            self.bigram = grown;
            self.stride = new_stride;
        }
        self.bigram.extend(std::iter::repeat(vacuum).take(self.stride));
        // 2. Словарь + индекс.
        self.tokens.push(word.to_string());
        self.index.insert(word.to_string(), id);
        // 3. Вложения только для нового слова (не пересобираем весь V×dims).
        let v = cse_encode(word, self.dims);
        let v32: Vec<f32> = v.iter().map(|&x| x as f32).collect();
        let t = quantize(&v32, 0.05);
        let q = t.dequantize();
        self.embeds_trit.push(t);
        self.embeds_f64.push(q.iter().map(|&x| x as f64).collect());
        self.anchors
            .push((fnv1a64(word.as_bytes()) % ARCHETYPES as u64) as u8);
        Some(id)
    }

    /// Subword-фолбек: жадное разложение слова на известные подтокены
    /// (длиннейший префикс слева). Пустой вектор = слово не разложимо.
    /// Предел глубины 6 кусков защищает от бессмысленного дробления.
    pub fn subword_ids(&self, word: &str) -> Vec<u32> {
        let mut out = Vec::new();
        let bytes = word.as_bytes();
        let mut pos = 0usize;
        while pos < bytes.len() && out.len() < 6 {
            let mut best: Option<(usize, u32)> = None;
            // длиннейший префикс word[pos..], присутствующий в словаре
            for end in (pos + 1..=bytes.len()).rev() {
                if !word.is_char_boundary(end) {
                    continue;
                }
                if let Some(&id) = self.index.get(&word[pos..end]) {
                    best = Some((end, id));
                    break;
                }
            }
            match best {
                Some((end, id)) => {
                    out.push(id);
                    pos = end;
                }
                None => return Vec::new(), // тупик: слово не разложимо
            }
        }
        if pos < bytes.len() {
            return Vec::new(); // не доели слово за лимит кусков
        }
        out
    }

    /// Сборка кристалла из готовых компонентов (путь потокового
    /// инжектора `poler-engine::triune::ingest`): словарь + уже
    /// квантованная Trit5-матрица биграмм + статистика корпуса.
    pub fn from_parts(
        tokens: Vec<String>,
        bigram: Vec<u8>,
        corpus_words: u64,
        corpus_chars: u64,
        bigram_nonzeros: u64,
        theta_hi: f64,
        theta_lo: f64,
        dims: usize,
    ) -> Result<Crystal, String> {
        if !(0.5..=8.0).contains(&theta_hi) || !(0.05..=0.95).contains(&theta_lo) {
            return Err(format!("пороги вне диапазона: θ_hi={theta_hi}, θ_lo={theta_lo}"));
        }
        if theta_hi <= theta_lo {
            return Err("θ_hi должен быть больше θ_lo".into());
        }
        let vocab = tokens.len();
        if vocab == 0 || vocab > 1_000_000 {
            return Err(format!("странный словарь: {vocab}"));
        }
        let stride = vocab.div_ceil(5);
        if bigram.len() != vocab * stride {
            return Err(format!(
                "матрица биграмм {vocab}×{stride} ожидается, байт: {}",
                bigram.len()
            ));
        }
        let index: HashMap<String, u32> =
            tokens.iter().enumerate().map(|(i, t)| (t.clone(), i as u32)).collect();
        let mut crystal = Crystal {
            tokens,
            index,
            bigram,
            stride,
            corpus_words,
            corpus_chars,
            bigram_nonzeros,
            theta_hi_milli: (theta_hi * 1000.0).round() as u32,
            embeds_trit: Vec::new(),
            embeds_f64: Vec::new(),
            anchors: Vec::new(),
            dims: dims.clamp(16, 512),
        };
        crystal.derive_embeddings();
        Ok(crystal)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Тесты движка (перенесены из triune/crystal.rs, pqc/sha256.rs, ssn/cse.rs)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const CORPUS: &str = "Мозг мухи держит ритм мысли. Вихрь крутит смысл по кругу. \
        Кристалл хранит слова мира. Мозг говорит через кристалл. Вихрь диспетчеризует внимание. \
        Муха задаёт пульс системы. Система слушает вход. Вход течёт через кодировщик. \
        Кодировщик превращает текст в вектор. Вектор вливается в нейроны. \
        Открой терминал и покажи статус. Запусти сборку проекта. Покажи логи системы.";

    // ── SHA-256 ──

    fn hex_of(data: &[u8]) -> String {
        hex(&sha256(data))
    }

    #[test]
    fn sha256_known_vectors() {
        // FIPS 180-4 / NIST примеры.
        assert_eq!(
            hex_of(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex_of(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex_of(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn sha256_incremental_equals_oneshot() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let oneshot = sha256(&data);
        let mut inc = Sha256::new();
        for chunk in data.chunks(7) {
            inc.update(chunk);
        }
        assert_eq!(inc.finalize(), oneshot);
    }

    #[test]
    fn sha256_long_input_boundary() {
        // Границы паддинга: 55, 56, 63, 64, 119, 120 байт.
        for n in [55usize, 56, 63, 64, 119, 120, 128] {
            let data = vec![b'x'; n];
            let mut h = Sha256::new();
            h.update(&data);
            let d1 = h.finalize();
            assert_eq!(d1, sha256(&data), "n={n}");
            assert_eq!(d1.len(), 32);
        }
    }

    // ── CSE золотой фазы ──

    const A: &str = "герой идёт в поход против тьмы";
    const B: &str = "герой идёт в поход против бездны";
    const C: &str = "кофеварка сломалась вчера";

    #[test]
    fn cse_determinism_and_norm() {
        assert_eq!(cse_encode(A, 128), cse_encode(A, 128));
        let v = cse_encode(A, 128);
        assert!((cse_norm(&v) - 1.0).abs() < 1e-9, "norm={}", cse_norm(&v));
    }

    #[test]
    fn cse_parity_with_python_proof() {
        // Паритет с Python-доказательством движка (V8-прогон сьюта):
        // cos(a,b)=0.9987, cos(a,c)=-0.3547.
        let (va, vb, vc) = (cse_encode(A, 128), cse_encode(B, 128), cse_encode(C, 128));
        let dot = |a: &[f64], b: &[f64]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f64>();
        let norm = |a: &[f64]| cse_norm(a);
        let cab = dot(&va, &vb) / (norm(&va) * norm(&vb));
        let cac = dot(&va, &vc) / (norm(&va) * norm(&vc));
        assert!((cab - 0.9987).abs() < 5e-4, "cos(a,b)={cab:.6}");
        assert!((cac - (-0.3547)).abs() < 5e-4, "cos(a,c)={cac:.6}");
    }

    // ── Trit5 / TritState ──

    #[test]
    fn trit5_codec_roundtrip() {
        for b in 0u8..=242 {
            let five = Trit5Codec::unpack_5(b);
            assert_eq!(Trit5Codec::pack_5(&five), Some(b), "байт {b}");
        }
        // Вакуум — байт 121, не 0x00 (0x00 = пять тритов −1).
        assert_eq!(Trit5Codec::pack_5(&[0i8; 5]), Some(121));
        assert_eq!(Trit5Codec::unpack_5(121), [0i8; 5]);
        assert_eq!(Trit5Codec::unpack_5(0), [-1i8; 5]);
        // Хвостовые байты ≥ 243 — вакуум.
        assert_eq!(Trit5Codec::unpack_5(243), [0i8; 5]);
        assert_eq!(Trit5Codec::unpack_5(255), [0i8; 5]);
    }

    #[test]
    fn quantize_dequantize_roundtrip_bound() {
        let p = vec![0.9, -0.4, 0.0, 0.04, -0.95, 0.3, 0.3, 0.3];
        let q = quantize(&p, 0.05);
        let back = q.dequantize();
        assert_eq!(q.dims, 8);
        assert_eq!(q.scale, 0.95);
        assert!((back[0] - 0.95).abs() < 1e-6);
        assert!((back[1] + 0.95).abs() < 1e-6);
        assert_eq!(back[2], 0.0, "ноль остаётся нулём");
        assert_eq!(back[3], 0.0, "мёртвая зона 0.0475 отсекла 0.04");
        assert!((back[4] + 0.95).abs() < 1e-6);
        for (a, b) in p.iter().zip(&back) {
            assert!((a - b).abs() <= q.scale + 1e-6);
        }
    }

    #[test]
    fn nomul_dot_matches_scalar_math() {
        let p = vec![0.5, -0.5, 0.5, 0.5, -0.5, 0.0, 0.12, -0.5, 0.5, 0.5];
        let x = vec![1.0, 2.0, -3.0, 0.5, 0.25, 10.0, -2.0, 4.0, -1.0, 0.0];
        let q = quantize(&p, 0.0);
        let simd = q.dot(&x);
        let scalar = q.dequantize().iter().zip(&x).map(|(&a, &b)| a * b).sum::<f32>();
        assert!((simd - scalar).abs() < 1e-5, "nomul {simd} vs scalar {scalar}");
    }

    // ── Кристалл ──

    #[test]
    fn build_is_deterministic() {
        let a = Crystal::build(CORPUS, 64, 64, 1.7, 0.5).unwrap();
        let b = Crystal::build(CORPUS, 64, 64, 1.7, 0.5).unwrap();
        assert_eq!(a.to_bytes(), b.to_bytes(), "тот же корпус → те же байты");
    }

    #[test]
    fn roundtrip_preserves_everything() {
        let c = Crystal::build(CORPUS, 64, 64, 1.7, 0.5).unwrap();
        let bytes = c.to_bytes();
        let back = Crystal::load(&bytes, 64).unwrap();
        assert_eq!(back.tokens, c.tokens);
        assert_eq!(back.corpus_words, c.corpus_words);
        assert_eq!(back.corpus_chars, c.corpus_chars);
        assert_eq!(back.bigram_nonzeros, c.bigram_nonzeros);
        for prev in 0..c.vocab() as u32 {
            for next in 0..c.vocab() as u32 {
                assert_eq!(
                    c.bigram_trit(prev, next),
                    back.bigram_trit(prev, next),
                    "биграмма {prev}→{next} изменилась"
                );
            }
        }
    }

    #[test]
    fn tamper_detected_by_sha() {
        let mut bytes = Crystal::build(CORPUS, 64, 64, 1.7, 0.5).unwrap().to_bytes();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        assert!(Crystal::load(&bytes, 64).is_err(), "порча тела должна ловиться");
        bytes[0x0C] ^= 0x01; // порча заголовка (vocab)
        assert!(Crystal::load(&bytes, 64).is_err());
    }

    #[test]
    fn bigram_values_are_trits() {
        let c = Crystal::build(CORPUS, 64, 64, 1.7, 0.5).unwrap();
        for prev in 0..c.vocab() as u32 {
            for next in 0..c.vocab() as u32 {
                assert!([-1i8, 0, 1].contains(&c.bigram_trit(prev, next)));
            }
        }
        // «через кодировщик» — сильная биграмма корпуса → +1
        let (a, b) = (c.id_of("через").unwrap(), c.id_of("кодировщик").unwrap());
        assert_eq!(c.bigram_trit(a, b), 1, "частая пара должна притягиваться");
    }

    #[test]
    fn embeddings_are_no_mul_trits() {
        let c = Crystal::build(CORPUS, 64, 64, 1.7, 0.5).unwrap();
        for i in 0..c.vocab() as u32 {
            let t = c.embed_trit(i);
            assert_eq!(t.dims, 64);
            assert_eq!(t.packed_bytes().len(), 64_usize.div_ceil(5));
            assert!(t.nonzeros > 0, "вложение токена не вакуум");
        }
    }

    #[test]
    fn anchors_in_archetype_range() {
        let c = Crystal::build(CORPUS, 64, 64, 1.7, 0.5).unwrap();
        for i in 0..c.vocab() as u32 {
            assert!(c.anchor(i) < ARCHETYPES);
        }
        // Одинаковое слово → одинаковый якорь (детерминизм FNV).
        assert_eq!(c.anchor(0), c.anchor(0));
    }

    #[test]
    fn embedded_crystal_loads() {
        let c = Crystal::embedded().expect("зашитый кристалл должен загружаться");
        assert!(c.vocab() >= 32, "словарь встроенного кристалла: {}", c.vocab());
        assert!(c.corpus_words > 100, "корпус встроенного кристалла: {}", c.corpus_words);
        assert!(c.bigram_nonzeros > 0, "биграммы встроенного кристалла пусты");
    }

    #[test]
    fn tokenizer_lowercases_and_splits() {
        let ts = tokenize("Открой Терминал, и — покажи: статус!");
        assert_eq!(ts, vec!["открой", "терминал", "и", "покажи", "статус"]);
    }

    #[test]
    fn expand_vocab_gives_new_word_full_life() {
        let mut c = Crystal::build(CORPUS, 64, 64, 1.7, 0.5).unwrap();
        let v0 = c.vocab();
        assert!(c.id_of("квантигон").is_none(), "слова нет в корпусе");
        let id = c.expand_vocab("квантигон").expect("слово должно добавиться");
        assert_eq!(c.vocab(), v0 + 1);
        assert_eq!(c.id_of("квантигон"), Some(id));
        // Вложения и якорь на месте.
        assert_eq!(c.embed_trit(id).dims, 64);
        assert!(c.embed_trit(id).nonzeros > 0, "вложение нового слова не вакуум");
        assert!(!c.embed_f64(id).is_empty());
        assert!(c.anchor(id) < ARCHETYPES);
        // Повторное добавление — идемпотентно.
        assert_eq!(c.expand_vocab("квантигон"), Some(id));
        assert_eq!(c.vocab(), v0 + 1);
        // Пустое и сверхдлинное слова отклоняются.
        assert!(c.expand_vocab("").is_none());
        let long = "а".repeat(256);
        assert!(c.expand_vocab(&long).is_none());
    }

    #[test]
    fn expand_vocab_preserves_bigrams_and_serializes_clean() {
        let mut c = Crystal::build(CORPUS, 64, 64, 1.7, 0.5).unwrap();
        let (a, b) = (c.id_of("через").unwrap(), c.id_of("кодировщик").unwrap());
        let before = c.bigram_trit(a, b);
        assert_eq!(before, 1, "пару ожидает притяжение");
        // 30 расширений — матрица растёт со stride-запасом.
        for i in 0..30 {
            c.expand_vocab(&format!("неологизм{i}")).unwrap();
        }
        assert_eq!(c.bigram_trit(a, b), 1, "старые биграммы целы после роста");
        // Новая строка — вакуум (слово никогда не встречалось в парах).
        let new_id = c.id_of("неологизм0").unwrap();
        for next in 0..c.vocab() as u32 {
            assert_eq!(c.bigram_trit(new_id, next), 0);
        }
        // Сериализация: stride-запас не утекает, roundtrip бит-в-бит.
        let bytes = c.to_bytes();
        let back = Crystal::load(&bytes, 64).unwrap();
        assert_eq!(back.vocab(), c.vocab());
        for prev in 0..c.vocab() as u32 {
            for next in 0..c.vocab() as u32 {
                assert_eq!(c.bigram_trit(prev, next), back.bigram_trit(prev, next));
            }
        }
        // Файл ровно V×ceil(V/5) байт биграмм: никаких скрытых запасов.
        let v = c.vocab();
        let tail = &bytes[0x50..];
        let mut pos = 0usize;
        for _ in 0..v {
            pos += 1 + tail[pos] as usize;
        }
        assert_eq!(bytes.len(), 0x50 + pos + v * v.div_ceil(5), "хвост — ровно матрица");
    }

    #[test]
    fn subword_fallback_decomposes_unknown_words() {
        let c = Crystal::build(CORPUS, 64, 64, 1.7, 0.5).unwrap();
        // «мозговой» не в корпусе, но «мозг» есть… проверяем общий механизм
        // на гарантированно разложимом случае: склейке двух словарных слов.
        let w1 = c.tokens[0].clone();
        let w2 = c.tokens[1].clone();
        let glued = format!("{w1}{w2}");
        let ids = c.subword_ids(&glued);
        assert_eq!(ids.len(), 2, "склейка двух слов режется на 2 куска");
        assert_eq!(ids[0], c.id_of(&w1).unwrap());
        assert_eq!(ids[1], c.id_of(&w2).unwrap());
        // Словарное слово = само себя одним куском.
        assert_eq!(c.subword_ids(&w1), vec![c.id_of(&w1).unwrap()]);
        // Неразложимый мусор → пусто.
        assert!(c.subword_ids("ъъъъъ").is_empty() || {
            // «ъ» может не быть в словаре — тогда пусто; иначе один кусок
            c.subword_ids("ъъъъъ").len() <= 1
        });
    }

    #[test]
    fn from_parts_rejects_bad_input() {
        let c = Crystal::build(CORPUS, 64, 64, 1.7, 0.5).unwrap();
        let v = c.vocab();
        let stride = v.div_ceil(5);
        // Матрица не того размера.
        assert!(Crystal::from_parts(
            c.tokens.clone(),
            vec![0u8; v * stride + 1],
            c.corpus_words, c.corpus_chars, c.bigram_nonzeros, 1.7, 0.5, 64
        )
        .is_err());
        // Пороги вне диапазона.
        assert!(Crystal::from_parts(
            c.tokens.clone(),
            vec![0u8; v * stride],
            c.corpus_words, c.corpus_chars, c.bigram_nonzeros, 9.9, 0.5, 64
        )
        .is_err());
        // Корректный вход — работает.
        let parts = Crystal::from_parts(
            c.tokens.clone(),
            vec![121u8; v * stride],
            c.corpus_words, c.corpus_chars, 0, 1.7, 0.5, 64,
        );
        assert!(parts.is_ok());
    }

    #[test]
    fn hebbian_bump_saturates_and_counts() {
        let mut c = Crystal::build(CORPUS, 64, 64, 1.7, 0.5).unwrap();
        let (a, b) = (c.id_of("мозг").unwrap(), c.id_of("муха").unwrap());
        let nz_before = c.bigram_nonzeros;
        let t0 = c.bigram_trit(a, b);
        // Пять усилений подряд — насыщение на +1.
        let mut t = t0;
        for _ in 0..5 {
            t = c.bigram_bump(a, b, 1);
        }
        assert_eq!(t, 1, "синапс насытился на +1");
        // Счётчик ненулевых согласован.
        if t0 == 0 && t == 1 {
            assert_eq!(c.bigram_nonzeros, nz_before + 1);
        }
        // Ослабление до −1 и обратно в вакуум не роняет инвариант ≥ 0.
        for _ in 0..10 {
            c.bigram_bump(a, b, -1);
        }
        assert_eq!(c.bigram_trit(a, b), -1);
        assert!(c.bigram_nonzeros > 0);
    }

    #[test]
    fn assoc_neighbors_combine_semantics_and_cooccurrence() {
        let c = Crystal::build(CORPUS, 64, 64, 1.7, 0.5).unwrap();
        let (a, b) = (c.id_of("через").unwrap(), c.id_of("кодировщик").unwrap());
        // Прямой партнёр по биграмме попадает в ассоциаты.
        let assoc = c.assoc_neighbors(a, 4, 8);
        assert!(assoc.contains(&b), "кодировщик — партнёр «через»");
        // Нет повторов и нет самого нейрона.
        let mut sorted = assoc.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), assoc.len(), "дубликатов нет");
        assert!(!assoc.contains(&a), "сам себе не сосед");
    }
}
