//! poler-edit :: TRIT-SILICON — троичная физика кремния (𝕋 = {-1, 0, +1},
//! Абсолютный Ноль [N:D] на ℝP¹, вакуум pack5 = 121, барьер LENS No-Hits,
//! 256-тритные семантические сигнатуры скоупов и 3-way алгебра слияния Клини).
//!
//! Портировано и адаптировано из `poler-engine/src/asm/{trit_asm,zero_asm,lens_asm}`:
//! * На `x86_64` работают нативные микроядра `global_asm!` (`extern "sysv64"`,
//!   совместимо с Linux ELF, Windows MSVC и Windows GNU / Wine):
//!   - `poler_edit_trit_neg` (SSE2 `psubb` — 16 трит/такт на ЛЮБОМ x86_64)
//!   - `poler_edit_trit_and` / `poler_edit_trit_or` (SSE4.1 `pminsb`/`pmaxsb`)
//!   - `poler_edit_trit_dot` (SSSE3+SSE4.1 `psignb` + `pmovsxbw` + `pmaddwd` —
//!     скалярное произведение тритов БЕЗ вещественного и без байтового умножителя)
//!   - `poler_edit_trit_pack5` (цепочки `lea` ×3 без `imul`, 5 трит → 1 байт)
//!   - `poler_edit_trit_unpack5` (magic-множитель `0x55555556` без `div`)
//!   - `poler_edit_az_class`, `poler_edit_az_inv`, `poler_edit_az_cmp`
//!     (128-битное сравнение дробей `[N:D]` перекрёстным `imul` БЕЗ `idiv`)
//!   - `poler_edit_az_trit_zero_count` (SSE2 `pcmpeqb 121` + `pmovmskb` + `popcnt`)
//! * На `aarch64` / прочих таргетах (Android / macOS Silicon) автоматически
//!   включаются побитово эквивалентные скалярные ядра.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::sync::OnceLock;

/// Уникальный код троичного вакуума в `pack5`: (3⁵ − 1)/2 = 121 (0x79).
/// Единственный байт из [0..242], где все 5 тритов равны 0 (⊙⊙⊙⊙⊙).
pub const TRIT5_ZERO_CODE: u8 = 121;

/// Симметричные полюса насыщения проективной прямой ℝP¹ (|POS| == |NEG|).
pub const POLE_I64_POS: i64 = i64::MAX;
pub const POLE_I64_NEG: i64 = -i64::MAX;

/// Длина троичной семантической сигнатуры скоупа: 256 тритов → 52 pack5-байта
/// (в 118 раз компактнее классического 1536×f32 RAG-эмбеддинга).
pub const SIG_TRITS: usize = 256;
pub const SIG_PACKED_BYTES: usize = 52; // ceil(256 / 5) = 52 (260 слотов, хвост ⊙)

/// CPUID-паспорт кремния текущего хоста.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct SiliconCaps {
    pub arch: &'static str,
    pub sse2: bool,
    pub ssse3: bool,
    pub sse41: bool,
    pub avx2: bool,
    pub popcnt: bool,
}

impl SiliconCaps {
    fn detect() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            Self {
                arch: "x86_64",
                sse2: std::is_x86_feature_detected!("sse2"),
                ssse3: std::is_x86_feature_detected!("ssse3"),
                sse41: std::is_x86_feature_detected!("sse4.1"),
                avx2: std::is_x86_feature_detected!("avx2"),
                popcnt: std::is_x86_feature_detected!("popcnt"),
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            Self {
                arch: std::env::consts::ARCH,
                sse2: false,
                ssse3: false,
                sse41: false,
                avx2: false,
                popcnt: false,
            }
        }
    }
}

static CAPS: OnceLock<SiliconCaps> = OnceLock::new();

#[inline]
pub fn caps() -> SiliconCaps {
    *CAPS.get_or_init(SiliconCaps::detect)
}

// =====================================================================
// x86_64 global_asm! микроядра (SSE2 / SSSE3 / SSE4.1 / POPCNT / int128)
// =====================================================================

#[cfg(target_arch = "x86_64")]
mod x86_kernels {
    use std::arch::global_asm;

