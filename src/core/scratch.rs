//! RAM-полигон `vfs://ai_scratch/` — Кристаллический_scratchpad агента.
//!
//! ## Фаза 5 «MCP-глаза на VFS»
//!
//! Современные IDE не могут превратить оперативную память в бесконечный
//! полигон для целей самого ИИ: временные заметки, графы гипотез и
//! черновики патчей приходится выгружать на физический SSD, ломая
//! git status и дёргая вотчеры. Этот модуль — суверенное изолированное
//! пространство агента ЦЕЛИКОМ в RAM:
//!
//! * **Ноль байт на диске**: записи живут в content-addressed сторе
//!   чанков (BLAKE3 → байты), никаких временных файлов, git чист.
//! * **O(1) память за счёт дедупликации** (закон масштаба движка):
//!   нарезка FastCDC (`fastcdc.rs`, вырезана из poler-engine) режет
//!   контент границами, зависящими только от содержимого, — черновик,
//!   дублирующий источник на 90%, не занимает лишнюю RAM: общие чанки
//!   хранятся один раз и разделяются ссылками. База-источник
//!   регистрируется через [`ScratchPad::register_base`].
//! * **Мгновенный откат**: [`ScratchPad::discard`] / `discard_all`
//!   стирают черновик за микросекунды (референсы прочь, сиротские
//!   чанки освобождаются).
//! * **Атомарная материализация**: протоколный слой (`scratch_apply`)
//!   пересобирает запись из чанков и применяет её либо в CoW-оверлей
//!   архива (существующий `vfs_write` + `vfs_flush`), либо атомарно
//!   в реальный файл (tmp + fsync + rename) — см. [`atomic_write`].
//!
//! RAM-дисциплина: физический след полигона ограничен
//! [`SCRATCH_MAX_TOTAL_BYTES`] (256 МиБ), одна запись —
//! [`SCRATCH_MAX_ENTRY_BYTES`] (16 МиБ).

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::fastcdc::{chunk_hash, split_chunks, CdcParams};

/// Схема URI полигона (корень виртуального поддерева VFS).
pub const SCRATCH_SCHEME: &str = "vfs://ai_scratch/";
/// Максимальная длина имени записи (с подкаталогами).
pub const SCRATCH_MAX_NAME_LEN: usize = 128;
/// Лимит одной записи (RAM-дисциплина).
pub const SCRATCH_MAX_ENTRY_BYTES: u64 = 16 * 1024 * 1024;
/// Лимит физического следа всего полигона (только уникальные чанки).
pub const SCRATCH_MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

/// Полный URI записи полигона.
pub fn scratch_uri(name: &str) -> String {
    format!("{SCRATCH_SCHEME}{name}")
}

/// Является ли путь корнем/поддеревом полигона.
pub fn is_scratch_path(p: &str) -> bool {
    p == "vfs://ai_scratch" || p.starts_with(SCRATCH_SCHEME)
}

/// Нормализация имени: принимает `plan.md`, `/plan.md` и полный URI
/// `vfs://ai_scratch/plan.md`; возвращает каноническое относительное имя.
pub fn normalize_name(raw: &str) -> Result<String, String> {
    let mut name = raw.trim();
    if let Some(stripped) = name.strip_prefix(SCRATCH_SCHEME) {
        name = stripped;
    } else if name == "vfs://ai_scratch" {
        name = "";
    }
    let name = name.trim_start_matches('/');
    if name.is_empty() {
        return Err("пустое имя записи полигона".into());
    }
    if name.len() > SCRATCH_MAX_NAME_LEN {
        return Err(format!("имя длиннее {SCRATCH_MAX_NAME_LEN} байт"));
    }
    // Только печатные безопасные символы: без управляющих, без UTF-трюков.
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
    {
        return Err("имя: только [A-Za-z0-9._-/] (латиница, чтобы URI оставался портируемым)".into());
    }
    // Побег из песочницы запрещён.
    for seg in name.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(format!("имя содержит пустой/запретный сегмент: {seg:?}"));
        }
    }
    Ok(name.to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Физический чанк полигона: байты + счётчики ссылок.
struct ChunkSlot {
    data: Vec<u8>,
    /// Ссылки из записей (entry.refs).
    entry_refs: u32,
    /// Ссылки из зарегистрированных баз-источников (provenance).
    base_refs: u32,
}

/// Запись полигона: референсы чанков + метаданные.
pub struct ScratchEntry {
    /// BLAKE3-хеши чанков (порядок = порядок в контенте).
    pub refs: Vec<[u8; 32]>,
    /// Логический размер записи, байт.
    pub bytes: u64,
    /// Сколько чанков записи уже существовало в сторе на момент put
    /// (дедупликация: общие с базой/другими черновиками).
    pub shared_chunks: u64,
    /// URI источника, относительно которого черновик писался (для GUI).
    pub provenance: Option<String>,
    /// Время создания/обновления (epoch ms).
    pub created_ms: u64,
    pub updated_ms: u64,
}

/// Статистика одной операции put.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PutStats {
    pub bytes: u64,
    pub chunks: u64,
    /// Чанки, которых в сторе раньше не было (заняли физическую RAM).
    pub new_chunks: u64,
    /// Чанки, разделяемые с базой/другими записями.
    pub shared_chunks: u64,
    /// Приращение физического следа, байт (может быть отрицательным
    /// при замене записи).
    pub physical_delta: i64,
}

