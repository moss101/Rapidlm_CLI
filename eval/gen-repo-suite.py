#!/usr/bin/env python3
"""Generate the REPRESENTATIVE model-quality suite into eval/suite/.

SMOKE vs REPRESENTATIVE vs HELD-OUT
- eval/suite-smoke/  (gen-suite.py): 40 mechanical cases; harness validation,
  labelled smoke wherever reported. NOT a model-quality evaluation.
- eval/suite/        (this file): independent tasks across the capability
  axes the product claims — unfamiliar-code navigation, genuine multi-file
  refactoring, scattered long-context requirements, tool-error recovery,
  and multi-change integration. Every task carries its own repo, prompt,
  gold patch, protected judge files, and (where meaningful) a mutation
  matrix. This is the default `rapid eval` suite.
- eval/suite-heldout/ (heldout section below): tasks of the same shape that
  are NEVER used for tuning; they exist so reported numbers can be checked
  against unseen instances.

Grading v2 contract (enforced by the runner, data stored outside the
agent-editable scratch): `protected` files must survive byte-identical;
`mutants` must each be REJECTED by the submission (gold kills all of them —
the self-check below re-proves it every generation).

Verify-command quoting discipline: the runner executes `sh -c <verify>`.
Inline python judges are single-quoted at the shell level and therefore
use ONLY double quotes inside. No judge file ever lives in the scratch.
"""
import json, os, shutil, subprocess, tempfile
from collections import Counter

HERE = os.path.dirname(os.path.abspath(__file__))
SUITE = os.path.join(HERE, "suite")
HELDOUT = os.path.join(HERE, "suite-heldout")


def write_task(directory, task_id, category, prompt, setup, gold, verify,
               fails_before=True, protected=None, mutants=None):
    spec = {
        "id": task_id,
        "category": category,
        "prompt": prompt,
        "setup": setup,
        "gold": gold,
        "verify": verify,
        "verify_fails_before": fails_before,
    }
    if protected:
        spec["protected"] = protected
    if mutants:
        spec["mutants"] = [{"file": f, "contents": c} for f, c in mutants]
    with open(os.path.join(directory, task_id + ".json"), "w") as f:
        json.dump(spec, f, indent=2)
        f.write("\n")


# ===========================================================================
# Shared seed: the gifserv service (navigate tasks)
# ===========================================================================

GIFSERV = {
    "README.md": (
        "# gifserv\n\n"
        "A tiny URL-shortener service with fixed-window rate limiting.\n\n"
        "Layout:\n"
        "- config.py — runtime settings (single source of truth for tunables).\n"
        "- store.py — in-memory KV counters with per-key TTLs.\n"
        "- ratelimit.py — the rate limiter; counters live in store.\n"
        "- api.py — request handler; returns 429 when the limiter denies.\n"
        "- worker.py — the background sweeper pass scheduler.\n"
        "- CONTRACTS.md — behavioral contracts the service must honor.\n"
    ),
    "CONTRACTS.md": (
        "# Contracts\n\n"
        "1. RATE_LIMIT = 0 is a FULL SHUTDOWN: every request is denied.\n"
        "   Any positive value admits up to that many requests per window.\n"
        "2. Rate windows are FIXED: a key's window expires exactly\n"
        "   WINDOW_SECONDS after its FIRST request, no matter how many\n"
        "   requests follow inside the window. A window expired AT its\n"
        "   expiry timestamp is stale.\n"
        "3. The sweeper pass interval comes from configuration and takes\n"
        "   effect without code changes.\n"
    ),
    "config.py": (
        "RATE_LIMIT = 60\n"
        "WINDOW_SECONDS = 60\n"
        "SWEEP_INTERVAL_SECONDS = 30\n"
    ),
    "store.py": (
        "class Store:\n"
        "    def __init__(self):\n"
        "        self._data = {}\n"
        "\n"
        "    def incr(self, key, now):\n"
        "        \"\"\"Increment key's counter; first hit opens the window.\"\"\"\n"
        "        entry = self._data.get(key)\n"
        "        if entry is None:\n"
        "            self._data[key] = {\"count\": 1, \"expires_at\": now + self.window_seconds()}\n"
        "            return 1\n"
        "        entry[\"count\"] += 1\n"
        "        entry[\"expires_at\"] = now + self.window_seconds()\n"
        "        return entry[\"count\"]\n"
        "\n"
        "    def get(self, key, now):\n"
        "        entry = self._data.get(key)\n"
        "        if entry is None or entry[\"expires_at\"] <= now:\n"
        "            return 0\n"
        "        return entry[\"count\"]\n"
        "\n"
        "    def window_seconds(self):\n"
        "        import config\n"
        "        return config.WINDOW_SECONDS\n"
        "\n"
        "    def sweep(self, now):\n"
        "        stale = [k for k, v in self._data.items() if v[\"expires_at\"] <= now]\n"
        "        for k in stale:\n"
        "            del self._data[k]\n"
        "        return len(stale)\n"
    ),
    "ratelimit.py": (
        "import config\n"
        "from store import Store\n"
        "\n"
        "_STORE = Store()\n"
        "\n"
        "def check_rate(key, now):\n"
        "    \"\"\"True when key may proceed, False to deny (429).\"\"\"\n"
        "    limit = config.RATE_LIMIT\n"
        "    if limit <= 0:\n"
        "        return True  # unlimited mode for the incident\n"
        "    return _STORE.incr(key, now) <= limit\n"
    ),
    "api.py": (
        "import ratelimit\n"
        "\n"
        "def handle_request(key, now):\n"
        "    if not ratelimit.check_rate(key, now):\n"
        "        return 429\n"
        "    return 200\n"
    ),
    "worker.py": (
        "import config\n"
        "from store import Store\n"
        "\n"
        "_STORE = Store()\n"
        "\n"
        "def sweep_interval():\n"
        "    \"\"\"Seconds between sweeper passes (from configuration).\"\"\"\n"
        "    return getattr(config, \"SWEEP_INTERVAL_S\", 30)\n"
    ),
}

RATELIMIT_FIXED = (
    "import config\n"
    "from store import Store\n"
    "\n"
    "_STORE = Store()\n"
    "\n"
    "def check_rate(key, now):\n"
    "    \"\"\"True when key may proceed, False to deny (429).\"\"\"\n"
    "    limit = config.RATE_LIMIT\n"
    "    if limit <= 0:\n"
    "        return False\n"
    "    return _STORE.incr(key, now) <= limit\n"
)

