#!/bin/zsh
# Re-run (2026-08-29) of the 2026-08-28 qwen-vs-rapid benchmark.
# Identical prompts through rapid exec and qwen -p (same OpenRouter model),
# capturing wall time (/usr/bin/time -p), stdout, stderr, and exit codes.
# rapid is pinned to the openrouter config entry via RAPIDLM_CONFIG because
# ~/.rapidlm/config.toml now defaults to a different provider (b-ai).
set -u
B=/tmp/rapid-qwen-bench
RAPID="/Users/mohsin/projects/RapidLM CLI/target/debug/rapid"
MODEL="inclusionai/ling-3.0-flash-fin:free"
BASE="https://openrouter.ai/api/v1"
ORK=$(awk -F'"' '/^\[model\.openrouter\]/{f=1} f && /^api_key/{print $2; exit}' "$HOME/.rapidlm/config.toml")
mkdir -p "$B"
cat > "$B/rapid-openrouter.toml" <<EOF
[models]
default = "openrouter"

[model.openrouter]
provider = "openai-compatible"
model = "$MODEL"
base_url = "$BASE"
api_key = "$ORK"
EOF

for p in p1 p2 p3 p4 probe; do
  for cli in rapid qwen; do
    d="$B/$p/$cli"
    rm -rf "$d"; mkdir -p "$d"
    prompt="$(cat "$B/$p.txt" 2>/dev/null)"
    if [[ $p == probe ]]; then prompt="Reply with exactly one word: pong"; fi
    echo "== $p $cli start $(date '+%H:%M:%S')"
    if [[ $cli == rapid ]]; then
      ( cd "$d" && RAPIDLM_CONFIG="$B/rapid-openrouter.toml" /usr/bin/time -p "$RAPID" exec "$prompt" >stdout.txt 2>time_err.txt; echo $? >exit_code )
    else
      ( cd "$d" && OPENAI_API_KEY="$ORK" OPENAI_BASE_URL="$BASE" OPENAI_MODEL="$MODEL" \
          /usr/bin/time -p qwen -p "$prompt" -o json >stdout.json 2>time_err.txt; echo $? >exit_code )
    fi
    echo "== $p $cli exit $(cat "$d/exit_code") $(grep '^real' "$d/time_err.txt" | tr -d '\n')"
    sleep 8
  done
done
echo ALLDONE
