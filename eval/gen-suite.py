#!/usr/bin/env python3
"""Generate the 40-task benchmark suite into eval/suite/.

Each task seeds a scratch repo (setup files), states a plain-language task,
carries a gold patch, and names a verification command (exit 0 = correct).
The verification command is an external judge: it runs after whatever the
agent did and is the only scoring signal. `{RAPID}` in a verify command names
the rapid binary itself.
"""
import json, os

SUITE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "suite")

def write_task(task_id, category, prompt, setup, gold, verify, fails_before=True):
    spec = {
        "id": task_id,
        "category": category,
        "prompt": prompt,
        "setup": setup,
        "gold": gold,
        "verify": verify,
        "verify_fails_before": fails_before,
    }
    with open(os.path.join(SUITE, task_id + ".json"), "w") as f:
        json.dump(spec, f, indent=2)
        f.write("\n")

os.makedirs(SUITE, exist_ok=True)
N = 1

# ---------------------------------------------------------------- bugfix x10
BUGS = [
    ("calc", "add", "def add(a, b):\n    return a - b\n", "def add(a, b):\n    return a + b\n",
     "add(4, 2) returns -2; it must return 6"),
    ("strs", "shout", 'def shout(s):\n    return s.lower()\n', 'def shout(s):\n    return s.upper()\n',
     'shout("hey") returns "hey"; it must return "HEY"'),
    ("stats", "mean", "def mean(xs):\n    return sum(xs) / (len(xs) + 1)\n", "def mean(xs):\n    return sum(xs) / len(xs)\n",
     "mean([1, 2, 3]) returns 2.0... wrong: it must return 2"),
    ("lists", "drop_last", "def drop_last(xs):\n    return xs[1:]\n", "def drop_last(xs):\n    return xs[:-1]\n",
     "drop_last([1, 2, 3]) drops the FIRST item; it must drop the last"),
    ("calc", "mul", "def mul(a, b):\n    return a + b\n", "def mul(a, b):\n    return a * b\n",
     "mul(4, 2) returns 6; it must return 8"),
    ("strs", "repeat", 'def repeat(s, n):\n    return "-".join([s] * n)\n', 'def repeat(s, n):\n    return s * n\n',
     'repeat("ab", 2) returns "ab-ab"; it must return "abab"'),
    ("stats", "median", "def median(xs):\n    return xs[0]\n",
     "def median(xs):\n    ys = sorted(xs)\n    n = len(ys)\n    mid = n // 2\n    if n % 2:\n        return ys[mid]\n    return (ys[mid - 1] + ys[mid]) / 2\n",
     "median(xs) returns the first element; it must return the statistical median"),
    ("lists", "chunk", "def chunk(xs, n):\n    return [xs[i:i+n] for i in range(0, len(xs), n) if len(xs[i:i+n]) == n]\n",
     "def chunk(xs, n):\n    return [xs[i:i+n] for i in range(0, len(xs), n)]\n",
     "chunk([1, 2, 3], 2) drops the remainder [3]; it must keep it"),
    ("calc", "neg", "def neg(x):\n    return x\n", "def neg(x):\n    return -x\n",
     "neg(5) returns 5; it must return -5"),
    ("strs", "title_case", 'def title_case(s):\n    return s.capitalize().lower()\n', 'def title_case(s):\n    return s.capitalize()\n',
     'title_case("hello world") returns "Hello world" lowercased after the first letter... it must not lowercase the rest'),
]
CHECKS = {
    "add": "assert add(4, 2) == 6",
    "shout": 'assert shout("hey") == "HEY"',
    "mean": "assert mean([1, 2, 3]) == 2",
    "drop_last": "assert drop_last([1, 2, 3]) == [1, 2]",
    "mul": "assert mul(4, 2) == 8",
    "repeat": 'assert repeat("ab", 2) == "abab"',
    "median": "assert median([3, 1, 2]) == 2",
    "chunk": "assert chunk([1, 2, 3], 2) == [[1, 2], [3]]",
    "neg": "assert neg(5) == -5",
    "title_case": 'assert title_case("hello world") == "Hello world"',
}
for module, func, buggy, fixed, symptom in BUGS:
    test = "from " + module + " import " + func + "\n" + CHECKS[func] + "\nprint('OK')\n"
    write_task(
        "bugfix-%03d" % N, "bugfix",
        "test_" + module + ".py currently fails. Read " + module + ".py, find and fix the bug (known symptom: " + symptom + "), and make test_" + module + ".py pass. Do not change the test.",
        {module + ".py": buggy, "test_" + module + ".py": test},
        {module + ".py": fixed},
        "python3 -B test_" + module + ".py",
    )
    N += 1

# ------------------------------------------------------------- multifile x8
MULTIFILE = [
    ("TIMEOUT", "30", "60"), ("RETRIES", "1", "3"), ("BATCH_SIZE", "10", "25"),
    ("FEATURE_ON", "False", "True"), ("GREETING", '"hi"', '"hello"'),
    ("LIMIT", "5", "50"), ("SCHEME", '"http"', '"https"'), ("VERSION", "1", "2"),
]
for key, old_value, new_value in MULTIFILE:
    setup = {
        "config.py": key + " = " + old_value + "\n",
        "client.py": "import config\n\ndef value():\n    return config." + key + "\n",
        "worker.py": "import config\n\ndef current():\n    return config." + key + "\n",
        "test_config.py": (
            "from client import value\nfrom worker import current\n"
            "assert value() == current(), 'modules disagree'\nprint('OK')\n"
        ),
    }
    check = (
        "python3 -B -c 'import config, sys; sys.exit(0 if config." + key + " == " + new_value
        + " else 1)' && python3 -B test_config.py"
    )
    write_task(
        "multifile-%03d" % N, "multifile",
        "The requirement changed: config." + key + " must be " + new_value + " (it is "
        + old_value + "). Update config.py, keep client.py and worker.py working, and make the check pass: "
        + check,
        setup,
        {"config.py": key + " = " + new_value + "\n"},
        check,
    )
    N += 1