STORE_FIXED_TTL = (
    "class Store:\n"
    "    def __init__(self):\n"
    "        self._data = {}\n"
    "\n"
    "    def incr(self, key, now):\n"
    "        \"\"\"Increment key's counter; first hit opens the window.\"\"\"\n"
    "        entry = self._data.get(key)\n"
    "        if entry is None:\n"
    "            self._data[key] = {\"count\": 1, \"expires_at\": now + self.window_seconds()}\n"
    "            return 1\n"
    "        entry[\"count\"] += 1\n"
    "        return entry[\"count\"]\n"
    "\n"
    "    def get(self, key, now):\n"
    "        entry = self._data.get(key)\n"
    "        if entry is None or entry[\"expires_at\"] <= now:\n"
    "            return 0\n"
    "        return entry[\"count\"]\n"
    "\n"
    "    def window_seconds(self):\n"
    "        import config\n"
    "        return config.WINDOW_SECONDS\n"
    "\n"
    "    def sweep(self, now):\n"
    "        stale = [k for k, v in self._data.items() if v[\"expires_at\"] <= now]\n"
    "        for k in stale:\n"
    "            del self._data[k]\n"
    "        return len(stale)\n"
)

WORKER_FIXED = (
    "import config\n"
    "from store import Store\n"
    "\n"
    "_STORE = Store()\n"
    "\n"
    "def sweep_interval():\n"
    "    \"\"\"Seconds between sweeper passes (from configuration).\"\"\"\n"
    "    return config.SWEEP_INTERVAL_SECONDS\n"
)


def gen_navigate(directory, prefix):
    # -- navigate A: RATE_LIMIT=0 must deny (contract 1) --------------------
    write_task(
        directory, prefix + "-navigate-shutdown", "navigate",
        "During an incident, operators set RATE_LIMIT to 0 in config.py expecting a full "
        "shutdown, but requests still succeed. CONTRACTS.md contract 1 says RATE_LIMIT = 0 "
        "is a full shutdown: every request must be denied. Find the responsible code "
        "(start at api.py and follow the calls) and fix it so a limit of 0 denies every "
        "request while any positive limit keeps admitting up to that many requests per "
        "window. Do not change config.py or CONTRACTS.md.",
        GIFSERV,
        {"ratelimit.py": RATELIMIT_FIXED},
        "python3 -B -c '"
        "import config, ratelimit; "
        "config.RATE_LIMIT = 0; "
        "assert ratelimit.check_rate(\"probe\", 1) is False, \"zero must deny\"; "
        "config.RATE_LIMIT = 2; "
        "assert ratelimit.check_rate(\"u\", 2) is True; "
        "assert ratelimit.check_rate(\"u\", 3) is True; "
        "assert ratelimit.check_rate(\"u\", 4) is False, \"positive limit still counts\""
        "'",
        protected=["config.py", "CONTRACTS.md"],
        mutants=[("ratelimit.py", RATELIMIT_FIXED.replace(
            "return _STORE.incr(key, now) <= limit",
            "return _STORE.incr(key, now) < limit"))],
    )
    # -- navigate B: fixed-window TTL drift (contract 2) ---------------------
    write_task(
        directory, prefix + "-navigate-fixed-window", "navigate",
        "CONTRACTS.md contract 2 requires FIXED rate windows: a key's window must expire "
        "exactly WINDOW_SECONDS after its FIRST request, no matter how many requests "
        "follow. A monitoring probe showed windows being extended by traffic. Read the "
        "service (api.py and ratelimit.py show the call path) and fix the code so the "
        "contract holds. Do not change config.py, CONTRACTS.md, or the public signatures.",
        GIFSERV,
        {"store.py": STORE_FIXED_TTL},
        "python3 -B -c '"
        "from store import Store; "
        "s = Store(); "
        "s.incr(\"k\", 0); s.incr(\"k\", 10); s.incr(\"k\", 20); "
        "assert s.get(\"k\", 59) == 3 and s.get(\"k\", 61) == 0, \"window must expire "
        "WINDOW_SECONDS after the FIRST hit\""
        "'",
        protected=["config.py", "CONTRACTS.md"],
        mutants=[("store.py", STORE_FIXED_TTL.replace(
            "        entry[\"count\"] += 1\n        return entry[\"count\"]",
            "        entry[\"count\"] += 1\n"
            "        entry[\"expires_at\"] = max(entry[\"expires_at\"], now + self.window_seconds())\n"
            "        return entry[\"count\"]"))],
    )
    # -- navigate C: sweeper ignores configuration (contract 3) --------------
    write_task(
        directory, prefix + "-navigate-sweeper-config", "navigate",
        "Operators changed SWEEP_INTERVAL_SECONDS in config.py and the sweeper's pass "
        "interval did not change (CONTRACTS.md contract 3). Trace how worker.py resolves "
        "the interval, find why configuration is ignored, and fix it so the configured "
        "value is honored. Keep the sweep_interval() signature and do not change config.py.",
        GIFSERV,
        {"worker.py": WORKER_FIXED},
        "python3 -B -c '"
        "import config, worker; "
        "config.SWEEP_INTERVAL_SECONDS = 7; "
        "assert worker.sweep_interval() == 7, \"configured interval must be honored\"; "
        "config.SWEEP_INTERVAL_SECONDS = 45; "
        "assert worker.sweep_interval() == 45"
        "'",
        protected=["config.py", "CONTRACTS.md"],
        mutants=[("worker.py", WORKER_FIXED.replace(
            "return config.SWEEP_INTERVAL_SECONDS", "return config.WINDOW_SECONDS"))],
    )


# ===========================================================================
# Refactor: consolidation with drift (real multi-file gold patches)
# ===========================================================================

def consolidation_refactor(directory, task_id, prompt_intro, modules, contract_test,
                           shared_module, shared_gold, mutants, extract_name):
    """Common shape: N modules each define `extract_name` (drifted); consolidate
    into shared_module; every module re-exports the name; contract test judges;
    a grep clause forbids any residual local def."""
    setup = dict(modules)
    setup["test_contract.py"] = contract_test
    gold = {shared_module: shared_gold}
    for name in modules:
        gold[name] = "from " + shared_module[:-3] + " import " + extract_name + "\n"
    verify = ("python3 -B test_contract.py && ! grep -q \"def " + extract_name + "\" "
              + " ".join(modules.keys()))
    write_task(
        directory, task_id, "refactor",
        prompt_intro + " "
        + "The authoritative behavior is test_contract.py. Create " + shared_module + " with the "
        "single authoritative definition and change every listed module to import the name "
        "from it (keep re-exporting the same name so existing imports keep working). Every "
        "module must behave identically and all contract assertions must pass. Do not modify "
        "test_contract.py, and no listed module may keep its own def.",
        setup, gold, verify, protected=["test_contract.py"], mutants=mutants,
    )


