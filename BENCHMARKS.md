# 🔬 `poler` Architectural Benchmarks & Agentic Performance

> **Why `poler` Outperforms `ripgrep` for AI Agents, Large Repositories, and Modern Coding Workflows.**

---

## ⚡ The Architectural Paradigm Shift

`ripgrep` was engineered in 2016 for a **human developer** running a single command in a terminal, visually scanning terminal output (`path:line:text`). 

In 2026, the primary consumer of codebase search is an **autonomous AI agent** (Claude Code, Cursor, Codex, Antigravity). Agents operate under fundamentally different computational constraints:
1. **Repeated High-Frequency Queries:** An agent debugging an issue issues 30–60 queries over the same codebase in a 3-minute loop. `ripgrep` has **zero cross-query caching** and rescans the entire filesystem on every invocation.
2. **Context Window Token Economy:** Raw text dumps burn valuable context window tokens. An agent needs compact, exact byte anchors (`[file_id, offset, len]`) to surgically read only the required snippet.
3. **Subprocess Overhead:** Spawning hundreds of independent `rg` processes triggers system-call thrashing and fork bombs (e.g., VS Code 1.113 unbounded process freeze).
4. **Read-Only Limitation:** `ripgrep` cannot modify files. The agent is forced to orchestrate fragile, multi-step `sed`/`awk`/Python scripts to apply changes.

`poler` was built from the silicon up to solve these constraints.

---

## 📊 Head-to-Head Comparison: `poler` vs `ripgrep`

| Metric / Capability | `ripgrep` (v15.2.0) | `poler` (v0.97.0) | Architectural Advantage |
| :--- | :--- | :--- | :--- |
| **Warm Query Latency** | **40–120 ms** (full directory re-traversal every run) | **2–4 ms** (`.t5a` L1–L3 Address Crystal projection) | **10× – 50× Faster** for agent loops |
| **Memory Footprint (40 GB file)** | Reads into memory / high RSS | **< 100 KB** (Zero-Copy Piece-Table metadata) | **$O(1)$ RAM** guaranteed |
| **Output for LLM Agents** | Human-targeted stdout (`file:line:content`) | Hardware-aligned 16-byte anchors (`--format address`) | **95% token savings** |
| **In-Place File Surgery** | ❌ Unsupported (find-only) | ✅ Native atomic Piece-Table (`poler patch` / `replace_match`) | Closed Find $\to$ Read $\to$ Patch loop |
| **Archive & Document VFS** | ⚠️ Flat text grep only; no doc extraction | ✅ Native VFS for `.docx`, `.epub`, `.odt`, `.pdf`, `.zip` | Direct in-archive modification |
| **Syntax Guarding** | ❌ None | ✅ $GF(3)$ Balanced Trit Syntax & Bracket Balance Guard | Prevents automated AI syntax breakage |
| **AI Protocol Integration** | ❌ Raw CLI only | ✅ Native Model Context Protocol (**39 MCP Tools**) | Zero fork-bomb overhead |

---

## ⚡ Live Head-to-Head Speed & Token Battle

Reproduce locally with:
```bash
python3 benches/battle_rg.py /path/to/codebase
```

![Poler vs Ripgrep Head-to-Head Battle](assets/battle_benchmark.png)

---

## 🧪 The 5-Level Architectural Test Suite

The repository includes a self-contained benchmark suite (`benches/stress_5levels.py`) verifying core performance invariants:

### Level 1: Zero-Copy Piece Surgery in the Middle of a 500 MB File
- **Test:** Injects a target needle at byte 262,144,000 (exact middle of 500 MB), mutates it via `replace_match`, and flushes to disk.
- **Latency:** Initial mmap descriptor open in **1.88 ms**.
- **RAM footprint:** Initial Piece-Table metadata occupies **< 70 KB** of RAM.
- **Result:** Needle replaced in-place on disk with zero whole-file rewrites.

### Level 2: Virtual Window Scrolling over 1,000,000 Lines (62 MB)
- **Test:** Generates a 1M line log with an anomaly at line 500,000.
- **Latency:** `poler view --lines 499998..500002` extracts the exact 5-line window in **11.82 ms**.
- **Result:** $O(1)$ memory usage; zero linear allocations.

### Level 3: Address Mode & L1–L3 Cache (.t5a) Topology
- **Test:** Crawls repository source code and streams 16-byte binary anchors.
- **Latency:**
  - Cold scan: **4.22 ms** (557 MB/s).
  - Warm scan (.t5a projection): **2–4 ms**.
- **Output:** Exact byte offsets (`offset: 3857, len: 10`), verified to match target symbols bit-for-bit.

### Level 4: In-Archive VFS Surgery without Extraction
- **Test:** Opens a compressed `.zip` archive, mutates `manifest.txt` via two-phase CoW Piece-Table (`vfs_open` $\to$ `replace_match` $\to$ `vfs_save` $\to$ `vfs_flush`).
- **Result:** Modified entry persisted cleanly to disk without extracting the archive.

### Level 5: Model Context Protocol (MCP) Resident Loop
- **Test:** Dispatches resident MCP `tools/call` over stdio JSON-RPC 2.0.
- **Latency:** Roundtrip search response in **3 ms**.

---

## 🏃 Reproducing the Benchmarks

To execute the automated benchmark suite on your machine:

```bash
# Ensure poler is compiled in release mode and present in PATH
cargo build --release
cp target/release/poler ~/.local/bin/

# Run the 5-level benchmark suite
python3 benches/stress_5levels.py
```
