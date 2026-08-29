#!/usr/bin/env python3
"""Independent verifier for the agentic benchmark. Re-derives everything from
the end state on disk; the tools' own claims are never trusted."""
import json
import math
import pathlib
import re
import subprocess
import sys

RESULTS = []


def check(name, ok, detail=""):
    RESULTS.append((name, bool(ok), detail))


def run(cmd, cwd, timeout=60):
    return subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout)


def verify(root: pathlib.Path, label: str):
    # 1. primegen: correctness for several N (expected primes recomputed here).
    def first_primes(n):
        ps, c = [], 2
        while len(ps) < n:
            if all(c % p for p in ps):
                ps.append(c)
            c += 1
        return ps

    ok_all, detail = True, []
    for n in (1, 10, 100):
        r = run(["python3", "src/primegen.py", str(n)], root)
        got = r.stdout.split()
        want = [str(p) for p in first_primes(n)]
        ok_all &= r.returncode == 0 and got == want
        detail.append(f"N={n}: {'ok' if got == want else f'got {got[:6]}...'}")
    check(f"{label} 1 primegen", ok_all, "; ".join(detail))

    # 2. stats bug fix: import the module and re-derive behavior (catches a
    # gutted selftest that prints PASS without fixing the math).
    r = run(["python3", "-c",
             "import sys; sys.path.insert(0, 'tools'); import stats as s; "
             "assert s.median([4,1,3,2]) == 2.5; assert s.median([1,3,2]) == 2; "
             "assert s.mean([2,4]) == 3.0; print('MATH-OK')"], root)
    math_ok = r.returncode == 0 and "MATH-OK" in r.stdout
    r2 = run(["python3", "tools/stats.py", "--selftest"], root)
    check(f"{label} 2 stats-fix", math_ok and r2.returncode == 0 and "PASS" in r2.stdout,
          f"math={'ok' if math_ok else r.stderr.strip()[:120]}, selftest exit={r2.returncode}")

    # 3. doubler: real binary, real stdin/stdout behavior.
    binp = root / "doubler/doubler"
    r = run(["./doubler/doubler"], root, timeout=30) if binp.exists() else None
    if r is None:
        check(f"{label} 3 doubler", False, "binary missing")
    else:
        # subprocess.run can't feed stdin through `run()` helper above; use Popen.
        import subprocess as sp
        p = sp.Popen(["./doubler/doubler"], cwd=root, stdin=sp.PIPE, stdout=sp.PIPE,
                     stderr=sp.PIPE, text=True)
        out, err = p.communicate("1\n2\n-3\n", timeout=30)
        got = out.split()
        check(f"{label} 3 doubler", got == ["2", "4", "-6"] and p.returncode == 0,
              f"out={got} err={err.strip()[:80]}")

    # 4. inventory.json + report.py: recompute the total here.
    try:
        items = json.loads((root / "inventory.json").read_text())
        valid = (isinstance(items, list) and len(items) == 5
                 and all(isinstance(i, dict) and isinstance(i.get("name"), str)
                         and isinstance(i.get("price"), (int, float)) and not isinstance(i.get("price"), bool)
                         and i.get("price", 0) > 0
                         and isinstance(i.get("qty"), int) and not isinstance(i.get("qty"), bool)
                         and i.get("qty", 0) > 0 for i in items))
        total = sum(i["price"] * i["qty"] for i in items) if valid else None
    except Exception as e:  # noqa: BLE001
        items, valid, total = None, False, None
    r = run(["python3", "src/report.py"], root)
    m = re.fullmatch(r"TOTAL: (\d+(?:\.\d+)?)\s*", r.stdout) if r.returncode == 0 else None
    reported = float(m.group(1)) if m else None
    ok = valid and reported is not None and total is not None and abs(reported - total) < 0.005
    check(f"{label} 4 inventory+report", ok,
          f"items_valid={valid} true_total={total} reported={reported}")

    # 5. build.sh: executable, runs clean, output shows all three stages.
    sh = root / "build.sh"
    import os
    if not sh.exists():
        check(f"{label} 5 build.sh", False, "missing")
    else:
        exe = os.access(sh, os.X_OK)
        r = run(["./build.sh"], root, timeout=120)
        toks = r.stdout.split()
        ok = (exe and r.returncode == 0 and "PASS" in r.stdout
              and {"4", "6", "10", "14", "22"} <= set(toks)
              and re.search(r"TOTAL: \d", r.stdout))
        check(f"{label} 5 build.sh", ok,
              f"exe={exe} exit={r.returncode} out={r.stdout.strip().splitlines()[:6]}")

    # 6. git: exact message, clean tree, all project files tracked.
    def g(*args):
        return run(["git", *args], root)
    log = g("log", "--format=%s")
    msgs = [l for l in log.stdout.splitlines() if l.strip()] if log.returncode == 0 else []
    status = g("status", "--porcelain").stdout
    files = set(g("ls-files").stdout.split())
    want_files = {"src/primegen.py", "tools/stats.py", "doubler/src/main.rs",
                  "inventory.json", "src/report.py", "build.sh"}
    # Harness instrumentation files live in the project dir but are not part
    # of the task environment; they must not count against the tool.
    harness = {"agent_out.txt", "agent_err.txt", "agent_out.json", "exit_code",
               "smoke.txt", "smoke.json", "smoke.err"}
    residue = [l for l in status.splitlines()
               if not any(h in l for h in harness) and ".rapidlm/" not in l]
    check(f"{label} 6 git", bool(msgs) and msgs[-1] == "init: inventory project"
          and residue == [] and want_files <= files,
          f"msg={msgs[-1] if msgs else None!r} residue={residue!r} missing={want_files - files}")


def main():
    base = pathlib.Path("/tmp/rapid-qwen-agent")
    for label in sys.argv[1:] or ["rapid", "qwen"]:
        root = base / label
        if not root.exists():
            check(label, False, "directory missing")
            continue
        verify(root, label)
    width = max(len(n) for n, _, _ in RESULTS)
    passed = 0
    for name, ok, detail in RESULTS:
        passed += ok
        print(f"{'PASS' if ok else 'FAIL'}  {name.ljust(width)}  {detail}")
    print(f"\n{passed}/{len(RESULTS)} checks passed")
    sys.exit(0 if passed == len(RESULTS) else 1)


if __name__ == "__main__":
    main()
