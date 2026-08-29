#!/bin/zsh
# Agentic benchmark (2026-08-29): one hard multi-part task per tool, identical
# prompt and starting state. Usage: agent_run.sh <rapid|qwen>
set -u
B=/tmp/rapid-qwen-agent
T=$1
RAPID="/Users/mohsin/projects/RapidLM CLI/target/debug/rapid"
MODEL="inclusionai/ling-3.0-flash-fin:free"
BASE="https://openrouter.ai/api/v1"
ORK=$(awk -F'"' '/^\[model\.openrouter\]/{f=1} f && /^api_key/{print $2; exit}' "$HOME/.rapidlm/config.toml")
prompt="$(cat "$B/task.txt")"
d="$B/$T"
cd "$d" || exit 9
echo "== $T start $(date '+%H:%M:%S')"
if [[ $T == rapid ]]; then
  RAPIDLM_CONFIG="$B/../rapid-qwen-bench/rapid-openrouter.toml" \
  RAPIDLM_PERMISSION_MODE=bypassPermissions \
    /usr/bin/time -p "$RAPID" exec "$prompt" >agent_out.txt 2>agent_err.txt
else
  OPENAI_API_KEY="$ORK" OPENAI_BASE_URL="$BASE" OPENAI_MODEL="$MODEL" \
    /usr/bin/time -p qwen -p "$prompt" -o json --approval-mode=yolo >agent_out.json 2>agent_err.txt
fi
echo $? > exit_code
echo "== $T exit $(cat exit_code) $(grep '^real' agent_err.txt | tr -d '\n') end $(date '+%H:%M:%S')"
echo DONE-$T
