//! VAULT-ASM — сокровищница DeepSeek Vault в машинном коде x86_64.
//!
//! v0.87.0: пять микроядер на чистом ассемблере (Intel-синтаксис,
//! `global_asm!`), портированных из `docs/vault_drafts/`:
//!
//! | Ядро | Источник в сокровищнице | Базис |
//! |------|--------------------------|-------|
//! | [`fep_asm`] | `poler-core/src/fep_loss.rs` (Фристон) | AVX2+FMA |
//! | [`lens_asm`] | `poler-lens/src/lens_index.rs` (No-Hits) | SSE2 (базовый!) |
//! | [`synapse_asm`] | блоки 0986/0984 (синапс 1.7Б) | AVX2+FMA+F16C |
//! | [`cordic_asm`] | P3/CORDIC S¹ | SSE2 + целочисленный |
//! | [`stdp_asm`] | LanguageCoreV2 (0601, STDP+WTA) | AVX2+FMA |
//!
//! v0.88.0 TRIT-ASM: троичная физика процессора — триты и кутриты
//! подняты из Rust-слоя движка (calc/trits.rs, pqc) в машинный код:
//!
//! | Ядро | Физика | Базис |
//! |------|--------|-------|
//! | [`trit_asm`] | 𝕋={−1,0,+1}: Клини, vpsignb, LEA×3, magic-÷3, VPTERNLOGD | SSSE3/SSE4.1/AVX-512 |
//! | [`qutrit_asm`] | ℤ₃ ω-ротор 120° + мост трит→S¹ | SSE2/AVX2+FMA |
//!
//! v0.89.0 ABS-ZERO: АЗУ — Абсолютный Ноль ([`zero_asm`]): проективная
//! прямая ℝP¹ [N:D] (деление перекрёстным умножением — ДЕЛЕНИЯ НЕТ,
//! полюс D=0 — точка, не сбой), изолированный/уникальный/симметричный
//! машинный нуль (121 = все-⊙, ±(3⁴⁰−1)/2), безопасное деление без #DE,
//! ноль-сумма кутрита (N, D, −(N+D)).
//!
//! v0.90.0 SYNAPSE-TRIT ([`synapse_trit_asm`]): слияние тритов + кутритов
//! + нейромодулей — ℤ₁₂-ротор (синус за ~1 такт вместо ~200 FLOPs),
//! трит-MAC без умножителя, pack5-поле 0.2 Б/синапс (50M синапсов
//! FlyWire = 10 МБ — кэш L3), CSR-коннектом с rayon-параллелизмом,
//! точный Q8-распад к абсолютному нулю за конечные шаги.
//!
//! Каждое ядро имеет скалярный эталон (fallback для хостов без AVX2 и
//! референс для тестов). CLI: `--asm-info`, `--asm-bench`.

#[cfg(target_arch = "x86_64")]
// (не перенесено из poler-engine: нужно только трит-спектроскопии ε-ранга)
#[cfg(target_arch = "x86_64")]
// (fep_asm остался в poler-engine/harvest: cli-скану нужен только lens)
#[cfg(target_arch = "x86_64")]
pub mod lens_asm;
#[cfg(target_arch = "x86_64")]
// (не перенесено из poler-engine: нужно только трит-спектроскопии ε-ранга)
#[cfg(target_arch = "x86_64")]
// (не перенесено из poler-engine: нужно только трит-спектроскопии ε-ранга)
#[cfg(target_arch = "x86_64")]
// (не перенесено из poler-engine: нужно только трит-спектроскопии ε-ранга)
#[cfg(target_arch = "x86_64")]
// (не перенесено из poler-engine: нужно только трит-спектроскопии ε-ранга)
#[cfg(target_arch = "x86_64")]
// (cordic_asm остался в poler-engine/harvest: cli-скану нужен только lens)
// (qutrit_asm остался в poler-engine/harvest: cli-скану нужен только lens)
// (stdp_asm остался в poler-engine/harvest: cli-скану нужен только lens)
// (synapse_asm остался в poler-engine/harvest: cli-скану нужен только lens)
// (synapse_trit_asm остался в poler-engine/harvest: cli-скану нужен только lens)
// (trit_asm остался в poler-engine/harvest: cli-скану нужен только lens)
#[cfg(target_arch = "x86_64")]
// (zero_asm остался в poler-engine/harvest: cli-скану нужен только lens)

use std::sync::OnceLock;

/// CPUID-возможности хоста (детекция один раз).
#[derive(Debug, Clone, Copy)]
pub struct AsmCaps {
    pub avx2: bool,
    pub fma: bool,
    pub f16c: bool,
    pub popcnt: bool,
    pub avx512f: bool,
    pub sse2: bool,
    /// AVX (VEX-кодировка) — базис vpsubb/vpsignb/vpminsb-ядер trit_asm.
    pub avx: bool,
    /// SSSE3 — vpsignb (тритное умножение знаком).
    pub ssse3: bool,
    /// SSE4.1 — vpminsb/vpmaxsb (Клини-вентили), vpmovsxbw.
    pub sse41: bool,
}

impl AsmCaps {
    fn detect() -> Self {
        Self {
            avx2: is_x86_feature_detected!("avx2"),
            fma: is_x86_feature_detected!("fma"),
            f16c: is_x86_feature_detected!("f16c"),
            popcnt: is_x86_feature_detected!("popcnt"),
            avx512f: is_x86_feature_detected!("avx512f"),
            sse2: is_x86_feature_detected!("sse2"),
            avx: is_x86_feature_detected!("avx"),
            ssse3: is_x86_feature_detected!("ssse3"),
            sse41: is_x86_feature_detected!("sse4.1"),
        }
    }