def gen_refactor(directory):
    # -- discounts ------------------------------------------------------------
    economy = "def discount(customer_type, subtotal):\n    return 0.05 if customer_type == \"member\" else 0.0\n"
    premium = ("def discount(customer_type, subtotal):\n"
               "    if customer_type == \"member\":\n"
               "        return 0.05\n"
               "    if customer_type == \"vip\":\n"
               "        return 0.15\n"
               "    return 0.0\n")
    legacy = "def discount(customer_type, subtotal):\n    return 0.05 if customer_type == \"member\" else 0.0\n"
    pricing_gold = ("def discount(customer_type, subtotal):\n"
                    "    if subtotal <= 0:\n"
                    "        return 0.0\n"
                    "    if customer_type == \"member\":\n"
                    "        return 0.05\n"
                    "    if customer_type == \"vip\":\n"
                    "        return 0.15\n"
                    "    return 0.0\n")
    contract = (
        "from economy import discount as d1\n"
        "from premium import discount as d2\n"
        "from legacy_cli import discount as d3\n"
        "import pricing\n"
        "for d in (d1, d2, d3, pricing.discount):\n"
        "    assert d(\"member\", 100) == 0.05\n"
        "    assert d(\"vip\", 100) == 0.15\n"
        "    assert d(\"guest\", 100) == 0.0\n"
        "    assert d(\"member\", 0) == 0.0, \"empty carts get no discount\"\n"
        "print(\"CONTRACT-OK\")\n"
    )
    consolidation_refactor(
        directory, "refactor-discount-consolidation",
        "economy.py, premium.py, and legacy_cli.py each define their own discount() and the "
        "copies have drifted apart (one lost a tier, none handles empty carts).",
        {"economy.py": economy, "premium.py": premium, "legacy_cli.py": legacy},
        contract, "pricing.py", pricing_gold,
        mutants=[
            ("pricing.py", pricing_gold.replace("        return 0.15\n", "        return 0.10\n")),
            ("pricing.py", pricing_gold.replace("    if subtotal <= 0:\n        return 0.0\n", "")),
        ],
        extract_name="discount",
    )

    # -- backoff --------------------------------------------------------------
    def client(expr):
        return ("RETRY_CEILING = 5\n\n\ndef backoff(attempt):\n"
                "    \"\"\"Seconds to wait before retry number `attempt`.\"\"\"\n"
                "    return " + expr + "\n")
    backoff_gold = ("def backoff(attempt):\n"
                    "    \"\"\"Seconds to wait before retry number `attempt`.\"\"\"\n"
                    "    if attempt < 0:\n"
                    "        return 0\n"
                    "    return 2 ** attempt\n")
    contract = (
        "from api_client import backoff as b1\n"
        "from mail_client import backoff as b2\n"
        "from report_client import backoff as b3\n"
        "import retry_policy\n"
        "for b in (b1, b2, b3, retry_policy.backoff):\n"
        "    assert [b(a) for a in range(6)] == [1, 2, 4, 8, 16, 32]\n"
        "    assert b(-1) == 0, \"negative attempts wait zero\"\n"
        "print(\"CONTRACT-OK\")\n"
    )
    consolidation_refactor(
        directory, "refactor-backoff-consolidation",
        "api_client.py, mail_client.py, and report_client.py each define their own backoff() "
        "and the copies disagree (squared, exponential, and linear).",
        {"api_client.py": client("attempt ** 2"),
         "mail_client.py": client("2 ** attempt"),
         "report_client.py": client("3 * attempt")},
        contract, "retry_policy.py", backoff_gold,
        mutants=[("retry_policy.py", backoff_gold.replace("return 2 ** attempt", "return 2 ** (attempt - 1)"))],
        extract_name="backoff",
    )

    # -- key normalization ------------------------------------------------------
    def parser(body):
        return body
    normalize_gold = ("def normalize_key(raw):\n"
                      "    \"\"\"Normalize one settings key: trimmed, lowercase, dashes to underscores.\"\"\"\n"
                      "    return raw.strip().lower().replace(\"-\", \"_\")\n")
    contract = (
        "from ini_parser import normalize_key as n1\n"
        "from env_parser import normalize_key as n2\n"
        "from cli_flags import normalize_key as n3\n"
        "import normalize\n"
        "for n in (n1, n2, n3, normalize.normalize_key):\n"
        "    assert n(\"  Max-Connections \") == \"max_connections\"\n"
        "    assert n(\"TIMEOUT\") == \"timeout\"\n"
        "print(\"CONTRACT-OK\")\n"
    )
    consolidation_refactor(
        directory, "refactor-normalize-consolidation",
        "ini_parser.py, env_parser.py, and cli_flags.py each normalize settings keys their "
        "own way, so the same key resolves differently per source.",
        {"ini_parser.py": parser("def normalize_key(raw):\n    return raw.strip()\n"),
         "env_parser.py": parser("def normalize_key(raw):\n    return raw.strip().lower().replace(\"-\", \"_\")\n"),
         "cli_flags.py": parser("def normalize_key(raw):\n    return raw.strip().lower()\n")},
        contract, "normalize.py", normalize_gold,
        mutants=[("normalize.py", normalize_gold.replace(".replace(\"-\", \"_\")", ""))],
        extract_name="normalize_key",
    )

    # -- log formatting -----------------------------------------------------------
    web_l = ("LEVEL_TAGS = {\"INFO\": \"INFO\", \"WARN\": \"WARN\", \"ERROR\": \"ERROR\"}\n\n\ndef format_line(level, message, stamp):\n"
             "    return stamp + \" [\" + LEVEL_TAGS.get(level, level) + \"] \" + message\n")
    job_l = ("def format_line(level, message, stamp):\n"
             "    return stamp + \" \" + message\n")
    cli_l = ("def format_line(level, message, stamp):\n"
             "    return \"[\" + level + \"] \" + message\n")
    logfmt_gold = ("def format_line(level, message, stamp):\n"
                   "    \"\"\"Canonical line: <ISO stamp> [<LEVEL>] <message>.\"\"\"\n"
                   "    return stamp + \" [\" + level + \"] \" + message\n")
    contract = (
        "from web_logger import format_line as f1\n"
        "from job_logger import format_line as f2\n"
        "from cli_logger import format_line as f3\n"
        "import logfmt\n"
        "for f in (f1, f2, f3, logfmt.format_line):\n"
        "    assert f(\"INFO\", \"hello\", \"2026-01-01T00:00:00\") == \"2026-01-01T00:00:00 [INFO] hello\"\n"
        "    assert f(\"ERROR\", \"boom\", \"2026-01-01T00:00:01\") == \"2026-01-01T00:00:01 [ERROR] boom\"\n"
        "print(\"CONTRACT-OK\")\n"
    )
    consolidation_refactor(
        directory, "refactor-logfmt-consolidation",
        "web_logger.py, job_logger.py, and cli_logger.py each format log lines differently "
        "(one drops the level, one drops the timestamp), so no two logs parse the same.",
        {"web_logger.py": web_l, "job_logger.py": job_l, "cli_logger.py": cli_l},
        contract, "logfmt.py", logfmt_gold,
        mutants=[("logfmt.py", logfmt_gold.replace("\" [\" + level + \"] \"", "\" \""))],
        extract_name="format_line",
    )


