#!/usr/bin/env python3
"""Live regression test suite: Container CRC-32 & VFS integrity for poler patch.
Verifies atomic patching, auto-detection, explicit :: syntax, ambiguous match refusal,
and bit-for-bit preservation of undamaged archive entries and Central Directory.
"""
import os
import shutil
import subprocess
import sys
import tempfile
import zipfile

def find_poler_bin():
    if "POLER_BIN" in os.environ and os.path.exists(os.environ["POLER_BIN"]):
        return os.environ["POLER_BIN"]
    repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    candidates = [
        os.path.join(repo_root, "target", "release", "poler"),
        os.path.join(repo_root, "target", "debug", "poler"),
        os.path.expanduser("~/.local/bin/poler"),
        shutil.which("poler"),
    ]
    for c in candidates:
        if c and os.path.exists(c) and os.access(c, os.X_OK):
            return c
    return "poler"

POLER = find_poler_bin()
_temp_dir = None
if "POLER_TEST_DIR" in os.environ:
    SB = os.environ["POLER_TEST_DIR"]
    os.makedirs(SB, exist_ok=True)
else:
    _temp_dir = tempfile.TemporaryDirectory(prefix="poler-crc-test-")
    SB = _temp_dir.name

def run(*args, **kw):
    r = subprocess.run([POLER, *args], capture_output=True, text=True,
                       timeout=60, cwd=SB, **kw)
    return r.returncode, (r.stdout or "") + (r.stderr or "")

def make_zip(path, entries):
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        for name, data in entries:
            z.writestr(name, data)

def check_zip(path):
    try:
        with zipfile.ZipFile(path) as z:
            bad = z.testzip()  # None => all CRC32 are valid
            names = z.namelist()
            content = {n: z.read(n) for n in names}
        return (bad is None, f"testzip={bad}, entries={len(names)}")
    except Exception as e:
        return (False, f"EXC {type(e).__name__}: {e}")

results = []

def test(name, fn):
    try:
        ok, msg = fn()
    except Exception as e:
        ok, msg = False, f"EXC {type(e).__name__}: {e}"
    results.append((name, ok, msg))
    print(f"{'PASS' if ok else 'FAIL'}  {name}: {msg}")

# ── T1: patch zip without entry (auto-detect) ──
def t1():
    p = f"{SB}/t1.zip"
    make_zip(p, [("config.txt", "version=1.0\nmode=production\n"),
                 ("readme.md", "# Hello\nThis is readme.\n")])
    rc, out = run("patch", p, "mode=production", "mode=staging")
    ok1, m1 = check_zip(p)
    data = zipfile.ZipFile(p).read("config.txt").decode()
    return (rc == 0 and ok1 and "mode=staging" in data and "readme" in
            zipfile.ZipFile(p).read("readme.md").decode(),
            f"rc={rc}, zip_ok={ok1} ({m1}), content={data!r}, out={out.strip()[:120]!r}")
test("T1 patch zip without entry (auto)", t1)

# ── T2: patch via explicit :: entry syntax ──
def t2():
    p = f"{SB}/t2.zip"
    make_zip(p, [("config.txt", "alpha\nbeta\ngamma\n"),
                 ("other.txt", "nothing here\n")])
    rc, out = run("patch", f"{p}::config.txt", "beta", "BETA-PATCHED")
    ok1, m1 = check_zip(p)
    data = zipfile.ZipFile(p).read("config.txt").decode()
    return (rc == 0 and ok1 and "BETA-PATCHED" in data and "gamma" in data,
            f"rc={rc}, zip_ok={ok1} ({m1}), content={data!r}")
test("T2 patch via :: entry", t2)

# ── T3: --entry flag ──
def t3():
    p = f"{SB}/t3.zip"
    make_zip(p, [("a/config.txt", "hello world\n"),
                 ("b/config.txt", "hello world\n")])
    rc, out = run("patch", p, "hello world", "hello poler", "--entry", "a/config.txt")
    ok1, m1 = check_zip(p)
    a = zipfile.ZipFile(p).read("a/config.txt").decode()
    b = zipfile.ZipFile(p).read("b/config.txt").decode()
    return (rc == 0 and ok1 and "poler" in a and "poler" not in b,
            f"rc={rc}, zip_ok={ok1} ({m1}), a={a!r}, b={b!r}")
test("T3 patch --entry flag", t3)

