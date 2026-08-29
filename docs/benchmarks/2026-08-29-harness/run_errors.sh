#!/bin/zsh
# Error-handling matrix (2026-08-29 re-run): bad key / unreachable base URL /
# unknown model, both CLIs. Key always read from the openrouter config section.
set -u
B=/tmp/rapid-qwen-bench/err
RAPID="/Users/mohsin/projects/RapidLM CLI/target/debug/rapid"
MODEL="inclusionai/ling-3.0-flash-fin:free"
BASE="https://openrouter.ai/api/v1"
ORK=$(awk -F'"' '/^\[model\.openrouter\]/{f=1} f && /^api_key/{print $2; exit}' "$HOME/.rapidlm/config.toml")
rm -rf "$B"; mkdir -p "$B"

# Build rapid config variants (key never printed).
python3 - "$ORK" <<'EOF'
import sys, os
key = sys.argv[1]
os.makedirs("/tmp/rapid-qwen-bench/err", exist_ok=True)
def write(name, model, base, apikey):
    with open(f"/tmp/rapid-qwen-bench/err/{name}.toml", "w") as f:
        f.write(f'[models]\ndefault = "openrouter"\n\n[model.openrouter]\nprovider = "openai-compatible"\nmodel = "{model}"\nbase_url = "{base}"\napi_key = "{apikey}"\n')
write("badkey", "inclusionai/ling-3.0-flash-fin:free", "https://openrouter.ai/api/v1", "sk-or-v1-0000000000000000000000000000000000000000000000000000")
write("unreach", "inclusionai/ling-3.0-flash-fin:free", "http://127.0.0.1:9", key)
write("badmodel", "not a valid model!!", "https://openrouter.ai/api/v1", key)
EOF

run_rapid() { # name config
  ( cd "$B" && RAPIDLM_CONFIG="$B/$2.toml" /usr/bin/time -p "$RAPID" exec "Reply with exactly one word: pong" >"rapid_$1.out" 2>"rapid_$1.err"; echo $? >"rapid_$1.code" )
}
run_qwen() { # name model base key
  ( cd "$B" && OPENAI_API_KEY="$4" OPENAI_BASE_URL="$3" OPENAI_MODEL="$2" qwen -p "Reply with exactly one word: pong" -o json >"qwen_$1.out" 2>"qwen_$1.err"; echo $? >"qwen_$1.code" )
}

run_rapid badkey badkey
run_qwen badkey "$MODEL" "$BASE" "sk-invalid-000"
run_rapid unreach unreach
run_qwen unreach "$MODEL" "http://127.0.0.1:9" "$ORK"
run_rapid badmodel badmodel
run_qwen badmodel "no-such-vendor/no-such-model:free" "$BASE" "$ORK"

echo MATRIXDONE
for f in "$B"/*.code; do echo "$f=$(cat $f)"; done