    global_asm!(
        r#"
        .text
        .p2align 4
    .Lpe_z121:
        .byte 121, 121, 121, 121, 121, 121, 121, 121
        .byte 121, 121, 121, 121, 121, 121, 121, 121
        .p2align 4
    .Lpe_one16:
        .word 1, 1, 1, 1, 1, 1, 1, 1

        # ----------------------------------------------------------------
        # void poler_edit_trit_neg(const i8* a, i8* out, usize n)
        #   rdi=a, rsi=out, rdx=n. Базовый SSE2 (доступен на каждом x86_64).
        # ----------------------------------------------------------------
        .globl poler_edit_trit_neg
        .p2align 4
    poler_edit_trit_neg:
        xor rcx, rcx
        pxor xmm1, xmm1
        test rdx, rdx
        jz .Lpetn_fin
    .Lpetn_lp:
        mov rax, rdx
        sub rax, rcx
        cmp rax, 16
        jb .Lpetn_fin
        movdqu xmm0, [rdi + rcx]
        movdqa xmm2, xmm1
        psubb xmm2, xmm0
        movdqu [rsi + rcx], xmm2
        add rcx, 16
        jmp .Lpetn_lp
    .Lpetn_fin:
        ret

        # ----------------------------------------------------------------
        # void poler_edit_trit_and(const i8* a, const i8* b, i8* out, usize n)
        #   Клини AND = min(a, b) (SSE4.1 pminsb).
        # ----------------------------------------------------------------
        .globl poler_edit_trit_and
        .p2align 4
    poler_edit_trit_and:
        xor r8, r8
        test rcx, rcx
        jz .Lpeta_fin
    .Lpeta_lp:
        mov rax, rcx
        sub rax, r8
        cmp rax, 16
        jb .Lpeta_fin
        movdqu xmm0, [rdi + r8]
        movdqu xmm1, [rsi + r8]
        pminsb xmm0, xmm1
        movdqu [rdx + r8], xmm0
        add r8, 16
        jmp .Lpeta_lp
    .Lpeta_fin:
        ret

        # ----------------------------------------------------------------
        # void poler_edit_trit_or(const i8* a, const i8* b, i8* out, usize n)
        #   Клини OR = max(a, b) (SSE4.1 pmaxsb).
        # ----------------------------------------------------------------
        .globl poler_edit_trit_or
        .p2align 4
    poler_edit_trit_or:
        xor r8, r8
        test rcx, rcx
        jz .Lpeto_fin
    .Lpeto_lp:
        mov rax, rcx
        sub rax, r8
        cmp rax, 16
        jb .Lpeto_fin
        movdqu xmm0, [rdi + r8]
        movdqu xmm1, [rsi + r8]
        pmaxsb xmm0, xmm1
        movdqu [rdx + r8], xmm0
        add r8, 16
        jmp .Lpeto_lp
    .Lpeto_fin:
        ret

        # ----------------------------------------------------------------
        # i64 poler_edit_trit_dot(const i8* u, const i8* v, usize n)
        #   rdi=u, rsi=v, rdx=n -> rax = Σ u_i * v_i (кратно 16).
        #   Умножение ЗНАКОМ (SSSE3 psignb + SSE4.1 pmovsxbw + pmaddwd):
        #   для тритов ∈ [-1, 0, +1] psignb(u, v) == u * v ТОЧНО!
        # ----------------------------------------------------------------
        .globl poler_edit_trit_dot
        .p2align 4
    poler_edit_trit_dot:
        xor rcx, rcx
        pxor xmm4, xmm4                 # 4×i32 аккумулятор
        lea r9, [rip + .Lpe_one16]
        movdqa xmm5, [r9]
        test rdx, rdx
        jz .Lpetd_red
    .Lpetd_lp:
        mov rax, rdx
        sub rax, rcx
        cmp rax, 16
        jb .Lpetd_red
        movdqu xmm0, [rdi + rcx]
        movdqu xmm1, [rsi + rcx]
        psignb xmm0, xmm1               # 16 трит-произведений без умножителя
        pmovsxbw xmm2, xmm0             # младшие 8 тритов -> i16
        psrldq xmm0, 8
        pmovsxbw xmm3, xmm0             # старшие 8 тритов -> i16
        paddw xmm2, xmm3
        pmaddwd xmm2, xmm5              # пары i16 -> 4×i32
        paddd xmm4, xmm2
        add rcx, 16
        jmp .Lpetd_lp
    .Lpetd_red:
        movdqa xmm0, xmm4
        psrldq xmm0, 8
        paddd xmm4, xmm0
        movdqa xmm0, xmm4
        psrldq xmm0, 4
        paddd xmm4, xmm0
        movd eax, xmm4
        movsxd rax, eax
        ret

        # ----------------------------------------------------------------
        # usize poler_edit_trit_pack5(const i8* trits, u8* out, usize n_trits)
        #   5 тритов -> 1 байт [0..242] по схеме Горнера на LEA ×3 (без imul).
        # ----------------------------------------------------------------
        .globl poler_edit_trit_pack5
        .p2align 4
    poler_edit_trit_pack5:
        xor rcx, rcx
        xor r8, r8
    .Lpetp_lp:
        mov rax, rdx
        sub rax, rcx
        cmp rax, 5
        jb .Lpetp_fin
        movsx r10, byte ptr [rdi + rcx]
        inc r10
        movsx rax, byte ptr [rdi + rcx + 1]
        lea r10, [r10 + r10*2]
        lea r10, [r10 + rax + 1]
        movsx rax, byte ptr [rdi + rcx + 2]
        lea r10, [r10 + r10*2]
        lea r10, [r10 + rax + 1]
        movsx rax, byte ptr [rdi + rcx + 3]
        lea r10, [r10 + r10*2]
        lea r10, [r10 + rax + 1]
        movsx rax, byte ptr [rdi + rcx + 4]
        lea r10, [r10 + r10*2]
        lea r10, [r10 + rax + 1]
        mov [rsi + r8], r10b
        lea rcx, [rcx + 5]
        inc r8
        jmp .Lpetp_lp
    .Lpetp_fin:
        mov rax, r8
        ret

        # ----------------------------------------------------------------
        # usize poler_edit_trit_unpack5(const u8* packed, i8* out, usize n_bytes)
        #   1 байт -> 5 тритов через magic-множитель 0x55555556 (без div).
        # ----------------------------------------------------------------
        .globl poler_edit_trit_unpack5
        .p2align 4
    poler_edit_trit_unpack5:
        push rbx
        mov r10d, 0x55555556
        xor rcx, rcx
        mov r9, rsi
        mov r11, rdx
        test r11, r11
        jz .Lpetu_fin
    .Lpetu_lp:
        cmp rcx, r11
        jae .Lpetu_fin
        movzx ebx, byte ptr [rdi + rcx]
        mov eax, ebx
        mul r10d
        lea r8, [rdx + rdx*2]
        mov eax, ebx
        sub eax, r8d
        dec eax
        mov [r9 + 4], al
        mov ebx, edx
        mov eax, ebx
        mul r10d
        lea r8, [rdx + rdx*2]
        mov eax, ebx
        sub eax, r8d
        dec eax
        mov [r9 + 3], al
        mov ebx, edx
        mov eax, ebx
        mul r10d
        lea r8, [rdx + rdx*2]
        mov eax, ebx
        sub eax, r8d
        dec eax
        mov [r9 + 2], al
        mov ebx, edx
        mov eax, ebx
        mul r10d
        lea r8, [rdx + rdx*2]
        mov eax, ebx
        sub eax, r8d
        dec eax
        mov [r9 + 1], al
        mov ebx, edx
        mov eax, ebx
        mul r10d
        lea r8, [rdx + rdx*2]
        mov eax, ebx
        sub eax, r8d
        dec eax
        mov [r9], al
        inc rcx
        add r9, 5
        jmp .Lpetu_lp
    .Lpetu_fin:
        lea rax, [rcx + rcx*4]
        pop rbx
        ret

        # ----------------------------------------------------------------
        # u64 poler_edit_az_class(i64 n, i64 d)
        #   0=Finite, 1=Zero, 2=Pole, 3=Gauge (branchless sete + lea).
        # ----------------------------------------------------------------
        .globl poler_edit_az_class
        .p2align 4
    poler_edit_az_class:
        xor eax, eax
        test rdi, rdi
        sete al
        xor ecx, ecx
        test rsi, rsi
        sete cl
        lea rax, [rax + rcx*2]
        ret

        # ----------------------------------------------------------------
        # i64 poler_edit_az_cmp(i64 n1, i64 d1, i64 n2, i64 d2) -> [-1, 0, +1]
        #   128-битное перекрёстное сравнение N1*D2 vs N2*D1 БЕЗ деления!
        # ----------------------------------------------------------------
        .globl poler_edit_az_cmp
        .p2align 4
    poler_edit_az_cmp:
        mov r8, rdx
        mov rax, rdi
        imul rcx
        mov r10, rax
        mov r11, rdx
        mov rax, r8
        imul rsi
        cmp r11, rdx
        jl .Lpeac_lt
        jg .Lpeac_gt
        cmp r10, rax
        jb .Lpeac_lt
        ja .Lpeac_gt
        xor eax, eax
        ret
    .Lpeac_lt:
        mov rax, -1
        ret
    .Lpeac_gt:
        mov rax, 1
        ret

        # ----------------------------------------------------------------
        # usize poler_edit_az_trit_zero_count(const u8* packed, usize n)
        #   Число вакуумных байтов 121 (⊙⊙⊙⊙⊙) за проход SSE2 + POPCNT.
        # ----------------------------------------------------------------
        .globl poler_edit_az_trit_zero_count
        .p2align 4
    poler_edit_az_trit_zero_count:
        xor eax, eax
        xor rcx, rcx
        test rsi, rsi
        jz .Lpetz_fin
        lea r9, [rip + .Lpe_z121]
        movdqa xmm1, [r9]
    .Lpetz_lp:
        mov rdx, rsi
        sub rdx, rcx
        cmp rdx, 16
        jb .Lpetz_fin
        movdqu xmm0, [rdi + rcx]
        pcmpeqb xmm0, xmm1
        pmovmskb r8d, xmm0
        popcnt r8d, r8d
        add eax, r8d
        add rcx, 16
        jmp .Lpetz_lp
    .Lpetz_fin:
        ret
        "#
    );