# ===========================================================================
# Context: requirements scattered across many files
# ===========================================================================

POLICY_CONTRACTS = [
    ("service_shipping.py",
     "# CONTRACT(policy.shipping_cost): shipping_cost(weight_kg) returns\n"
     "# 5 + 2 * weight_kg, capped at 25 (oversized items ship at the flat max).\n"),
    ("service_tax.py",
     "# CONTRACT(policy.tax): tax(amount, region) multiplies amount by the\n"
     "# regional rate: EU 0.20, US 0.07, everywhere else 0.15.\n"),
    ("service_promos.py",
     "# CONTRACT(policy.discount_code): discount_code(code) returns the promo\n"
     "# fraction: WELCOME10 -> 0.10, VIP20 -> 0.20, unknown -> 0.0.\n"),
    ("service_totals.py",
     "# CONTRACT(policy.total): total(subtotal, weight_kg, region, code) computes\n"
     "# subtotal minus the code's discount fraction, applies the regional tax RATE\n"
     "# to the discounted amount, adds shipping_cost(weight_kg), and rounds the\n"
     "# result to 2 decimal places.\n"),
    ("service_express.py",
     "# CONTRACT(policy.can_express): can_express(region, weight_kg) is True only\n"
     "# when weight_kg is at most 20 AND region is not \"AU\".\n"),
    ("service_free_shipping.py",
     "# CONTRACT(policy.free_shipping): free_shipping(total, region) is True only\n"
     "# for US orders whose total is at least 50.\n"),
    ("service_billing_split.py",
     "# CONTRACT(policy.split_billing): split_billing(total, people) returns the\n"
     "# tuple (even_share, last_share): everyone but the last pays\n"
     "# round(total / people, 2); the last pays round(total - even_share * (people - 1), 2).\n"),
    ("service_currency.py",
     "# CONTRACT(policy.currency): currency(amount) formats as \"<amount> USD\"\n"
     "# with exactly two decimals.\n"),
]

POLICY_GOLD = (
    "def shipping_cost(weight_kg):\n"
    "    return min(25, 5 + 2 * weight_kg)\n"
    "\n"
    "def tax(amount, region):\n"
    "    return amount * {\"EU\": 0.20, \"US\": 0.07}.get(region, 0.15)\n"
    "\n"
    "def discount_code(code):\n"
    "    return {\"WELCOME10\": 0.10, \"VIP20\": 0.20}.get(code, 0.0)\n"
    "\n"
    "def total(subtotal, weight_kg, region, code):\n"
    "    discounted = subtotal * (1 - discount_code(code))\n"
    "    return round(discounted * (1 + tax(1, region)) + shipping_cost(weight_kg), 2)\n"
    "\n"
    "def can_express(region, weight_kg):\n"
    "    return weight_kg <= 20 and region != \"AU\"\n"
    "\n"
    "def free_shipping(total_amount, region):\n"
    "    return region == \"US\" and total_amount >= 50\n"
    "\n"
    "def split_billing(total_amount, people):\n"
    "    share = round(total_amount / people, 2)\n"
    "    last = round(total_amount - share * (people - 1), 2)\n"
    "    return share, last\n"
    "\n"
    "def currency(amount):\n"
    "    return \"%.2f USD\" % amount\n"
)

POLICY_VERIFY = (
    "python3 -B -c '"
    "import policy; "
    "assert policy.shipping_cost(0) == 5 and policy.shipping_cost(3) == 11 "
    "and policy.shipping_cost(50) == 25, \"cap must apply\"; "
    "assert abs(policy.tax(100, \"EU\") - 20.0) < 1e-9 "
    "and abs(policy.tax(100, \"US\") - 7.0) < 1e-9 "
    "and abs(policy.tax(100, \"JP\") - 15.0) < 1e-9; "
    "assert policy.discount_code(\"WELCOME10\") == 0.10 "
    "and policy.discount_code(\"VIP20\") == 0.20 and policy.discount_code(\"nope\") == 0.0; "
    "assert policy.total(100, 0, \"US\", \"WELCOME10\") == round(100 * 0.90 * 1.07 + 5, 2); "
    "assert policy.can_express(\"US\", 20) is True and policy.can_express(\"US\", 21) is False "
    "and policy.can_express(\"AU\", 5) is False; "
    "assert policy.free_shipping(50, \"US\") is True and policy.free_shipping(49.99, \"US\") is False "
    "and policy.free_shipping(99, \"EU\") is False; "
    "share, last = policy.split_billing(100, 3); "
    "assert share == 33.33 and last == 33.34; "
    "assert policy.currency(7) == \"7.00 USD\""
    "'"
)

POLICY_DECOYS = {
    "service_audit.py": (
        "# Audit notes, not contracts.\n"
        "# 2026-08: totals module refactored twice; watch rounding.\n"
        "# 2026-09: express eligibility tightened for AU.\n"
    ),
    "service_notes.py": (
        "# Random ops notes.\n"
        "# The staging pool is capped at 4 workers.\n"
        "# Never edit generated clients by hand.\n"
    ),
}


def gen_context(directory, prefix):
    # -- context A: policy contracts scattered through service modules --------
    setup = {}
    for fname, contract in POLICY_CONTRACTS:
        setup[fname] = (
            "Service helper. The binding behavior lives in the CONTRACT comment.\n\n"
            + contract
            + "\ndef _placeholder():\n    return None\n"
        )
    setup.update(POLICY_DECOYS)
    setup["README.md"] = (
        "# orderdesk\n\n"
        "Pricing and shipping policy is defined by CONTRACT comments scattered through\n"
        "the service_* modules. policy.py is the single place those behaviors are\n"
        "implemented for callers.\n"
    )
    write_task(
        directory, prefix + "-context-scattered-contracts", "context",
        "The orderdesk service defines its pricing behavior in CONTRACT comments scattered "
        "across the service_*.py modules (two of the files are just notes, not contracts). "
        "Implement policy.py so it exports exactly these functions with the contracted "
        "behavior: shipping_cost, tax, discount_code, total, can_express, free_shipping, "
        "split_billing, currency. Read every service_*.py first: the contracts are "
        "authoritative, including rounding, flooring, and argument order. Do not modify "
        "the service_*.py files.",
        setup,
        {"policy.py": POLICY_GOLD},
        POLICY_VERIFY,
        protected=[fname for fname, _ in POLICY_CONTRACTS] + list(POLICY_DECOYS),
        mutants=[("policy.py", POLICY_GOLD.replace(
            "return min(25, 5 + 2 * weight_kg)", "return 5 + 2 * weight_kg"))],
    )