/// Сводка полигона для `scratch_list` / GUI-панели.
#[derive(Debug, Clone)]
pub struct ScratchSummary {
    pub name: String,
    pub uri: String,
    pub bytes: u64,
    pub chunks: u64,
    pub shared_chunks: u64,
    pub provenance: Option<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
}

/// Итоговая статистика полигона.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ScratchStats {
    pub entries: u64,
    /// Суммарный логический объём записей.
    pub logical_bytes: u64,
    /// Физический след (только уникальные чанки).
    pub physical_bytes: u64,
    /// Уникальных чанков в сторе.
    pub unique_chunks: u64,
    /// Всего референсов из записей.
    pub total_refs: u64,
}

impl ScratchStats {
    /// Коэффициент дедупликации: сколько логических байт приходится
    /// на один физический (> 1.0 — полигон экономит RAM).
    pub fn dedup_ratio(&self) -> f64 {
        if self.physical_bytes == 0 {
            return 1.0;
        }
        self.logical_bytes as f64 / self.physical_bytes as f64
    }
}

/// RAM-полигон агента.
pub struct ScratchPad {
    entries: BTreeMap<String, ScratchEntry>,
    chunks: HashMap<[u8; 32], ChunkSlot>,
    params: CdcParams,
    physical: u64,
}

impl Default for ScratchPad {
    fn default() -> Self {
        Self::new()
    }
}

impl ScratchPad {
    /// Полигон с полигонынм масштабом FastCDC: min 1 КиБ / avg 4 КиБ /
    /// max 16 КиБ — единицы дедупликации заметок и черновиков.
    pub fn new() -> Self {
        ScratchPad {
            entries: BTreeMap::new(),
            chunks: HashMap::new(),
            params: CdcParams::new(1024, 4096, 16 * 1024),
            physical: 0,
        }
    }

    /// Зарегистрировать базу-источник: контент (файл на диске или запись
    /// смонтированного архива) нарезается и кладётся в стор БЕЗ записи.
    /// Последующие `put` черновиков поверх этого источника разделяют
    /// с ним общие чанки — «черновик на 90% дублирует исходник и не жрёт
    /// лишнюю RAM». Базы живут до `discard_all`.
    pub fn register_base(&mut self, data: &[u8]) -> Result<u64, String> {
        if data.len() as u64 > SCRATCH_MAX_ENTRY_BYTES {
            return Err(format!(
                "база-источник {n} байт > ліміту {SCRATCH_MAX_ENTRY_BYTES}",
                n = data.len()
            ));
        }
        let mut registered = 0u64;
        for chunk in split_chunks(data, &self.params) {
            let h = chunk_hash(chunk);
            match self.chunks.get_mut(&h) {
                Some(slot) => slot.base_refs = slot.base_refs.saturating_add(1),
                None => {
                    if self.physical + chunk.len() as u64 > SCRATCH_MAX_TOTAL_BYTES {
                        return Err(format!(
                            "полигон переполнен: {p} + {n} > {SCRATCH_MAX_TOTAL_BYTES} байт",
                            p = self.physical,
                            n = chunk.len()
                        ));
                    }
                    self.physical += chunk.len() as u64;
                    self.chunks.insert(
                        h,
                        ChunkSlot {
                            data: chunk.to_vec(),
                            entry_refs: 0,
                            base_refs: 1,
                        },
                    );
                    registered += 1;
                }
            }
        }
        Ok(registered)
    }