    extern "sysv64" {
        pub fn poler_edit_trit_neg(a: *const i8, out: *mut i8, n: usize);
        pub fn poler_edit_trit_and(a: *const i8, b: *const i8, out: *mut i8, n: usize);
        pub fn poler_edit_trit_or(a: *const i8, b: *const i8, out: *mut i8, n: usize);
        pub fn poler_edit_trit_dot(u: *const i8, v: *const i8, n: usize) -> i64;
        pub fn poler_edit_trit_pack5(trits: *const i8, out: *mut u8, n_trits: usize) -> usize;
        pub fn poler_edit_trit_unpack5(packed: *const u8, out: *mut i8, n_bytes: usize) -> usize;
        pub fn poler_edit_az_class(n: i64, d: i64) -> u64;
        pub fn poler_edit_az_cmp(n1: i64, d1: i64, n2: i64, d2: i64) -> i64;
        pub fn poler_edit_az_trit_zero_count(packed: *const u8, n: usize) -> usize;
    }
}

// =====================================================================
// Публичный фасад TRIT-ASM + скалярные эталоны
// =====================================================================

#[inline]
pub fn clamp_trit(v: i8) -> i8 {
    if v > 0 {
        1
    } else if v < 0 {
        -1
    } else {
        0
    }
}

pub fn trit_neg_scalar(a: &[i8], out: &mut [i8]) {
    let n = a.len().min(out.len());
    for i in 0..n {
        out[i] = -a[i];
    }
}