# ===========================================================================
# Errors: recovery through failing tools
# ===========================================================================

DEPLOY_SH = "set -e\npython3 tools/migrate.py\npython3 tools/seed.py\npython3 tools/report.py\necho DEPLOY-OK\n"

MIGRATE_BUGGY = (
    "import json\n"
    "\n"
    "def run():\n"
    "    with open(\"migrations/applied.json\", \"w\") as f:\n"
    "        json.dump([\"0001-init\"], f)\n"
    "\n"
    "if __name__ == \"__main__\":\n"
    "    run()\n"
    "    print(\"migrate: ok\")\n"
)
MIGRATE_FIXED = MIGRATE_BUGGY.replace(
    "import json\n",
    "import json\nimport os\n",
).replace(
    "    with open(\"migrations/applied.json\", \"w\") as f:\n",
    "    os.makedirs(\"migrations\", exist_ok=True)\n    with open(\"migrations/applied.json\", \"w\") as f:\n",
)

SEED_BUGGY = (
    "import json\n"
    "import os\n"
    "\n"
    "def run():\n"
    "    token = os.environ[\"SEED_TOKEN\"]\n"
    "    with open(\"seed/manifest.json\", \"w\") as f:\n"
    "        json.dump({\"token\": token}, f)\n"
    "\n"
    "if __name__ == \"__main__\":\n"
    "    run()\n"
    "    print(\"seed: ok\")\n"
)
SEED_FIXED = (
    "import json\n"
    "import os\n"
    "\n"
    "def _token():\n"
    "    token = os.environ.get(\"SEED_TOKEN\")\n"
    "    if token:\n"
    "        return token\n"
    "    with open(\".env\") as f:\n"
    "        for line in f:\n"
    "            if line.startswith(\"SEED_TOKEN=\"):\n"
    "                return line.strip().split(\"=\", 1)[1]\n"
    "    raise SystemExit(\"no SEED_TOKEN\")\n"
    "\n"
    "def run():\n"
    "    os.makedirs(\"seed\", exist_ok=True)\n"
    "    with open(\"seed/manifest.json\", \"w\") as f:\n"
    "        json.dump({\"token\": _token()}, f)\n"
    "\n"
    "if __name__ == \"__main__\":\n"
    "    run()\n"
    "    print(\"seed: ok\")\n"
)

REPORT_BUGGY = (
    "def run():\n"
    "    with open(\"Data/summary.csv\") as f:\n"
    "        rows = f.read().count(\"\\n\")\n"
    "    print(\"report: %d rows\" % rows)\n"
    "\n"
    "if __name__ == \"__main__\":\n"
    "    run()\n"
)
REPORT_FIXED = REPORT_BUGGY.replace("Data/summary.csv", "data/summary.csv")


def gen_errors(directory):
    write_task(
        directory, "errors-deploy-pipeline", "errors",
        "Running `sh deploy.sh` fails partway. Diagnose and fix the repo so the full "
        "pipeline runs and prints DEPLOY-OK. Every step's failure is a real bug in the "
        "repo (some hints are in README.md). Run deploy.sh as you work. Do not modify "
        "deploy.sh or .env.",
        {
            "deploy.sh": DEPLOY_SH,
            ".env": "SEED_TOKEN=abc123\n",
            "README.md": (
                "# deploy-bundle\n\n"
                "`sh deploy.sh` runs migrate, seed, and report, then prints DEPLOY-OK.\n"
                "Secrets live in .env (KEY=VALUE lines); tools may read .env when the\n"
                "variable is not exported.\n"
            ),
            "tools/migrate.py": MIGRATE_BUGGY,
            "tools/seed.py": SEED_BUGGY,
            "tools/report.py": REPORT_BUGGY,
            "data/summary.csv": "id,name\n1,a\n2,b\n",
        },
        {
            "tools/migrate.py": MIGRATE_FIXED,
            "tools/seed.py": SEED_FIXED,
            "tools/report.py": REPORT_FIXED,
        },
        "sh deploy.sh | grep DEPLOY-OK",
        protected=["deploy.sh", ".env"],
    )

    # -- interrupted refactor ------------------------------------------------
    UTILS = (
        "def fmt_value(v):\n"
        "    \"\"\"Format one report value: thousands separators, 2 decimals for floats.\"\"\"\n"
        "    if isinstance(v, float):\n"
        "        return \"{:,.2f}\".format(v)\n"
        "    return \"{:,}\".format(v)\n"
    )
    UTILS_MUTANT = UTILS.replace("\"{:,.2f}\".format(v)", "\"{}\".format(round(v, 2))")
    REPORT_INTERRUPTED = (
        "from utils import fmt\n"
        "\n"
        "def _fmt(v):\n"
        "    return str(v)\n"
        "\n"
        "def build_report(rows):\n"
        "    lines = []\n"
        "    for name, value in rows:\n"
        "        lines.append(name + \": \" + _fmt(value))\n"
        "    return \"\\n\".join(lines)\n"
    )
    REPORT_DONE = (
        "from utils import fmt_value\n"
        "\n"
        "def build_report(rows):\n"
        "    lines = []\n"
        "    for name, value in rows:\n"
        "        lines.append(name + \": \" + fmt_value(value))\n"
        "    return \"\\n\".join(lines)\n"
    )
    TEST_REPORT = (
        "from report import build_report\n"
        "r = build_report([(\"visits\", 12345), (\"revenue\", 1234.5), (\"units\", 42)])\n"
        "assert r == \"visits: 12,345\\nrevenue: 1,234.50\\nunits: 42\", repr(r)\n"
        "print(\"CONTRACT-OK\")\n"
    )
    write_task(
        directory, "errors-interrupted-refactor", "errors",
        "A refactor was interrupted midway: report.py was being migrated onto utils.py's "
        "formatter (fmt_value), but the migration is incomplete and the module does not "
        "even import cleanly. Finish the migration: report.py must use utils.fmt_value for "
        "every value (no local formatting helpers left), build_report must produce "
        "`name: <formatted>` lines, and the contract test must pass. Do not modify "
        "utils.py or test_contract.py.",
        {
            "utils.py": UTILS,
            "report.py": REPORT_INTERRUPTED,
            "test_contract.py": TEST_REPORT,
            "MIGRATION-NOTES.md": (
                "# Interrupted migration\n\n"
                "Goal: report.py uses utils.fmt_value for all values; the local _fmt helper\n"
                "goes away. The import at the top of report.py was left pointing at the old\n"
                "name `fmt` — utils.py never exported that.\n"
            ),
        },
        {"report.py": REPORT_DONE},
        "python3 -B test_contract.py",
        protected=["utils.py", "test_contract.py"],
        mutants=[("utils.py", UTILS_MUTANT)],
    )