    /// Сохранить/заменить запись. Возвращает статистику дедупликации.
    pub fn put(
        &mut self,
        raw_name: &str,
        data: &[u8],
        provenance: Option<&str>,
    ) -> Result<PutStats, String> {
        let name = normalize_name(raw_name)?;
        if data.len() as u64 > SCRATCH_MAX_ENTRY_BYTES {
            return Err(format!(
                "запись {n} байт > ліміту {SCRATCH_MAX_ENTRY_BYTES}",
                n = data.len()
            ));
        }
        let physical_before = self.physical;
        let created_before = self.entries.get(&name).map(|e| e.created_ms);
        // Предварительный расчёт: каких чанков в сторе ещё нет.
        let mut new_chunks = 0u64;
        let mut new_bytes = 0u64;
        for chunk in split_chunks(data, &self.params) {
            let h = chunk_hash(chunk);
            if !self.chunks.contains_key(&h) {
                new_chunks += 1;
                new_bytes += chunk.len() as u64;
            }
        }
        // Проверка ёмкости ДО любых мутаций: отклонённый put ничего
        // не ломает (звёздный аргумент — физика полигона инвариантна).
        if physical_before + new_bytes > SCRATCH_MAX_TOTAL_BYTES {
            return Err(format!(
                "полигон переполнен: {p} + {n} > {SCRATCH_MAX_TOTAL_BYTES} байт",
                p = physical_before,
                n = new_bytes
            ));
        }
        // Освободить референсы заменяемой записи (эксклюзивные чанки
        // уходят из стора; общие с новой записью — вернутся при вставке,
        // физический след тогда просто не меняется).
        let old = self.entries.remove(&name);
        if let Some(old) = &old {
            self.release_refs(&old.refs);
        }
        // Вставка.
        let mut refs = Vec::new();
        let mut shared_chunks = 0u64;
        for chunk in split_chunks(data, &self.params) {
            let h = chunk_hash(chunk);
            match self.chunks.get_mut(&h) {
                Some(slot) => {
                    slot.entry_refs = slot.entry_refs.saturating_add(1);
                    shared_chunks += 1;
                }
                None => {
                    self.physical += chunk.len() as u64;
                    self.chunks.insert(
                        h,
                        ChunkSlot {
                            data: chunk.to_vec(),
                            entry_refs: 1,
                            base_refs: 0,
                        },
                    );
                }
            }
            refs.push(h);
        }
        let now = now_ms();
        let provenance = provenance
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let bytes = data.len() as u64;
        let chunks = refs.len() as u64;
        self.entries.insert(
            name,
            ScratchEntry {
                refs,
                bytes,
                shared_chunks,
                provenance,
                created_ms: created_before.unwrap_or(now),
                updated_ms: now,
            },
        );
        Ok(PutStats {
            bytes,
            chunks,
            new_chunks,
            shared_chunks,
            physical_delta: self.physical as i64 - physical_before as i64,
        })
    }

    /// Уменьшить entry_refs; чанки без ссылок — освободить.
    fn release_refs(&mut self, refs: &[[u8; 32]]) {
        for h in refs {
            if let Some(slot) = self.chunks.get_mut(h) {
                slot.entry_refs = slot.entry_refs.saturating_sub(1);
                if slot.entry_refs == 0 && slot.base_refs == 0 {
                    self.physical -= slot.data.len() as u64;
                    self.chunks.remove(h);
                }
            }
        }
    }

    /// Пересобрать запись из чанков.
    pub fn get(&self, raw_name: &str) -> Result<Vec<u8>, String> {
        let name = normalize_name(raw_name)?;
        let entry = self
            .entries
            .get(&name)
            .ok_or_else(|| format!("в полигона нет записи {name:?}"))?;
        let mut out = Vec::with_capacity(entry.bytes as usize);
        for h in &entry.refs {
            match self.chunks.get(h) {
                Some(slot) => out.extend_from_slice(&slot.data),
                None => return Err(format!("чанк {} потерян — полигон повреждён", hex8(h))),
            }
        }
        Ok(out)
    }

    /// Существует ли запись.
    pub fn exists(&self, raw_name: &str) -> bool {
        normalize_name(raw_name).map(|n| self.entries.contains_key(&n)).unwrap_or(false)
    }

    /// Список записей (детерминированный порядок BTreeMap).
    pub fn list(&self) -> Vec<ScratchSummary> {
        self.entries
            .iter()
            .map(|(name, e)| ScratchSummary {
                uri: scratch_uri(name),
                name: name.clone(),
                bytes: e.bytes,
                chunks: e.refs.len() as u64,
                shared_chunks: e.shared_chunks,
                provenance: e.provenance.clone(),
                created_ms: e.created_ms,
                updated_ms: e.updated_ms,
            })
            .collect()
    }

