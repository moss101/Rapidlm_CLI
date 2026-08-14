# Headless JSONL Protocol

`rapid run --jsonl` writes only protocol records to stdout. Logs/diagnostics use stderr. UTF-8, one JSON object per line, no pretty printing.

## Base record

```json
{"schema":1,"type":"session.started","session_id":"...","seq":1,"time":"...","data":{}}
```

Required types include `session.*`, `turn.*`, `assistant.delta`, `assistant.message`, `tool.*`, `approval.required`, `agent.*`, `goal.*`, `evidence.*`, `artifact.created`, `error`, and `session.finished`.

Exit codes:
- `0`: requested run completed successfully;
- `2`: usage/config error;
- `3`: policy/approval prevented requested operation;
- `4`: provider/auth unavailable;
- `5`: runtime/internal failure;
- `6`: goal blocked/incomplete under `--require-complete`;
- `130`: user/host interrupt.

Unknown record types must be ignorable by v1 consumers. `assistant.delta` may be disabled with `--no-stream-events` without changing semantic lifecycle events.