# ===========================================================================
# Integration: reconcile multiple changes
# ===========================================================================

def gen_integration(directory):
    # -- merge two features into one model ------------------------------------
    MODELS_BASE = (
        "class User:\n"
        "    def __init__(self, name):\n"
        "        self.name = name\n"
    )
    AUTH_VERSION = (
        "class User:\n"
        "    def __init__(self, name, email):\n"
        "        self.name = name\n"
        "        self.email = email\n"
        "\n"
        "def validate_email(user):\n"
        "    return \"@\" in user.email and user.email.index(\"@\") > 0\n"
    )
    BILLING_VERSION = (
        "class User:\n"
        "    def __init__(self, name, plan):\n"
        "        self.name = name\n"
        "        self.plan = plan\n"
        "\n"
        "PLAN_PRICES = {\"free\": 0, \"pro\": 20, \"team\": 50}\n"
        "\n"
        "def plan_price(user, months=1):\n"
        "    return PLAN_PRICES.get(user.plan, 0) * months\n"
    )
    MERGED = (
        "class User:\n"
        "    def __init__(self, name, email, plan):\n"
        "        self.name = name\n"
        "        self.email = email\n"
        "        self.plan = plan\n"
        "\n"
        "PLAN_PRICES = {\"free\": 0, \"pro\": 20, \"team\": 50}\n"
        "\n"
        "def validate_email(user):\n"
        "    return \"@\" in user.email and user.email.index(\"@\") > 0\n"
        "\n"
        "def plan_price(user, months=1):\n"
        "    return PLAN_PRICES.get(user.plan, 0) * months\n"
    )
    MERGE_TEST = (
        "from models import User, validate_email, plan_price\n"
        "u = User(\"amy\", \"amy@example.com\", \"pro\")\n"
        "assert u.name == \"amy\" and u.email == \"amy@example.com\" and u.plan == \"pro\"\n"
        "assert validate_email(u) is True\n"
        "assert plan_price(u) == 20 and plan_price(u, 3) == 60\n"
        "assert plan_price(User(\"bob\", \"b@x.com\", \"free\")) == 0\n"
        "print(\"CONTRACT-OK\")\n"
    )
    write_task(
        directory, "integration-two-feature-merge", "integration",
        "Two feature branches each extended models.py, and now they must land together. "
        "feature-auth/user.py adds the email field and validate_email; feature-billing/user.py "
        "adds the plan field, PLAN_PRICES, and plan_price(user, months=1). Integrate BOTH "
        "features into the single top-level models.py so test_contract.py passes: User takes "
        "(name, email, plan); validate_email and plan_price keep their behavior and names. "
        "Do not modify feature-auth/, feature-billing/, or test_contract.py.",
        {
            "models.py": MODELS_BASE,
            "feature-auth/user.py": AUTH_VERSION,
            "feature-billing/user.py": BILLING_VERSION,
            "test_contract.py": MERGE_TEST,
        },
        {"models.py": MERGED},
        "python3 -B test_contract.py",
        protected=["feature-auth/user.py", "feature-billing/user.py", "test_contract.py"],
        mutants=[("models.py", MERGED.replace(
            "return PLAN_PRICES.get(user.plan, 0) * months",
            "return PLAN_PRICES.get(user.plan, 0)"))],
    )

    # -- post-rename import reconciliation ------------------------------------
    TEXTKIT = (
        "def slugify(text):\n"
        "    return \"-\".join(text.lower().split())\n"
        "\n"
        "def truncate(text, n):\n"
        "    return text if len(text) <= n else text[: n - 3] + \"...\"\n"
    )
    IMPORTER = (
        "from strings_util import slugify\n"
        "\n"
        "def topic_key(title):\n"
        "    return slugify(title)[:40]\n"
    )
    EXPORTER = (
        "from strings_util import truncate\n"
        "\n"
        "def heading(title):\n"
        "    return truncate(title, 24)\n"
    )
    KIT_TEST = (
        "from importer import topic_key\n"
        "from exporter import heading\n"
        "from textkit import slugify, truncate\n"
        "assert topic_key(\"Hello Wide World\") == \"hello-wide-world\"\n"
        "h = heading(\"x\" * 100)\n"
        "assert h.endswith(\"...\") and len(h) == 24, repr(h)\n"
        "assert slugify(\"A  B\") == \"a-b\" and truncate(\"abc\", 5) == \"abc\"\n"
        "print(\"CONTRACT-OK\")\n"
    )
    write_task(
        directory, "integration-post-rename-imports", "integration",
        "The strings_util module was renamed to textkit.py last week, but two new "
        "contributions (importer.py, exporter.py) were written against the old name, so "
        "the package is broken. Fix all imports so test_contract.py passes. The old "
        "strings_util.py is gone and must stay gone; textkit.py is authoritative and must "
        "not change. Do not modify test_contract.py.",
        {
            "textkit.py": TEXTKIT,
            "importer.py": IMPORTER,
            "exporter.py": EXPORTER,
            "test_contract.py": KIT_TEST,
        },
        {
            "importer.py": IMPORTER.replace("from strings_util import", "from textkit import"),
            "exporter.py": EXPORTER.replace("from strings_util import", "from textkit import"),
        },
        "python3 -B test_contract.py && ! test -e strings_util.py",
        protected=["textkit.py", "test_contract.py"],
    )


# ===========================================================================
# Held-out variants (same axes, unseen instances — never used for tuning)
# ===========================================================================

HELDOUT_WORKER_BUGGY = (
    "import config\n"
    "from store import Store\n"
    "\n"
    "_STORE = Store()\n"
    "\n"
    "def sweep(now):\n"
    "    \"\"\"Drop keys whose window has expired at or before `now`.\"\"\"\n"
    "    stale = [k for k, v in _STORE.all_items() if v[\"expires_at\"] < now]\n"
    "    for k in stale:\n"
    "        _STORE.drop(k)\n"
    "    return len(stale)\n"
)
HELDOUT_STORE = (
    "class Store:\n"
    "    def __init__(self):\n"
    "        self._data = {}\n"
    "\n"
    "    def open_window(self, key, now, window):\n"
    "        self._data[key] = {\"count\": 1, \"expires_at\": now + window}\n"
    "\n"
    "    def all_items(self):\n"
    "        return list(self._data.items())\n"
    "\n"
    "    def drop(self, key):\n"
    "        self._data.pop(key, None)\n"
)