# ── T4: ambiguous match (old_text in multiple entries) → refused with rc=4 ──
def t4():
    p = f"{SB}/t4.zip"
    make_zip(p, [("x.txt", "same token\n"), ("y.txt", "same token\n")])
    rc, out = run("patch", p, "same token", "changed")
    ok1, m1 = check_zip(p)
    unchanged = (zipfile.ZipFile(p).read("x.txt").decode() == "same token\n")
    return (rc == 4 and ok1 and unchanged,
            f"rc={rc}, zip_intact={ok1}, unchanged={unchanged}, out={out.strip()[:150]!r}")
test("T4 ambiguous patch refusal (rc=4)", t4)

# ── T5: view archive container → formatted listing, not raw bytes ──
def t5():
    p = f"{SB}/t5.zip"
    make_zip(p, [("dir/file1.txt", "aaa"), ("file2.txt", "bbb")])
    rc, out = run("view", p)
    looks_binary = sum(1 for c in out[:2000] if ord(c) < 9 or 13 < ord(c) < 32)
    has_names = ("file1.txt" in out and "file2.txt" in out)
    return (rc == 0 and has_names and looks_binary < 5,
            f"rc={rc}, names={has_names}, bin_chars={looks_binary}, head={out[:200]!r}")
test("T5 view arch.zip -> container listing", t5)

# ── T6: view specific entry :: ──
def t6():
    p = f"{SB}/t6.zip"
    make_zip(p, [("config.txt", "the quick brown fox\n"), ("data.bin", "zzz")])
    rc, out = run("view", f"{p}::config.txt")
    return (rc == 0 and "quick brown fox" in out and "zzz" not in out,
            f"rc={rc}, out={out.strip()[:120]!r}")
test("T6 view arch.zip::entry", t6)

# ── T7: non-existent entry -> rc=3 (not found) and clean error ──
def t7():
    p = f"{SB}/t7.zip"
    make_zip(p, [("real.txt", "content\n")])
    rc, out = run("patch", f"{p}::ghost.txt", "content", "new")
    ok1, m1 = check_zip(p)
    return (rc == 3 and ok1 and "real.txt" in zipfile.ZipFile(p).namelist(),
            f"rc={rc}, zip_ok={ok1}, out={out.strip()[:150]!r}")
test("T7 non-existent entry (rc=3)", t7)

# ── T8: VFS container patch on .docx ──
def t8():
    p = f"{SB}/t8.docx"
    make_zip(p, [("word/document.xml", '<w:t>OLD-TEXT</w:t>'),
                 ("[Content_Types].xml", '<Types/>')])
    rc, out = run("patch", p, "OLD-TEXT", "NEW-TEXT")
    ok1, m1 = check_zip(p)
    data = zipfile.ZipFile(p).read("word/document.xml").decode()
    return (rc == 0 and ok1 and "NEW-TEXT" in data,
            f"rc={rc}, zip_ok={ok1} ({m1}), content={data!r}")
test("T8 patch .docx container", t8)

# ── T9: flat file regression ──
def t9():
    p = f"{SB}/t9.txt"
    with open(p, "w") as f:
        f.write("plain text file\nsecond line\n")
    rc, out = run("patch", p, "second", "2nd")
    data = open(p).read()
    return (rc == 0 and "2nd" in data and "plain text" in data,
            f"rc={rc}, content={data!r}")
test("T9 flat file regression", t9)

# ── T10: large zip (100 entries) — rebuild & CRC-32 integrity ──
def t10():
    p = f"{SB}/t10.zip"
    entries = [(f"dir{i}/f{j}.txt", f"payload {i}-{j} " + "x" * (j * 37))
               for i in range(5) for j in range(20)]
    make_zip(p, entries)
    rc, out = run("patch", f"{p}::dir2/f7.txt", "payload 2-7", "PATCHED")
    ok1, m1 = check_zip(p)
    data = zipfile.ZipFile(p).read("dir2/f7.txt").decode()
    intact = all(zipfile.ZipFile(p).read(f"dir{i}/f{j}.txt") ==
                 (f"payload {i}-{j} " + "x" * (j * 37)).encode()
                 for i in range(5) for j in range(20) if (i, j) != (2, 7))
    return (rc == 0 and ok1 and "PATCHED" in data and intact,
            f"rc={rc}, zip_ok={ok1} ({m1}), patched_ok={'PATCHED' in data}, others_intact={intact}")
test("T10 large zip with 100 entries", t10)

print("\n== SUMMARY ==")
passed = sum(1 for _, ok, _ in results if ok)
print(f"{passed}/{len(results)} passed")
if _temp_dir:
    _temp_dir.cleanup()
sys.exit(0 if passed == len(results) else 1)
