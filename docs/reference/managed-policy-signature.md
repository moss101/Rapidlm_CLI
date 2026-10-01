# Signed managed policy

An administrator's managed policy (`RAPIDLM_MANAGED_CONFIG`, schema
`rapidlm.managed_config.v1`) can be required to carry a detached Ed25519
signature. The check happens where the policy is read, so every entry point
that loads it — an interactive session, `rapid exec`, `rapid mcp`,
`rapid plugin`, `rapid setup`, `rapid doctor` — refuses an unverified policy
the same way, before a session starts or anything is planned. A configured policy that fails the check is never treated as "no policy".

## Turning it on

| Variable | Meaning |
| --- | --- |
| `RAPIDLM_MANAGED_TRUSTED_KEYS` | Path to the trusted-keys file. When set, the policy **must** have a valid signature by one of these keys. |
| `RAPIDLM_MANAGED_REQUIRE_SIGNATURE` | Any value other than empty, `0` or `false` demands a signature. With no trusted keys configured this is an error, not an unsigned pass — it catches a deployment that forgot to provision the keys. |

The trust anchor lives **outside** the signed document: a policy cannot vouch
for itself. Provision the variables and the keys file where the user cannot
edit them, exactly as you provision `RAPIDLM_MANAGED_CONFIG` itself.

## Files

* **Trusted keys** — one key per line, `#` comments and blank lines allowed, at
  most 32 keys, none repeated, at most 16 KiB:

  ```
  # release-engineering, 2026
  ed25519:d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a
  ```

  A line that is not a comment, blank or a key is an error — it is never
  skipped. Keeping two keys trusted at once lets you rotate.
* **Signature** — `<policy path>.sig` beside the policy: `ed25519:` followed by
  the 128 hex digits of the 64-byte signature, at most 1 KiB. The signature is
  over the policy file's **exact bytes**: one changed character, or a trailing
  newline, invalidates it. A signature names no key; each trusted key is tried.

## Signing

The product only verifies; it holds no signing code and never sees a private
key. Sign with any Ed25519 tool. For example with Python's `cryptography`
package (with the RFC 8032 test seed, these calls give that RFC's public key and signature):

```python
from pathlib import Path
from cryptography.hazmat.primitives import serialization as s
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

key = Ed25519PrivateKey.generate()          # keep this off the verifying machines
public = key.public_key().public_bytes(s.Encoding.Raw, s.PublicFormat.Raw)
print("ed25519:" + public.hex())            # one line of the trusted-keys file

policy = Path("managed.toml")
Path("managed.toml.sig").write_text("ed25519:" + key.sign(policy.read_bytes()).hex() + "\n")
```

A signature file is `ed25519:` and the signature's hex, whatever tool made it.

## What `rapid doctor` shows

A `managed` row: the policy's origin (its path), whether its signature was
verified (and the fingerprint of the key that matched — the first 8 bytes of
the key's SHA-256) or "not checked" when no trusted keys are configured, and
the policy's content version. A configured policy that does not load, or whose
required signature does not verify, is a failure with its remediation.

## Limits

* The trust anchor is the process environment, so it holds only where the user
  cannot change the environment of the process that runs `rapid`: a user who can
  unset `RAPIDLM_MANAGED_TRUSTED_KEYS` can also unset `RAPIDLM_MANAGED_CONFIG`.
  A trusted-keys variable that is set but blank is an error, not "no keys".
* A signature is over bytes, not a path or a time: any older policy you signed,
  or any file you signed and then renamed to the configured path, verifies.
  There is no replay or rollback protection; `policy_version` (shown by
  `rapid doctor`) lets you see which document is live.

* Only the policy document is signed. The keys file and the environment are the
  administrator's to protect.
* A signature proves the policy came from a holder of a trusted key, not that it
  is current: there is no expiry or revocation beyond removing the key from the
  trusted set. Rotate by trusting the new key, re-signing, then dropping the
  old one.
* The update manifest's `signatures` field is not verified by this mechanism.