def gen_heldout(directory):
    # navigate: off-by-one expiry boundary
    write_task(
        directory, "heldout-navigate-expiry-boundary", "navigate",
        "CONTRACTS.md contract 2 says a window expired AT its expiry timestamp is stale, "
        "and a probe showed expired counters surviving one extra sweep. worker.sweep(now) "
        "must drop every key whose expires_at is at or before `now`. All store helpers you "
        "need exist. Fix worker.sweep. Do not change store.py, config.py, or CONTRACTS.md.",
        {**GIFSERV, "worker.py": HELDOUT_WORKER_BUGGY, "store.py": HELDOUT_STORE},
        {"worker.py": HELDOUT_WORKER_BUGGY.replace(
            'if v["expires_at"] < now]', 'if v["expires_at"] <= now]')},
        "python3 -B -c '"
        "import worker; "
        "worker._STORE.open_window(\"k\", 0, 60); "
        "removed = worker.sweep(60); "
        "assert removed == 1, \"a key expired AT now must be swept\"; "
        "worker._STORE.open_window(\"j\", 10, 60); "
        "assert worker.sweep(20) == 0, \"live windows survive\""
        "'",
        protected=["store.py", "config.py", "CONTRACTS.md"],
    )

    # refactor: validators consolidation (held-out)
    validators_gold = (
        "def validate(kind, value):\n"
        "    if kind == \"email\":\n"
        "        return \"@\" in value and \".\" in value.split(\"@\")[-1]\n"
        "    if kind == \"url\":\n"
        "        return value.startswith(\"https://\")\n"
        "    if kind == \"phone\":\n"
        "        return value.replace(\"-\", \"\").isdigit() and len(value.replace(\"-\", \"\")) == 10\n"
        "    raise ValueError(kind)\n"
    )
    contract = (
        "from email_field import validate as v1\n"
        "from url_field import validate as v2\n"
        "from phone_field import validate as v3\n"
        "import validators\n"
        "assert v1(\"email\", \"a@b.com\") is True and v1(\"email\", \"nope\") is False\n"
        "assert v2(\"url\", \"https://x.dev\") is True and v2(\"url\", \"http://x.dev\") is False\n"
        "assert v3(\"phone\", \"555-123-4567\") is True and v3(\"phone\", \"55512\") is False\n"
        "assert validators.validate(\"email\", \"a@b.com\") is True\n"
        "assert validators.validate(\"phone\", \"5551234567\") is True\n"
        "assert validators.validate(\"url\", \"https://y.io\") is True\n"
        "print(\"CONTRACT-OK\")\n"
    )
    write_task(
        directory, "heldout-refactor-validators", "refactor",
        "email_field.py, url_field.py, and phone_field.py each define a one-argument "
        "validate(value) for one field kind. Consolidate: create validators.py with "
        "validate(kind, value) handling email/url/phone exactly as test_contract.py "
        "requires (unknown kinds raise ValueError), and change the three field modules to "
        "re-export validate from validators (plain re-export: after the change each module "
        "must contain no `def validate` of its own). Do not modify test_contract.py.",
        {
            "email_field.py": "def validate(value):\n    return \"@\" in value and \".\" in value.split(\"@\")[-1]\n",
            "url_field.py": "def validate(value):\n    return value.startswith(\"https://\")\n",
            "phone_field.py": "def validate(value):\n    return value.replace(\"-\", \"\").isdigit() and len(value.replace(\"-\", \"\")) == 10\n",
            "test_contract.py": contract,
        },
        {
            "validators.py": validators_gold,
            "email_field.py": "from validators import validate\n",
            "url_field.py": "from validators import validate\n",
            "phone_field.py": "from validators import validate\n",
        },
        "python3 -B test_contract.py && ! grep -q \"def validate\" email_field.py url_field.py phone_field.py",
        protected=["test_contract.py"],
        mutants=[("validators.py", validators_gold.replace(
            'return value.startswith("https://")', 'return value.startswith("http")'))],
    )

    # context: inventory contracts (held-out)
    inv_contracts = [
        ("inv_stock.py", "# CONTRACT(inventory.available): available(sku) returns stocked minus reserved, floored at 0.\n"),
        ("inv_reserve.py", "# CONTRACT(inventory.reserve): reserve(sku, n) raises ValueError when n exceeds available; otherwise adds to reserved and returns the new available.\n"),
        ("inv_release.py", "# CONTRACT(inventory.release): release(sku, n) subtracts n from reserved, floored at 0, and returns the new available.\n"),
        ("inv_receive.py", "# CONTRACT(inventory.receive): receive(sku, n) adds n to stocked and returns the new stocked.\n"),
        ("inv_value.py", "# CONTRACT(inventory.value): value(catalog) sums stocked * price for every sku in the catalog dict {sku: price}; skus never received contribute 0.\n"),
        ("inv_lowstock.py", "# CONTRACT(inventory.low_stock): low_stock(threshold) lists skus whose available is at or below threshold, sorted alphabetically.\n"),
    ]
    inv_gold = (
        "_stocked = {}\n"
        "_reserved = {}\n"
        "\n"
        "def available(sku):\n"
        "    return max(0, _stocked.get(sku, 0) - _reserved.get(sku, 0))\n"
        "\n"
        "def reserve(sku, n):\n"
        "    if n > available(sku):\n"
        "        raise ValueError(\"insufficient stock\")\n"
        "    _reserved[sku] = _reserved.get(sku, 0) + n\n"
        "    return available(sku)\n"
        "\n"
        "def release(sku, n):\n"
        "    _reserved[sku] = max(0, _reserved.get(sku, 0) - n)\n"
        "    return available(sku)\n"
        "\n"
        "def receive(sku, n):\n"
        "    _stocked[sku] = _stocked.get(sku, 0) + n\n"
        "    return _stocked[sku]\n"
        "\n"
        "def value(catalog):\n"
        "    return sum(_stocked.get(sku, 0) * price for sku, price in catalog.items())\n"
        "\n"
        "def low_stock(threshold):\n"
        "    skus = set(_stocked) | set(_reserved)\n"
        "    return sorted(sku for sku in skus if available(sku) <= threshold)\n"
    )
    inv_verify = "python3 -B -c '" + "\n".join([
        "import inventory",
        "assert inventory.receive(\"widget\", 10) == 10",
        "assert inventory.receive(\"gadget\", 3) == 3",
        "assert inventory.available(\"widget\") == 10",
        "assert inventory.reserve(\"widget\", 4) == 6",
        "assert inventory.available(\"widget\") == 6",
        "try:",
        "    inventory.reserve(\"gadget\", 4)",
        "    raise SystemExit(\"over-reserve must raise ValueError\")",
        "except ValueError:",
        "    pass",
        "assert inventory.release(\"widget\", 10) == 10, \"release floors reserved at zero\"",
        "assert inventory.low_stock(3) == [\"gadget\"]",
        "assert inventory.value({\"widget\": 2, \"ghost\": 5}) == 20",
    ]) + "'"
    setup = {}
    for fname, contract_text in inv_contracts:
        setup[fname] = "Inventory helper. The binding behavior lives in the CONTRACT comment.\n\n" + contract_text
    setup["README.md"] = (
        "# warehouse\n\n"
        "Inventory behavior is defined by CONTRACT comments across the inv_*.py modules.\n"
        "inventory.py implements them for callers.\n"
    )
    write_task(
        directory, "heldout-context-inventory", "context",
        "The warehouse service defines inventory behavior in CONTRACT comments across the "
        "inv_*.py modules. Implement inventory.py so it exports exactly: available, reserve, "
        "release, receive, value, low_stock — each with the contracted behavior. State is "
        "module-level (start empty). The contracts are authoritative, including the "
        "ValueError and flooring rules. Do not modify the inv_*.py files.",
        setup,
        {"inventory.py": inv_gold},
        inv_verify,
        protected=[fname for fname, _ in inv_contracts],
        mutants=[("inventory.py", inv_gold.replace(
            "    if n > available(sku):\n        raise ValueError(\"insufficient stock\")\n",
            "    if n < 0:\n        raise ValueError(\"insufficient stock\")\n"))],
    )

    # integration: two-branch pipeline config merge (held-out)
    BASE = (
        "STAGES = [\"build\", \"test\"]\n"
        "def pipeline():\n"
        "    return list(STAGES)\n"
    )
    CACHE_VERSION = (
        "STAGES = [\"build\", \"test\"]\n"
        "CACHE_DIR = \".cache\"\n"
        "def pipeline():\n"
        "    return list(STAGES)\n"
        "def cache_dir():\n"
        "    return CACHE_DIR\n"
    )
    LINT_VERSION = (
        "STAGES = [\"lint\", \"build\", \"test\"]\n"
        "def pipeline():\n"
        "    return list(STAGES)\n"
    )
    MERGED_CFG = (
        "STAGES = [\"lint\", \"build\", \"test\"]\n"
        "CACHE_DIR = \".cache\"\n"
        "def pipeline():\n"
        "    return list(STAGES)\n"
        "def cache_dir():\n"
        "    return CACHE_DIR\n"
    )
    cfg_test = (
        "import pipeline_config\n"
        "from pipeline_config import pipeline, cache_dir\n"
        "assert pipeline() == [\"lint\", \"build\", \"test\"]\n"
        "assert cache_dir() == \".cache\"\n"
        "assert pipeline() is not pipeline_config.STAGES, \"callers get a copy\"\n"
        "print(\"CONTRACT-OK\")\n"
    )
    write_task(
        directory, "heldout-integration-pipeline-merge", "integration",
        "Two branches extended pipeline_config.py: the cache branch (cache-branch/version.py) "
        "adds CACHE_DIR and cache_dir(); the lint branch (lint-branch/version.py) adds the "
        "lint stage to STAGES. Merge BOTH into the top-level pipeline_config.py so "
        "test_contract.py passes: STAGES starts with lint, cache_dir() returns \".cache\", and "
        "pipeline() still returns a fresh copy. Do not modify the branch directories or the "
        "test.",
        {
            "pipeline_config.py": BASE,
            "cache-branch/version.py": CACHE_VERSION,
            "lint-branch/version.py": LINT_VERSION,
            "test_contract.py": cfg_test,
        },
        {"pipeline_config.py": MERGED_CFG},
        "python3 -B test_contract.py",
        protected=["cache-branch/version.py", "lint-branch/version.py", "test_contract.py"],
    )