pub fn trit_neg(a: &[i8], out: &mut [i8]) {
    let n = a.len().min(out.len());
    #[cfg(target_arch = "x86_64")]
    if caps().sse2 && n >= 16 {
        let bulk = n & !15;
        unsafe { x86_kernels::poler_edit_trit_neg(a.as_ptr(), out.as_mut_ptr(), bulk) };
        trit_neg_scalar(&a[bulk..n], &mut out[bulk..n]);
        return;
    }
    trit_neg_scalar(&a[..n], &mut out[..n]);
}

pub fn trit_and_scalar(a: &[i8], b: &[i8], out: &mut [i8]) {
    let n = a.len().min(b.len()).min(out.len());
    for i in 0..n {
        out[i] = a[i].min(b[i]);
    }
}

pub fn trit_and(a: &[i8], b: &[i8], out: &mut [i8]) {
    let n = a.len().min(b.len()).min(out.len());
    #[cfg(target_arch = "x86_64")]
    if caps().sse41 && n >= 16 {
        let bulk = n & !15;
        unsafe {
            x86_kernels::poler_edit_trit_and(a.as_ptr(), b.as_ptr(), out.as_mut_ptr(), bulk)
        };
        trit_and_scalar(&a[bulk..n], &b[bulk..n], &mut out[bulk..n]);
        return;
    }
    trit_and_scalar(&a[..n], &b[..n], &mut out[..n]);
}

pub fn trit_or_scalar(a: &[i8], b: &[i8], out: &mut [i8]) {
    let n = a.len().min(b.len()).min(out.len());
    for i in 0..n {
        out[i] = a[i].max(b[i]);
    }
}

pub fn trit_or(a: &[i8], b: &[i8], out: &mut [i8]) {
    let n = a.len().min(b.len()).min(out.len());
    #[cfg(target_arch = "x86_64")]
    if caps().sse41 && n >= 16 {
        let bulk = n & !15;
        unsafe {
            x86_kernels::poler_edit_trit_or(a.as_ptr(), b.as_ptr(), out.as_mut_ptr(), bulk)
        };
        trit_or_scalar(&a[bulk..n], &b[bulk..n], &mut out[bulk..n]);
        return;
    }
    trit_or_scalar(&a[..n], &b[..n], &mut out[..n]);
}

pub fn trit_dot_scalar(u: &[i8], v: &[i8]) -> i64 {
    let n = u.len().min(v.len());
    let mut acc = 0i64;
    for i in 0..n {
        acc += (u[i] as i64) * (v[i] as i64);
    }
    acc
}

/// Скалярное произведение тритовых векторов ⟨u, v⟩ = Σ uᵢvᵢ без умножителя (`psignb`).
pub fn trit_dot(u: &[i8], v: &[i8]) -> i64 {
    let n = u.len().min(v.len());
    #[cfg(target_arch = "x86_64")]
    {
        let c = caps();
        if c.ssse3 && c.sse41 && n >= 16 {
            let bulk = n & !15;
            let head = unsafe { x86_kernels::poler_edit_trit_dot(u.as_ptr(), v.as_ptr(), bulk) };
            return head + trit_dot_scalar(&u[bulk..n], &v[bulk..n]);
        }
    }
    trit_dot_scalar(&u[..n], &v[..n])
}

