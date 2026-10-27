# CI with key isolation

The pattern: `rapid exec` in CI runs against repository-controlled code —
prompts, scripts, playbooks, anything in the checkout. If the provider key
sat in the environment or the config the run reads, repository-controlled
code could read it. Instead the key lives **only in the proxy**:

```
checkout ── rapid exec ──> 127.0.0.1:8787 (key-proxy) ──> the provider
     holds a dummy token        holds the real key            one origin
```

## The pieces

| File | What it is |
|---|---|
| `key-proxy.py` | Stdlib-only local forward proxy. Injects `Authorization: Bearer $KEY` toward exactly one upstream origin, replaces whatever the caller sent (the dummy token rapid holds is useless outside), refuses `CONNECT` (no open relay), never logs headers. |
| `action.yml` | A composite GitHub Action: starts the proxy with the `api-key` secret, writes rapid a config pointing at the proxy with a dummy token, runs `rapid exec --plan` on the prompt input. |

## Use in a workflow

```yaml
- uses: your-org/your-repo/.github/actions/rapid@main   # vendored: copy examples/ci
  with:
    prompt: "Fix the failing CI job in this repository and open the explanation as a comment."
    api-key: ${{ secrets.PROVIDER_KEY }}
```

The run's rapid configuration (`$RAPIDLM_HOME/config.toml`, written by the
action) names only `http://127.0.0.1:8787/v1` and a fixed dummy key:
nothing in the checkout — including anything the model itself writes — can
recover the real credential, because no process in the job besides the
proxy ever holds it.

The proxy's own behavior is tested in the repository's test suite
(`apps/rapid/tests/ci_key_proxy.rs`): it injects the key upstream,
discards the caller's credentials, and answers `CONNECT` with a refusal.
