import json, re, pathlib, subprocess, sys

B = pathlib.Path("/tmp/rapid-qwen-bench")

def qwen_result_text(d):
    data = json.loads((d / "stdout.json").read_text())
    return next(ev.get("result", "") for ev in data if ev.get("type") == "result")

def qwen_files(d, ext):
    return sorted(d.glob(f"*.{ext}"))

def fences(text):
    return re.findall(r"```[a-zA-Z]*\n(.*?)```", text, re.S)

def rapid_source(d, ext):
    text = (d / "stdout.txt").read_text()
    blocks = fences(text)
    if blocks:
        return max(blocks, key=len).strip() + "\n"
    return text.strip() + "\n"

def qwen_source(d, ext):
    made = qwen_files(d, ext)
    if made:
        return "FILE:" + str(made[0].resolve())
    text = qwen_result_text(d)
    blocks = fences(text)
    if blocks:
        return max(blocks, key=len).strip() + "\n"
    return text.strip() + "\n"

def materialize(tag, src, name):
    if src.startswith("FILE:"):
        return pathlib.Path(src[5:])
    out = B / f"eval/{tag}_{name}"
    out.write_text(src)
    return out

def run(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, timeout=60, **kw)

results = []
def record(name, ok, detail):
    results.append((name, "PASS" if ok else "FAIL", detail))

# ---- p1: balanced brackets (rust)
expected = ["true", "false", "true", "false"]
for cli in ["rapid", "qwen"]:
    d = B / "p1" / cli
    src = materialize(f"p1{cli}", rapid_source(d, "rs") if cli == "rapid" else qwen_source(d, "rs"), "is_balanced.rs")
    c = run(["rustc", "--edition", "2021", "-D", "warnings", "-o", f"/tmp/rapid-qwen-bench/eval/p1{cli}_bin", str(src)])
    if c.returncode != 0:
        record(f"p1 {cli}", False, f"compile failed: {c.stderr.strip()[:300]}")
        continue
    r = run([f"/tmp/rapid-qwen-bench/eval/p1{cli}_bin"])
    got = r.stdout.split()
    record(f"p1 {cli}", got == expected, f"output={got}")

# ---- p2: binary search fix (python)
expected = ["3", "0", "5", "-1"]
for cli in ["rapid", "qwen"]:
    d = B / "p2" / cli
    src = materialize(f"p2{cli}", rapid_source(d, "py") if cli == "rapid" else qwen_source(d, "py"), "fix.py")
    r = run(["python3", str(src)])
    got = r.stdout.split()
    record(f"p2 {cli}", got == expected, f"output={got} stderr={r.stderr.strip()[:150]}")

# ---- p4: parse_port (rust)
for cli in ["rapid", "qwen"]:
    d = B / "p4" / cli
    src = materialize(f"p4{cli}", rapid_source(d, "rs") if cli == "rapid" else qwen_source(d, "rs"), "parse_port.rs")
    c = run(["rustc", "--edition", "2021", "-D", "warnings", "-o", f"/tmp/rapid-qwen-bench/eval/p4{cli}_bin", str(src)])
    if c.returncode != 0:
        record(f"p4 {cli}", False, f"compile failed: {c.stderr.strip()[:300]}")
        continue
    r = run([f"/tmp/rapid-qwen-bench/eval/p4{cli}_bin"])
    record(f"p4 {cli}", r.returncode == 0, "stdout:\n" + r.stdout.strip())

# ---- p3: explanation text (no compile) - dump both for review
for cli in ["rapid", "qwen"]:
    text = (B / "p3" / cli / "stdout.txt").read_text() if cli == "rapid" else qwen_result_text(B / "p3" / cli)
    (B / "eval" / f"p3{cli}.txt").write_text(text)

for name, status, detail in results:
    print(f"{status}  {name}: {detail}")