/// Упаковка 5 тритов ∈ {-1, 0, +1} в 1 байт [0..242] (хвост < 5 дополняется нулями ⊙).
pub fn pack5_scalar(trits: &[i8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(trits.len().div_ceil(5));
    let mut i = 0;
    while i < trits.len() {
        let mut acc = 0u8;
        for k in 0..5 {
            let t = if i + k < trits.len() {
                clamp_trit(trits[i + k])
            } else {
                0
            };
            acc = acc * 3 + (t + 1) as u8;
        }
        out.push(acc);
        i += 5;
    }
}

pub fn pack5(trits: &[i8], out: &mut Vec<u8>) {
    out.clear();
    if trits.is_empty() {
        return;
    }
    let total_bytes = trits.len().div_ceil(5);
    out.resize(total_bytes, TRIT5_ZERO_CODE);
    let full_groups = trits.len() / 5;
    #[cfg(target_arch = "x86_64")]
    {
        if full_groups > 0 {
            unsafe {
                x86_kernels::poler_edit_trit_pack5(
                    trits.as_ptr(),
                    out.as_mut_ptr(),
                    full_groups * 5,
                );
            }
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        for g in 0..full_groups {
            let base = g * 5;
            let mut acc = 0u8;
            for k in 0..5 {
                acc = acc * 3 + (clamp_trit(trits[base + k]) + 1) as u8;
            }
            out[g] = acc;
        }
    }
    let rem_start = full_groups * 5;
    if rem_start < trits.len() {
        let mut acc = 0u8;
        for k in 0..5 {
            let t = if rem_start + k < trits.len() {
                clamp_trit(trits[rem_start + k])
            } else {
                0
            };
            acc = acc * 3 + (t + 1) as u8;
        }
        out[full_groups] = acc;
    }
}

pub fn unpack5_scalar(packed: &[u8], out: &mut Vec<i8>) {
    out.clear();
    out.resize(packed.len() * 5, 0);
    for (j, &b) in packed.iter().enumerate() {
        let mut v = b.min(242);
        for k in (0..5).rev() {
            let q = v / 3;
            let r = v - q * 3;
            out[j * 5 + k] = (r as i8) - 1;
            v = q;
        }
    }
}

pub fn unpack5(packed: &[u8], out: &mut Vec<i8>) {
    out.clear();
    if packed.is_empty() {
        return;
    }
    out.resize(packed.len() * 5, 0);
    #[cfg(target_arch = "x86_64")]
    {
        unsafe {
            x86_kernels::poler_edit_trit_unpack5(packed.as_ptr(), out.as_mut_ptr(), packed.len());
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        unpack5_scalar(packed, out);
    }
}

// =====================================================================
// ABS-ZERO: Проективная прямая ℝP¹ [N:D] и Q8-распад в Абсолютный Ноль
// =====================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjClass {
    Finite = 0,
    Zero = 1,
    Pole = 2,
    Gauge = 3,
}

impl ProjClass {
    #[inline]
    fn from_code(code: u64) -> Self {
        match code {
            1 => ProjClass::Zero,
            2 => ProjClass::Pole,
            3 => ProjClass::Gauge,
            _ => ProjClass::Finite,
        }
    }
}

/// Проективное отношение `[N:D]` без инструкций деления.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proj {
    pub n: i64,
    pub d: i64,
}

impl Proj {
    pub const ZERO: Proj = Proj { n: 0, d: 1 };
    pub const ONE: Proj = Proj { n: 1, d: 1 };
    pub const INF: Proj = Proj { n: 1, d: 0 };
    pub const GAUGE: Proj = Proj { n: 0, d: 0 };

    #[inline]
    pub const fn new(n: i64, d: i64) -> Self {
        Self { n, d }
    }

    #[inline]
    pub fn classify(&self) -> ProjClass {
        #[cfg(target_arch = "x86_64")]
        {
            ProjClass::from_code(unsafe { x86_kernels::poler_edit_az_class(self.n, self.d) })
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            classify_scalar(self.n, self.d)
        }
    }

    #[inline]
    pub fn inv(&self) -> Self {
        Proj { n: self.d, d: self.n }
    }

    pub fn canonicalize(&self) -> Self {
        match self.classify() {
            ProjClass::Gauge => Proj::GAUGE,
            ProjClass::Zero => Proj::ZERO,
            ProjClass::Pole => Proj::INF,
            ProjClass::Finite => {
                let mut n = self.n;
                let mut d = self.d;
                if d < 0 {
                    n = n.saturating_neg();
                    d = d.saturating_neg();
                }
                let g = gcd64(n.unsigned_abs(), d.unsigned_abs()) as i64;
                if g > 1 {
                    n /= g;
                    d /= g;
                }
                Proj { n, d }
            }
        }
    }
}

#[inline]
pub fn classify_scalar(n: i64, d: i64) -> ProjClass {
    let code = (n == 0) as u64 + 2 * ((d == 0) as u64);
    ProjClass::from_code(code)
}

/// Бинарный НОД Штейна на сдвигах и вычитаниях (без `div`).
pub fn gcd64(mut a: u64, mut b: u64) -> u64 {
    if a == 0 {
        return b;
    }
    if b == 0 {
        return a;
    }
    let za = a.trailing_zeros();
    let zb = b.trailing_zeros();
    a >>= za;
    b >>= zb;
    let shift = za.min(zb);
    loop {
        if a > b {
            std::mem::swap(&mut a, &mut b);
        }
        b -= a;
        if b == 0 {
            return a << shift;
        }
        b >>= b.trailing_zeros();
    }
}

#[inline]
pub fn cmp_proj_scalar(a: &Proj, b: &Proj) -> Ordering {
    // Обработка полюса d == 0: северный полюс [N>0 : 0] выше любого конечного.
    if a.d == 0 && b.d == 0 {
        return a.n.signum().cmp(&b.n.signum());
    }
    if a.d == 0 {
        return if a.n >= 0 { Ordering::Greater } else { Ordering::Less };
    }
    if b.d == 0 {
        return if b.n >= 0 { Ordering::Less } else { Ordering::Greater };
    }
    let (an, ad) = if a.d < 0 { (-a.n as i128, -a.d as i128) } else { (a.n as i128, a.d as i128) };
    let (bn, bd) = if b.d < 0 { (-b.n as i128, -b.d as i128) } else { (b.n as i128, b.d as i128) };
    (an * bd).cmp(&(bn * ad))
}

/// Точное сравнение двух проективных оценок `[N:D]` без плавающей точки и без деления.
#[inline]
pub fn cmp_proj(a: &Proj, b: &Proj) -> Ordering {
    #[cfg(target_arch = "x86_64")]
    if a.d > 0 && b.d > 0 {
        let r = unsafe { x86_kernels::poler_edit_az_cmp(a.n, a.d, b.n, b.d) };
        return r.cmp(&0);
    }
    cmp_proj_scalar(a, b)
}

/// Подсчёт числа вакуумных `pack5`-байтов `121` (⊙⊙⊙⊙⊙).
pub fn trit_zero_count_scalar(packed: &[u8]) -> usize {
    packed.iter().filter(|&&b| b == TRIT5_ZERO_CODE).count()
}

pub fn trit_zero_count(packed: &[u8]) -> usize {
    let n = packed.len();
    if n == 0 {
        return 0;
    }
    #[cfg(target_arch = "x86_64")]
    {
        let c = caps();
        if c.sse2 && c.popcnt && n >= 16 {
            let bulk = n & !15;
            let head =
                unsafe { x86_kernels::poler_edit_az_trit_zero_count(packed.as_ptr(), bulk) };
            return head + trit_zero_count_scalar(&packed[bulk..]);
        }
    }
    trit_zero_count_scalar(packed)
}

/// Доля вакуума в `[N:D]` (число нулевых pack5-байтов к общей длине).
#[inline]
pub fn trit_vacuum_ratio(packed: &[u8]) -> Proj {
    if packed.is_empty() {
        return Proj::ONE;
    }
    Proj::new(trit_zero_count(packed) as i64, packed.len() as i64)
}

/// Целочисленный Q8-распад памяти резонанса (`7/8 = 224/256`).
/// Достигает ТОЧНОГО абсолютного нуля `0` за конечное число шагов
/// (без вечного хвоста из 1074 субнормалей IEEE-754).
#[inline]
pub fn q8_decay(r: i64) -> i64 {
    if r == 0 {
        0
    } else if r > 0 {
        (r.saturating_mul(224)) >> 8
    } else {
        -(((-r).saturating_mul(224)) >> 8)
    }
}

// =====================================================================
// LENS No-Hits барьер контрактов (отсечение галлюцинаций и чужих скоупов)
// =====================================================================

/// Проективный барьер `LENS No-Hits`:
/// оставляет индексы `i`, где `scores[i] > min_score`, все биты `require`
/// выставлены и ни один бит `forbid` не присутствует.
pub fn lens_filter_proj(
    scores: &[Proj],
    flags: &[u64],
    min_score: Proj,
    require: u64,
    forbid: u64,
) -> Vec<usize> {
    let n = scores.len().min(flags.len());
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let f = flags[i];
        if (f & require) != require {
            continue;
        }
        if (f & forbid) != 0 {
            continue;
        }
        if cmp_proj(&scores[i], &min_score) == Ordering::Greater {
            out.push(i);
        }
    }
    out
}

