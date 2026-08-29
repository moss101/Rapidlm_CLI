#!/usr/bin/env python3
"""Summarize one benchmark run: wall time, exit code, tokens per (prompt, cli).

rapid: token total from the `tokens used: N` line on stderr (time_err.txt).
qwen:  tokens from the `result` event's usage in stdout.json.
"""
import json
import pathlib
import re
import sys

B = pathlib.Path("/tmp/rapid-qwen-bench")
RUNS = sys.argv[1:] or ["probe", "p1", "p2", "p3", "p4"]


def qwen_usage(path):
    data = json.loads(path.read_text())
    for ev in data:
        if ev.get("type") == "result":
            u = ev.get("usage", {})
            return (u.get("input_tokens"), u.get("output_tokens"), u.get("total_tokens"),
                    ev.get("metadata", {}).get("total_api_requests") or u.get("requests"))
    return (None, None, None, None)


def rapid_tokens(path):
    m = re.search(r"^tokens used: (\d+)", path.read_text(), re.M)
    return int(m.group(1)) if m else None


def wall(path):
    m = re.search(r"^real (\S+)", path.read_text(), re.M)
    return float(m.group(1)) if m else None


def fmt(n):
    return f"{n:,}" if isinstance(n, int) else "-"


for p in RUNS:
    for cli in ("rapid", "qwen"):
        d = B / p / cli
        code = (d / "exit_code").read_text().strip() if (d / "exit_code").exists() else "missing"
        w = wall(d / "time_err.txt") if (d / "time_err.txt").exists() else None
        if cli == "rapid":
            t_in = t_out = None
            t_total = rapid_tokens(d / "time_err.txt")
            reqs = 1
        else:
            t_in, t_out, t_total, reqs = qwen_usage(d / "stdout.json")
        print(f"{p:5} {cli:5} exit={code} wall={w:>6}s tokens(in/out/total)="
              f"{fmt(t_in) if t_in is not None else '-'}/{fmt(t_out) if t_out is not None else '-'}/"
              f"{fmt(t_total) if t_total is not None else '-'} reqs={reqs if reqs else '-'}")
