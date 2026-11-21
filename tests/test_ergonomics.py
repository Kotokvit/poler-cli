#!/usr/bin/env python3
"""Live ergonomics test suite for poler:
   1. --dry-run / --diff  (Zero-Disk-IO, unified diff format)
   2. --json              (ok/error_code, dry_run, structured output)
   3. exit codes 0-5      (SUCCESS, GENERAL_ERROR, USAGE, NOT_FOUND, AMBIGUOUS, SYNTAX_BROKEN)
"""
import json as jsonlib
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
    _temp_dir = tempfile.TemporaryDirectory(prefix="poler-erg-test-")
    SB = _temp_dir.name

def run(*args):
    r = subprocess.run([POLER, *args], capture_output=True, text=True, timeout=60, cwd=SB)
    return r.returncode, (r.stdout or "") + (r.stderr or "")

def make_zip(path, entries):
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        for n, d in entries:
            z.writestr(n, d)

results = []
def test(name, fn):
    try:
        ok, msg = fn()
    except Exception as e:
        ok, msg = False, f"EXC {type(e).__name__}: {e}"
    results.append((name, ok))
    print(f"{'PASS' if ok else 'FAIL'}  {name}: {msg}")

# ═══════════ 1. DRY-RUN ═══════════

def d1():
    p = f"{SB}/flat.txt"
    with open(p, "w") as f: f.write("line one\nline two\nline three\n")
    before = open(p, "rb").read()
    rc, out = run("patch", p, "line two", "LINE TWO CHANGED", "--dry-run")
    after = open(p, "rb").read()
    return (rc == 0 and before == after and "-line two" in out and "+LINE TWO CHANGED" in out,
            f"rc={rc}, file_unchanged={before == after}, has_minus={'-line two' in out}, has_plus={'+LINE TWO CHANGED' in out}")
test("D1 dry-run: file untouched", d1)

def d2():
    p = f"{SB}/flat.txt"
    rc, out = run("patch", p, "line two", "LINE TWO", "--diff")
    has_hdr = ("---" in out and "+++" in out)
    return (rc == 0 and has_hdr and "@@" in out,
            f"rc={rc}, headers={has_hdr}, hunk={'@@' in out}, head={out[:150]!r}")
test("D2 --diff: unified diff format", d2)

def d3():
    p = f"{SB}/arch.zip"
    make_zip(p, [("config.txt", "mode=production\n"), ("other.txt", "data\n")])
    before = open(p, "rb").read()
    rc, out = run("patch", p, "mode=production", "mode=staging", "--dry-run")
    after = open(p, "rb").read()
    intact = zipfile.ZipFile(p).testzip() is None
    return (rc == 0 and before == after and intact and "-mode=production" in out,
            f"rc={rc}, zip_bytes_unchanged={before == after}, testzip_ok={intact}, diff_has_old={('-mode=production' in out)}")
test("D3 dry-run on zip: container untouched", d3)

def d4():
    p = f"{SB}/doc.docx"
    make_zip(p, [("word/document.xml", "<w:t>OLD</w:t>"), ("[Content_Types].xml", "<Types/>")])
    before = open(p, "rb").read()
    rc, out = run("patch", f"{p}::word/document.xml", "OLD", "NEW", "--dry-run")
    has_hunk = "@@" in out and "-<w:t>OLD</w:t>" in out and "+<w:t>NEW</w:t>" in out
    return (rc == 0 and before == open(p, "rb").read() and has_hunk,
            f"rc={rc}, unchanged={before == open(p, 'rb').read()}, has_hunk={has_hunk}, out={out[:160]!r}")
test("D4 dry-run ::entry in docx", d4)

# ═══════════ 2. --json ═══════════

def j1():
    p = f"{SB}/ok.txt"
    with open(p, "w") as f: f.write("hello world\n")
    rc, out = run("patch", p, "hello", "hi", "--json")
    try:
        j = jsonlib.loads(out.strip().splitlines()[-1])
        return (rc == 0 and j.get("ok") is True and j.get("dry_run") is False and j.get("path") == p,
                f"rc={rc}, json={j}")
    except Exception as e:
        return (False, f"rc={rc}, not-json: {out[:150]!r} ({e})")
test("J1 --json successful patch", j1)