// =====================================================================
// 256-тритная семантическая сигнатура скоупа (52 байта pack5 вместо 6 КБ f32)
// =====================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TritSig256 {
    pub packed: [u8; SIG_PACKED_BYTES],
    pub active: u16,
}

impl TritSig256 {
    pub const VACUUM: Self = Self {
        packed: [TRIT5_ZERO_CODE; SIG_PACKED_BYTES],
        active: 0,
    };

    /// Построить 256-тритный отпечаток текста скоупа/запроса (идентификаторы + биграммы токенов).
    /// Знак вклада ±1 определяется второй независимой компонентой хеша (SimHash в 𝕋²⁵⁶),
    /// а мёртвая зона `[-θ, +θ]` переводит слабый шум в вакуум `0` (⊙).
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut acc = [0i32; SIG_TRITS];
        let mut token_count = 0u32;
        let mut prev_hash: u64 = 0;

        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            let is_tok = b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80;
            if !is_tok {
                i += 1;
                continue;
            }
            let start = i;
            i += 1;
            while i < bytes.len() {
                let c = bytes[i];
                if c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80 {
                    i += 1;
                } else {
                    break;
                }
            }
            let tok = &bytes[start..i];
            if tok.len() < 2 {
                continue;
            }
            token_count += 1;
            // FNV-1a с регистронезависимым ASCII-свёртыванием + разбиение camelCase/snake_case.
            let mut h: u64 = 0xcbf29ce484222325;
            for &ch in tok {
                h ^= (ch.to_ascii_lowercase()) as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            Self::splat_hash(&mut acc, h, 2);
            if prev_hash != 0 {
                let bigram = prev_hash.rotate_left(13) ^ h.wrapping_mul(0x9e3779b97f4a7c15);
                Self::splat_hash(&mut acc, bigram, 1);
            }
            prev_hash = h;
        }

