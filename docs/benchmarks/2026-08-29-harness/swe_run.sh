#!/bin/zsh
# SWE-repo-fix benchmark runner. Usage: swe_run.sh <rapid|qwen> [attempt]
set -u
B=/tmp/swe-bench
T=$1
RAPID="/Users/mohsin/projects/RapidLM CLI/target/debug/rapid"
MODEL="inclusionai/ling-3.0-flash-fin:free"
BASE="https://openrouter.ai/api/v1"
ORK=$(awk -F'"' '/^\[model\.openrouter\]/{f=1} f && /^api_key/{print $2; exit}' "$HOME/.rapidlm/config.toml")
prompt="$(cat "$B/swe_task.txt")"
d="$B/$T"
cd "$d" || exit 9
echo "== $T attempt ${2:-1} start $(date '+%H:%M:%S')"
if [[ $T == rapid ]]; then
  RAPIDLM_CONFIG=/tmp/rapid-qwen-bench/rapid-openrouter.toml \
  RAPIDLM_PERMISSION_MODE=bypassPermissions \
    /usr/bin/time -p "$RAPID" exec --verbose "$prompt" >swe_out.txt 2>swe_err.txt
else
  OPENAI_API_KEY="$ORK" OPENAI_BASE_URL="$BASE" OPENAI_MODEL="$MODEL" \
    /usr/bin/time -p qwen -p "$prompt" -o json --approval-mode=yolo >swe_out.json 2>swe_err.txt
fi
echo $? > swe_exit_code
echo "== $T attempt ${2:-1} exit $(cat swe_exit_code) $(grep '^real' swe_err.txt | tr -d '\n') end $(date '+%H:%M:%S')"
echo SWEDONE-$T