# ------------------------------------------------------------------ tests x8
TESTS = [
    ("pathutil", {"basename": 'def basename(p):\n    return p.rsplit("/", 1)[-1]\n',
                  "ext": "def ext(p):\n    return p.rsplit('.', 1)[-1] if '.' in p else ''\n"},
     ['assert basename("/x/y.py") == "y.py"', 'assert ext("y.py") == "py"']),
    ("numutil", {"clamp": "def clamp(x, lo, hi):\n    return max(lo, min(hi, x))\n",
                 "sign": "def sign(x):\n    return (x > 0) - (x < 0)\n"},
     ['assert clamp(9, 0, 5) == 5', 'assert sign(-3) == -1 and sign(0) == 0 and sign(4) == 1']),
    ("strutil", {"split_words": "def split_words(s):\n    return s.split()\n",
                 "join_words": "def join_words(ws):\n    return ' '.join(ws)\n"},
     ['assert split_words("a b") == ["a", "b"]', 'assert join_words(["a", "b"]) == "a b"']),
    ("logutil", {"level_name": "def level_name(n):\n    return {10: 'DEBUG', 20: 'INFO'}.get(n, 'OTHER')\n",
                 "enabled": "def enabled(n):\n    return n >= 20\n"},
     ['assert level_name(20) == "INFO"', 'assert enabled(10) is False and enabled(20) is True']),
    ("mathutil", {"square": "def square(x):\n    return x * x\n", "cube": "def cube(x):\n    return x * x * x\n"},
     ['assert square(3) == 9', 'assert cube(2) == 8']),
    ("listutil", {"dedupe": "def dedupe(xs):\n    return list(dict.fromkeys(xs))\n",
                  "flatten": "def flatten(xss):\n    return [x for xs in xss for x in xs]\n"},
     ['assert dedupe([1, 1, 2]) == [1, 2]', 'assert flatten([[1], [2, 3]]) == [1, 2, 3]']),
    ("dictutil", {"merge": "def merge(a, b):\n    return {**a, **b}\n",
                  "invert": "def invert(d):\n    return {v: k for k, v in d.items()}\n"},
     ['assert merge({"a": 1}, {"b": 2}) == {"a": 1, "b": 2}', 'assert invert({"a": 1}) == {1: "a"}']),
    ("textutil", {"wrap": "def wrap(s, n):\n    return s[:n]\n", "indent": "def indent(s):\n    return '  ' + s\n"},
     ['assert wrap("abcdef", 3) == "abc"', 'assert indent("x") == "  x"']),
]
for module, funcs, checks in TESTS:
    names = list(funcs.keys())
    setup = {module + ".py": "\n\n".join(funcs.values()) + "\n"}
    gold_test = "from " + module + " import " + ", ".join(names) + "\n" + "\n".join(checks) + "\nprint('OK')\n"
    write_task(
        "tests-%03d" % N, "tests",
        module + ".py has no tests. Write test_" + module + ".py that imports every public function ("
        + ", ".join(names) + ") and asserts their documented behavior, then make it pass.",
        setup,
        {"test_" + module + ".py": gold_test},
        "python3 -B test_" + module + ".py",
    )
    N += 1

# --------------------------------------------------------------- recovery x7
PIPELINE = (
    "raw = open('input.txt').read()\n"
    "PROCESSED = raw.strip().upper()\n"
    "open('out/result.txt', 'w').write(PROCESSED)\n"
)
PIPELINE_FIXED = (
    "import os\n"
    "raw = open('input.txt').read()\n"
    "PROCESSED = raw.strip().upper()\n"
    "os.makedirs('out', exist_ok=True)\n"
    "open('out/result.txt', 'w').write(PROCESSED)\n"
)
for index in range(7):
    write_task(
        "recovery-%03d" % N, "recovery",
        "sh run.sh fails partway through the pipeline. Diagnose the failure, repair it, and make sh run.sh print PIPELINE-OK.",
        {
            "pipeline.py": PIPELINE,
            "input.txt": "hello pipeline\n",
            "run.sh": "python3 -B pipeline.py && test -s out/result.txt && echo PIPELINE-OK\n",
        },
        {"pipeline.py": PIPELINE_FIXED},
        "sh run.sh | grep PIPELINE-OK",
    )
    N += 1

# --------------------------------------------------------------- workflow x7
for index in range(7):
    bad = {
        "name": "broken-%d" % index,
        "steps": [
            {"key": "a", "kind": "task", "label": "a", "depends_on": ["b"]},
            {"key": "b", "kind": "task", "label": "b", "depends_on": ["a"]},
        ],
    }
    good = {
        "name": "fixed-%d" % index,
        "steps": [
            {"key": "build", "kind": "task", "label": "build the artifact"},
            {"key": "check", "kind": "verification", "label": "check the artifact",
             "depends_on": ["build"], "command": "true"},
        ],
    }
    write_task(
        "workflow-%03d" % N, "workflow",
        "playbook.json fails to compile because its dependencies form a cycle. Replace it with a valid two-step playbook: a `task` step named build, and a `verification` step named check that depends on build and runs `true`. Verify with: {RAPID} playbook-compile playbook.json",
        {"playbook.json": json.dumps(bad, indent=2)},
        {"playbook.json": json.dumps(good, indent=2)},
        "{RAPID} playbook-compile playbook.json > /dev/null",
    )
    N += 1

print("tasks written:", N - 1)