def j2():
    p = f"{SB}/ok.txt"
    rc, out = run("patch", p, "hello", "x", "--json")
    try:
        j = jsonlib.loads(out.strip().splitlines()[-1])
        return (rc == 3 and j.get("ok") is False and j.get("error_code") == "match_not_found",
                f"rc={rc}, json={j}")
    except Exception as e:
        return (False, f"rc={rc}, not-json: {out[:150]!r} ({e})")
test("J2 --json error + error_code", j2)

def j3():
    p = f"{SB}/dr.txt"
    with open(p, "w") as f: f.write("aaa bbb\n")
    rc, out = run("patch", p, "bbb", "ccc", "--dry-run", "--json")
    try:
        j = jsonlib.loads(out.strip().splitlines()[-1])
        has_diff = "diff" in j or "-bbb" in out
        return (rc == 0 and j.get("ok") is True and j.get("dry_run") is True and has_diff,
                f"rc={rc}, json={j}")
    except Exception as e:
        return (False, f"rc={rc}, not-json: {out[:200]!r} ({e})")
test("J3 --json + dry-run", j3)

def j4():
    p = f"{SB}/arch.zip"
    rc, out = run("view", p, "--json")
    try:
        j = jsonlib.loads(out.strip().splitlines()[-1])
        return (rc == 0 and ("entries" in j or "path" in j or j.get("ok") is True),
                f"rc={rc}, json_keys={list(j.keys())[:8]}")
    except Exception as e:
        return (False, f"rc={rc}, not-json: {out[:150]!r} ({e})")
test("J4 view --json on archive container", j4)

# ═══════════ 3. EXIT CODES 0-5 ═══════════

def e0():
    p = f"{SB}/ok.txt"
    rc, _ = run("patch", p, "hi", "hello", "--json")
    rc2, _ = run("patch", p, "hello", "hi", "--dry-run")
    return (rc == 0 and rc2 == 0, f"patch_rc={rc}, dry_rc={rc2}")
test("E0 rc=0 SUCCESS", e0)

def e1():
    rc, out = run("patch", "/nonexistent-dir-xyz/file.txt", "a", "b")
    return (rc in (1, 3), f"rc={rc}, out={out[:100]!r}")
test("E1 inaccessible path: rc in (1,3)", e1)

def e2():
    rc, out = run("patch", f"{SB}/ok.txt")
    return (rc == 2, f"rc={rc}, out={out[:80]!r}")
test("E2 rc=2 USAGE_ERROR", e2)

def e3():
    p = f"{SB}/ok.txt"
    rc, _ = run("patch", p, "NO_SUCH_TOKEN", "x")
    return (rc == 3, f"rc={rc}")
test("E3 rc=3 NOT_FOUND", e3)

def e4():
    p = f"{SB}/amb.txt"
    with open(p, "w") as f: f.write("dup A\ndup B\n")
    rc, out = run("patch", p, "dup", "uniq")
    return (rc == 4 and "ambiguous" in out.lower(),
            f"rc={rc}, out={out[:120]!r}")
test("E4 rc=4 AMBIGUOUS_MATCH", e4)

def e5():
    p = f"{SB}/code.rs"
    with open(p, "w") as f: f.write("fn main() {\n    let x = (1 + 2);\n}\n")
    before = open(p, "rb").read()
    rc, out = run("patch", p, "(1 + 2)", "1 + 2)")
    after = open(p, "rb").read()
    rolled_back = (before == after)
    return (rc == 5 and rolled_back and "trit" in out.lower(),
            f"rc={rc}, rolled_back={rolled_back}, out={out[:160]!r}")
test("E5 rc=5 SYNTAX_BROKEN + atomic rollback", e5)

def e5b():
    p = f"{SB}/code2.rs"
    with open(p, "w") as f: f.write("fn main() {\n    let x = (1 + 2);\n}\n")
    rc, out = run("patch", p, "1 + 2", "3")
    ok = (rc == 0 and "(3)" in open(p).read())
    return (ok, f"rc={rc}, content={open(p).read()!r}")
test("E5b control: balanced edit succeeds (rc=0)", e5b)

print("\n== SUMMARY ==")
passed = sum(1 for _, ok in results if ok)
print(f"{passed}/{len(results)} passed")
if _temp_dir:
    _temp_dir.cleanup()
sys.exit(0 if passed == len(results) else 1)