        if token_count == 0 {
            return Self::VACUUM;
        }

        // Тернарный спайк с мёртвой зоной θ: слабые координаты обнуляются в ⊙.
        let theta = ((token_count as i32).isqrt()).max(1);
        let mut trits = [0i8; SIG_PACKED_BYTES * 5];
        let mut active = 0u16;
        for idx in 0..SIG_TRITS {
            let v = acc[idx];
            if v > theta {
                trits[idx] = 1;
                active += 1;
            } else if v < -theta {
                trits[idx] = -1;
                active += 1;
            }
        }
        let mut vec_out = Vec::with_capacity(SIG_PACKED_BYTES);
        pack5(&trits, &mut vec_out);
        let mut packed = [TRIT5_ZERO_CODE; SIG_PACKED_BYTES];
        packed.copy_from_slice(&vec_out[..SIG_PACKED_BYTES]);
        Self { packed, active }
    }

    #[inline]
    fn splat_hash(acc: &mut [i32; SIG_TRITS], mut h: u64, weight: i32) {
        // 4 проекции на один токен (разреженный тернарный JL-проектор).
        for _ in 0..4 {
            h ^= h >> 30;
            h = h.wrapping_mul(0xbf58476d1ce4e5b9);
            let slot = (h as usize) & (SIG_TRITS - 1);
            let sign = if (h >> 8) & 1 == 0 { weight } else { -weight };
            acc[slot] += sign;
        }
    }

    pub fn unpack_trits(&self) -> [i8; SIG_TRITS] {
        let mut buf = Vec::with_capacity(SIG_PACKED_BYTES * 5);
        unpack5(&self.packed, &mut buf);
        let mut out = [0i8; SIG_TRITS];
        out.copy_from_slice(&buf[..SIG_TRITS]);
        out
    }

    /// Проективное косинусно-троичное сходство `[⟨u, v⟩ : max(1, active_u + active_v)]` без деления!
    pub fn similarity(&self, other: &Self) -> Proj {
        if self.active == 0 || other.active == 0 {
            return Proj::ZERO;
        }
        let u = self.unpack_trits();
        let v = other.unpack_trits();
        let dot = trit_dot(&u, &v);
        let den = (self.active as i64 + other.active as i64).max(1);
        Proj::new(dot, den)
    }
}

// =====================================================================
// Троичная 3-way алгебра слияния веток правок (Kleene Merge)
// =====================================================================

