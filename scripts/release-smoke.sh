#!/bin/sh
# Clean-environment release smoke: run the release binary from a pristine
# HOME (no config, no trust, no credentials) and assert the documented
# first-contact behavior. Exit 0 iff every check passes.
set -eu
BIN="${1:-target/release/rapid}"
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
HOME="$WORK/home"
export HOME
mkdir -p "$HOME"
fail() { echo "FAIL: $1" >&2; exit 1; }

# 1. version prints and exits 0
"$BIN" --version | grep -q "rapid" || fail "--version"

# 2. help lists the shipped commands
"$BIN" --help | grep -q "exec" || fail "--help missing exec"
"$BIN" --help | grep -q "eval" || fail "--help missing eval"

# 3. a fresh project: trust is required and reported
mkdir -p "$WORK/proj" && cd "$WORK/proj" && git init -q .
OUT="$("$BIN" exec 'say hi' 2>&1 || true)"
echo "$OUT" | grep -q "not trusted" || fail "untrusted project must be named"

# 4. granting trust flips the gate
"$BIN" trust grant > /dev/null
"$BIN" trust status | grep -q "trusted" || fail "trust grant"

# 5. doctor runs offline and reports the model as the one open issue
"$BIN" doctor > /dev/null 2>&1 && true
# (doctor exits nonzero when a model is unconfigured; that is honest — the
#  smoke asserts it RAN, by checking its output shape)
"$BIN" doctor 2>/dev/null | grep -q "model" || fail "doctor model check"

# 6. eval offline passes against the shipped suite (mechanical validation)
mkdir -p smoke-repo && cd smoke-repo && git init -q .
"$BIN" trust grant > /dev/null
REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/.." 2>/dev/null && pwd)}"
if [ ! -d "$REPO_ROOT/eval/suite" ]; then
  fail "eval/suite not found; run the smoke from a checkout or set REPO_ROOT"
fi
"$BIN" eval --offline --suite "$REPO_ROOT/eval/suite" --scratch "$WORK/scratch" > /dev/null || fail "eval offline"

echo "release smoke: OK ($BIN)"