    /// Стереть одну запись (микросекундный откат идеи).
    pub fn discard(&mut self, raw_name: &str) -> Result<bool, String> {
        let name = normalize_name(raw_name)?;
        match self.entries.remove(&name) {
            Some(entry) => {
                self.release_refs(&entry.refs);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Стереть весь полигон (базы-источники тоже).
    pub fn discard_all(&mut self) -> usize {
        let n = self.entries.len();
        self.entries.clear();
        self.chunks.clear();
        self.physical = 0;
        n
    }

    /// Сводная статистика полигона.
    pub fn stats(&self) -> ScratchStats {
        ScratchStats {
            entries: self.entries.len() as u64,
            logical_bytes: self.entries.values().map(|e| e.bytes).sum(),
            physical_bytes: self.physical,
            unique_chunks: self.chunks.len() as u64,
            total_refs: self.entries.values().map(|e| e.refs.len() as u64).sum(),
        }
    }
}

fn hex8(h: &[u8; 32]) -> String {
    h.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Атомарная запись файла: tmp + fsync + rename (тот же контракт, что у
/// материализации .poler в `vfs_edit::flush_archive`). Проматериализовать
/// идею из полигона — значит либо увидеть её целиком на диске, либо не
/// увидеть вовсе; частичных файлов не бывает.
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<(), String> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("нет каталога для {path:?}"))?;
    let tmp = dir.join(format!(
        ".{}.poler-scratch-{}",
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "entry".into()),
        std::process::id()
    ));
    let write_one = |p: &Path| -> Result<(), String> {
        let mut f = std::fs::File::create(p).map_err(|e| format!("создание {p:?}: {e}"))?;
        f.write_all(data).map_err(|e| format!("запись {p:?}: {e}"))?;
        f.sync_all().map_err(|e| format!("fsync {p:?}: {e}"))?;
        Ok(())
    };
    write_one(&tmp)?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename {tmp:?} → {path:?}: {e}")
    })?;
    // fsync каталога (Unix): чтобы сама ссылка на новый файл пережила крах.
    #[cfg(unix)]
    {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SplitMix64-псевдослучайные байты (как у тестов движка):
    /// реальный контент не периодичен — чанки внутри записи различны.
    fn pseudo(len: usize, seed: u64) -> Vec<u8> {
        let mut st = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut v = Vec::with_capacity(len);
        while v.len() < len {
            st = st.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = st;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            v.extend_from_slice(&z.to_le_bytes());
        }
        v.truncate(len);
        v
    }

    #[test]
    fn names_normalize_and_reject_escapes() {
        assert_eq!(normalize_name("plan.md").unwrap(), "plan.md");
        assert_eq!(normalize_name("/plan.md").unwrap(), "plan.md");
        assert_eq!(
            normalize_name("vfs://ai_scratch/plan.md").unwrap(),
            "plan.md"
        );
        assert_eq!(
            normalize_name("vfs://ai_scratch/hypo/graph.md").unwrap(),
            "hypo/graph.md"
        );
        assert_eq!(
            normalize_name("vfs://ai_scratch").is_err(),
            true,
            "корень схемы — не имя записи"
        );
        assert!(normalize_name("").is_err());
        assert!(normalize_name("../escape.md").is_err(), "побег из песочницы");
        assert!(normalize_name("a/../b.md").is_err());
        assert!(normalize_name("план.md").is_err(), "только латиница/цифры/._-/");
        assert!(normalize_name(&"x".repeat(129)).is_err());
        assert!(normalize_name("a//b.md").is_err(), "пустой сегмент");
        assert!(normalize_name("a/./b.md").is_err());
    }

    #[test]
    fn put_get_roundtrip_and_replace() {
        let mut sp = ScratchPad::new();
        let data = pseudo(50_000, 7);
        let st = sp.put("draft.md", &data, None).unwrap();
        assert_eq!(st.bytes, 50_000);
        assert!(st.chunks >= 1);
        assert_eq!(sp.get("draft.md").unwrap(), data);
        assert_eq!(sp.get("vfs://ai_scratch/draft.md").unwrap(), data);
        // Замена: created сохраняется, updated растёт.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let data2 = pseudo(60_000, 9);
        sp.put("draft.md", &data2, None).unwrap();
        assert_eq!(sp.get("draft.md").unwrap(), data2);
        let l = &sp.list()[0];
        assert!(l.updated_ms >= l.created_ms);
        assert_eq!(sp.stats().entries, 1);
    }

    #[test]
    fn dedup_shares_chunks_between_drafts() {
        let mut sp = ScratchPad::new();
        let base = pseudo(200_000, 3);
        // Черновик = база с дописанным хвостом: content-defined границы
        // не двигаются до ресинхронизации — общий префикс режется теми же
        // чанками, новыми становятся только приграничные и хвостовые.
        let mut draft = base.clone();
        draft.extend_from_slice(&pseudo(3_000, 11));
        let st_base = sp.put("base.md", &base, None).unwrap();
        assert_eq!(st_base.shared_chunks, 0, "первая запись — всё новое");
        let st_draft = sp.put("draft.md", &draft, None).unwrap();
        assert!(
            st_draft.shared_chunks > 0,
            "черновик обязан делить чанки с базой (shared={})",
            st_draft.shared_chunks
        );
        // Логически два документа, физически — меньше суммы.
        let s = sp.stats();
        assert!(s.logical_bytes > s.physical_bytes, "дедуп экономит RAM");
        assert!(s.dedup_ratio() > 1.0);
        // Обе записи собираются бит-в-бит.
        assert_eq!(sp.get("base.md").unwrap(), base);
        assert_eq!(sp.get("draft.md").unwrap(), draft);
    }

    #[test]
    fn provenance_base_is_free_for_draft() {
        let mut sp = ScratchPad::new();
        let source = pseudo(300_000, 5);
        // Источник регистрируется как база (не занимает записей).
        let registered = sp.register_base(&source).unwrap();
        assert!(registered > 0);
        assert_eq!(sp.stats().entries, 0, "база не создаёт записей");
        // Черновик «на ~90% дублирует исходник»: та же правка в конце.
        let mut draft = source.clone();
        draft.extend_from_slice(&pseudo(2_000, 13));
        let st = sp.put("refactor.md", &draft, Some("file:///src/main.rs")).unwrap();
        assert!(
            st.shared_chunks as f64 / st.chunks as f64 > 0.85,
            "большинство чанков черновика разделяется с базой ({}/{})",
            st.shared_chunks,
            st.chunks
        );
        assert!(
            (1..=4).contains(&st.new_chunks),
            "новые чанки — только граница источника и хвост ({}), а не весь файл",
            st.new_chunks
        );
        assert_eq!(sp.get("refactor.md").unwrap(), draft);
        // provenance сохранён для GUI.
        assert_eq!(sp.list()[0].provenance.as_deref(), Some("file:///src/main.rs"));
    }

    #[test]
    fn discard_frees_physical_ram() {
        let mut sp = ScratchPad::new();
        let a = pseudo(100_000, 1);
        let b = pseudo(100_000, 2);
        sp.put("a.md", &a, None).unwrap();
        sp.put("b.md", &b, None).unwrap();
        let s1 = sp.stats();
        assert!(s1.physical_bytes > 0);
        assert!(sp.discard("a.md").unwrap());
        assert!(!sp.discard("a.md").unwrap(), "второй discard — уже false");
        let s2 = sp.stats();
        assert!(s2.physical_bytes < s1.physical_bytes, "RAM освобождена");
        assert_eq!(sp.get("b.md").unwrap(), b);
        assert!(sp.get("a.md").is_err());
        let n = sp.discard_all();
        assert_eq!(n, 1);
        assert_eq!(sp.stats().physical_bytes, 0, "полигон пуст — след нулевой");
    }

    #[test]
    fn size_limits_enforced() {
        let mut sp = ScratchPad::new();
        let huge = vec![0u8; SCRATCH_MAX_ENTRY_BYTES as usize + 1];
        assert!(sp.put("huge.md", &huge, None).is_err());
        assert!(sp.register_base(&huge).is_err());
    }

    #[test]
    fn atomic_write_produces_exact_file() {
        let dir = std::env::temp_dir().join(format!("poler-scratch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("materialized.md");
        let payload = pseudo(50_000, 21);
        atomic_write(&path, &payload).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), payload);
        // Повторная запись — замена, не конкатенация.
        let payload2 = pseudo(10_000, 22);
        atomic_write(&path, &payload2).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), payload2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_is_deterministic_and_scoped() {
        let mut sp = ScratchPad::new();
        sp.put("z.md", b"z", None).unwrap();
        sp.put("a/hypo.md", b"a", None).unwrap();
        sp.put("m.md", b"m", None).unwrap();
        let names: Vec<String> = sp.list().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["a/hypo.md", "m.md", "z.md"], "BTree-порядок");
        for s in sp.list() {
            assert!(s.uri.starts_with(SCRATCH_SCHEME));
        }
    }
}