/// Слить два тритовых вектора изменений относительно общей базы:
/// * `-1` — удаление куска/строки оригинала,
/// * `0`  — не тронуто (вакуум),
/// * `+1` — вставка/модификация.
///
/// Если хотя бы одна ветка `== 0`, результат однозначен без конфликта.
/// Конфликт фиксируется только при `a[i] != 0 && b[i] != 0 && a[i] != b[i]`.
pub fn kleene_merge_trits(branch_a: &[i8], branch_b: &[i8]) -> (Vec<i8>, Vec<usize>) {
    let n = branch_a.len().min(branch_b.len());
    let mut merged = Vec::with_capacity(n);
    let mut conflicts = Vec::new();
    for i in 0..n {
        let a = clamp_trit(branch_a[i]);
        let b = clamp_trit(branch_b[i]);
        if a == 0 {
            merged.push(b);
        } else if b == 0 || a == b {
            merged.push(a);
        } else {
            // Прямое столкновение +1 vs -1 → аннигиляция в 0 + маркер конфликта
            merged.push(0);
            conflicts.push(i);
        }
    }
    (merged, conflicts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack5_all_243_roundtrip_and_unique_zero_121() {
        let mut zero_codes = Vec::new();
        for code in 0u8..243 {
            let mut trits = Vec::new();
            unpack5(&[code], &mut trits);
            assert_eq!(trits.len(), 5);
            assert!(trits.iter().all(|&t| (-1..=1).contains(&t)));
            if trits.iter().all(|&t| t == 0) {
                zero_codes.push(code);
            }
            let mut repacked = Vec::new();
            pack5(&trits, &mut repacked);
            assert_eq!(repacked, vec![code], "mismatch on pack5 code {code}");
        }
        assert_eq!(zero_codes, vec![TRIT5_ZERO_CODE]);
    }

    #[test]
    fn simd_trit_ops_match_scalar_bit_for_bit() {
        let mut u = Vec::with_capacity(137);
        let mut v = Vec::with_capacity(137);
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..137 {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            u.push(((s >> 40) % 3) as i8 - 1);
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            v.push(((s >> 40) % 3) as i8 - 1);
        }

        let mut neg_hw = vec![0i8; u.len()];
        let mut neg_sc = vec![0i8; u.len()];
        trit_neg(&u, &mut neg_hw);
        trit_neg_scalar(&u, &mut neg_sc);
        assert_eq!(neg_hw, neg_sc);

        let mut and_hw = vec![0i8; u.len()];
        let mut and_sc = vec![0i8; u.len()];
        trit_and(&u, &v, &mut and_hw);
        trit_and_scalar(&u, &v, &mut and_sc);
        assert_eq!(and_hw, and_sc);

        let mut or_hw = vec![0i8; u.len()];
        let mut or_sc = vec![0i8; u.len()];
        trit_or(&u, &v, &mut or_hw);
        trit_or_scalar(&u, &v, &mut or_sc);
        assert_eq!(or_hw, or_sc);

        assert_eq!(trit_dot(&u, &v), trit_dot_scalar(&u, &v));
    }

    #[test]
    fn abs_zero_projective_128bit_exact_where_f64_fails() {
        // В f64 числа 3_000_000_000_000_000_001 и 3_000_000_000_000_000_000 склеиваются!
        let a = Proj::new(3_000_000_000_000_000_001, 3_000_000_000_000_000_000);
        let b = Proj::ONE;
        assert_eq!(cmp_proj(&a, &b), Ordering::Greater);
        assert_eq!(Proj::ZERO.classify(), ProjClass::Zero);
        assert_eq!(Proj::INF.classify(), ProjClass::Pole);
        assert_eq!(Proj::GAUGE.classify(), ProjClass::Gauge);
        assert_eq!(Proj::ZERO.inv().classify(), ProjClass::Pole);
    }

    #[test]
    fn q8_decay_reaches_exact_zero_in_finite_steps() {
        let mut r = 1_000_000i64;
        let mut steps = 0;
        while r != 0 && steps < 200 {
            r = q8_decay(r);
            steps += 1;
        }
        assert_eq!(r, 0, "Q8-распад обязан умирать в точном 0 за конечные шаги");
        assert!(steps < 120);
    }

    #[test]
    fn vacuum_scan_sse2_popcnt_parity() {
        let mut buf = vec![TRIT5_ZERO_CODE; 99];
        buf[0] = 42;
        buf[17] = 0;
        buf[63] = 242;
        buf[98] = 12;
        assert_eq!(trit_zero_count(&buf), 95);
        assert_eq!(trit_zero_count(&buf), trit_zero_count_scalar(&buf));
    }

    #[test]
    fn trit_sig256_distinguishes_similar_from_unrelated_scopes() {
        let s1 = TritSig256::from_bytes(
            b"pub fn open_with_cap(path: &Path, cap: u64) -> io::Result<PolerBuffer> { Mmap::map(&file) }",
        );
        let s2 = TritSig256::from_bytes(
            b"fn reload_mmap_buffer(path: &Path) -> io::Result<PolerBuffer> { Mmap::map(&file) }",
        );
        let s3 = TritSig256::from_bytes(
            b"SELECT user_id, email FROM invoices WHERE paid_at IS NOT NULL ORDER BY total_usd DESC;",
        );
        let sim_related = s1.similarity(&s2);
        let sim_unrelated = s1.similarity(&s3);
        assert_eq!(cmp_proj(&sim_related, &sim_unrelated), Ordering::Greater);
    }

    #[test]
    fn kleene_merge_detects_only_true_collisions() {
        let a = [0i8, 1, 0, -1, 1];
        let b = [1i8, 0, -1, -1, -1];
        let (merged, conflicts) = kleene_merge_trits(&a, &b);
        assert_eq!(merged, vec![1, 1, -1, -1, 0]);
        assert_eq!(conflicts, vec![4]);
    }
}