    /// Золотой набор VAULT-ASM (все ядра, кроме SSE2-базовых).
    pub fn golden(&self) -> bool {
        self.avx2 && self.fma && self.f16c
    }

    /// Троичный золотой набор TRIT-ASM (Клини + vpsignb + VPTERNLOGD).
    pub fn trit_golden(&self) -> bool {
        self.avx && self.ssse3 && self.sse41 && self.avx512f
    }
}

static CAPS: OnceLock<AsmCaps> = OnceLock::new();

/// Возможности CPU (кэш через `OnceLock`).
pub fn caps() -> AsmCaps {
    *CAPS.get_or_init(AsmCaps::detect)
}

/// Текстовый отчёт о микроядрах для CLI (`--asm-info`).
pub fn info_report() -> String {
    let c = caps();
    let on = |b: bool| if b { "ON" } else { "—" };
    format!(
        "VAULT-ASM x86_64 microkernels (v0.90.0 SYNAPSE-TRIT)\n\
         \n\
         CPU basis:\n\
           AVX2      : {}\n\
           FMA       : {}\n\
           F16C      : {}\n\
           POPCNT    : {}\n\
           AVX-512F  : {}\n\
           SSE2      : {} (baseline — LENS/CORDIC/QUTRIT-Q15 всегда)\n\
           SSSE3     : {} (vpsignb — тритное умножение знаком)\n\
           SSE4.1    : {} (Клини-вентили vpminsb/vpmaxsb)\n\
         \n\
         Kernels (source → engine):\n\
           fep_asm      poler-core/fep_loss.rs  → literary/engine.rs (F, ∇F)  [AVX2+FMA]\n\
           lens_asm     poler-lens/lens_index.rs → engine.rs Causal Nexus    [SSE2]\n\
           synapse_asm  vault 0986/0984 (1.7Б)  → SSN inference layer        [AVX2+FMA+F16C]\n\
           cordic_asm   P3 CORDIC S¹ (Q30)      → детерминированный ротор    [SSE2+int]\n\
           stdp_asm     LanguageCoreV2 0601      → трёхфакторный STDP + WTA   [AVX2+FMA]\n\
         \n\
         TRIT-ASM v0.88.0 (троичная физика процессора):\n\
           trit_asm     𝕋={{−1,0,+1}} Клини-вентили / ⟨u,v⟩ vpsignb /\n\
                       pack5 LEA×3 / unpack5 magic-÷3 / тернарный спайк ⊕⊙⊖\n\
                       / VPTERNLOGD-мультиплексор (imm 0xE4, sel?a:b)   [SSSE3+SSE4.1, AVX-512]\n\
           qutrit_asm   ℤ₃ ω-ротор 120° (f32 FMA + Q14 pmulhw) /\n\
                       мост трит→S¹ (θ_t = t·2π/3)              [SSE2/AVX2]\n\
         \n\
         ABS-ZERO v0.89.0 (АЗУ — Абсолютный Ноль, ℝP¹):\n\
           zero_asm     [N:D] класс (Finite/Zero/Pole/Gauge) / инверсия=ОБМЕН /\n\
                       умножение и ДЕЛЕНИЕ перекрёстным умножением (полюс D=0 —\n\
                       точка, не сбой; 0/0 → калибровка) / 128-битное сравнение\n\
                       дробей БЕЗ деления / div-safe ±MAX насыщение (нет #DE) /\n\
                       вакуум-скан pack5 (121=все⊙, pcmpeqb+popcnt) /\n\
                       qutrit3 (N,D,−(N+D)) — сумма ≡ 0 по построению    [int+SSE2+POPCNT]\n\
         \n\
         SYNAPSE-TRIT v0.90.0 (слияние тритов + кутритов + нейромодулей):\n\
           synapse_trit_asm  ℤ₁₂-ротор sincos Q24 — LUT в РЕГИСТРАХ (vpermd+\n\
                             vblendvps), ω-поворот бит-точно mod 12, 1+ω+ω²=0 /\n\
                             трит-MAC vpsignb (32 трита/итер, 0 умножений) /\n\
                             pack5-поле 0.2 Б/синапс: 50M синапсов FlyWire = 10 МБ\n\
                             (весь мозг в кэше L3) / CSR-коннектом + rayon /\n\
                             Q8-распад: абсолютный ноль за КОНЕЧНЫЕ шаги /\n\
                             АЗУ: точное [N:D]-ранжирование рёбер нексуса\n\
                             (cmp_canonical вместо f64-кастов), div_safe_f32\n\
                             в FEP-нормировке и STDP-softmax           [AVX2+SSSE3+SSE4.1]\n\
         \n\
         Golden set: {} | TRIT golden: {}",
        on(c.avx2),
        on(c.fma),
        on(c.f16c),
        on(c.popcnt),
        on(c.avx512f),
        on(c.sse2),
        on(c.ssse3),
        on(c.sse41),
        on(c.golden()),
        on(c.trit_golden()),
    )
}

/// Печать отчёта (CLI `--asm-info`).
pub fn print_info() {
    println!("{}", info_report());
}

fn mps(n: f64, secs: f64) -> f64 {
    n / secs / 1e6
}

fn gbs(bytes: f64, secs: f64) -> f64 {
    bytes / secs / 1e9
}

// (run_bench и bench-хелперы не перенесены: SIMD-микроядра остались
//  в poler-engine / harvest::trit_gate; cli-скану нужен только lens_asm)