# ===========================================================================
# Self-check: setup-only fails, gold passes, every mutant killed
# ===========================================================================

def self_check(directory):
    problems = []
    for fname in sorted(os.listdir(directory)):
        if not fname.endswith(".json"):
            continue
        with open(os.path.join(directory, fname)) as f:
            spec = json.load(f)
        verify = spec["verify"].replace("{RAPID}", "rapid")
        cases = []
        if spec.get("verify_fails_before"):
            cases.append(("setup-only", dict(spec["setup"]), "must-fail"))
        cases.append(("gold", {**spec["setup"], **spec["gold"]}, "must-pass"))
        for m in spec.get("mutants", []):
            cases.append(("mutant:" + m["file"],
                          {**spec["setup"], **spec["gold"], m["file"]: m["contents"]},
                          "must-fail"))
        for label, files, expectation in cases:
            d = tempfile.mkdtemp()
            try:
                for path, contents in files.items():
                    full = os.path.join(d, path)
                    os.makedirs(os.path.dirname(full) or d, exist_ok=True)
                    with open(full, "w") as g:
                        g.write(contents)
                run = subprocess.run(["sh", "-c", verify], cwd=d, capture_output=True, text=True)
                passed = run.returncode == 0
                if expectation == "must-pass" and not passed:
                    problems.append((fname, label, "gold failed", run.stdout[-200:], run.stderr[-300:]))
                if expectation == "must-fail" and passed:
                    problems.append((fname, label, "expected failure but passed", run.stdout[-200:], run.stderr[-300:]))
            finally:
                shutil.rmtree(d)
    return problems


def main():
    for directory in (SUITE, HELDOUT):
        os.makedirs(directory, exist_ok=True)
        for path in sorted(os.listdir(directory)):
            if path.endswith(".json"):
                os.remove(os.path.join(directory, path))
    gen_navigate(SUITE, "repo")
    gen_refactor(SUITE)
    gen_context(SUITE, "repo")
    gen_errors(SUITE)
    gen_integration(SUITE)
    gen_heldout(HELDOUT)
    counts = Counter()
    for path in sorted(os.listdir(SUITE)):
        if path.endswith(".json"):
            with open(os.path.join(SUITE, path)) as f:
                counts[json.load(f)["category"]] += 1
    print("representative tasks:", sum(counts.values()), "| by category:", dict(counts))
    held = [p[:-5] for p in sorted(os.listdir(HELDOUT)) if p.endswith(".json")]
    print("held-out tasks:", len(held), held)
    for directory, name in ((SUITE, "suite"), (HELDOUT, "suite-heldout")):
        problems = self_check(directory)
        if problems:
            for problem in problems:
                print("SELF-CHECK FAIL [%s] %s %s: %s\n  out: %s\n  err: %s"
                      % ((name,) + problem))
            raise SystemExit("self-check failed for " + name)
        print("self-check [%s]: setup-only fails, gold passes, every mutant killed" % name)


if __name__ == "__main__":
    main()
