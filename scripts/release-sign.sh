#!/bin/sh
# Release signing and verification via ssh-keygen ed25519 signatures
# (available on macOS/Linux by default; no external infrastructure).
#
# Sign:   RELEASE_KEY=~/.ssh/rapid-release-ed25519 scripts/release-sign.sh sign <manifest-file>
# Verify: scripts/release-sign.sh verify <manifest-file> <signature-file> <public-key>
#
# The signature covers the manifest's bytes — which include the artifact
# digests, the SBOM, and the provenance block — so verifying it authenticates
# the whole release record at once.
set -eu

cmd="${1:-}"
shift || true

case "$cmd" in
  sign)
    manifest="${1:?usage: release-sign.sh sign <manifest-file> [key]}"
    key="${2:-${RELEASE_KEY:-$HOME/.ssh/rapid-release-ed25519}}"
    if [ ! -f "$key" ]; then
      ssh-keygen -t ed25519 -f "$key" -N "" -C "rapid-release-signing" >/dev/null
      echo "generated release key: $key (+ .pub)"
    fi
    ssh-keygen -Y sign -f "$key" -n rapid-release "$manifest" >/dev/null
    echo "signed: $manifest.sig"
    ;;
  verify)
    manifest="${1:?usage: release-sign.sh verify <manifest-file> <sig> <pub-key>}"
    sig="${2:?usage: release-sign.sh verify <manifest-file> <sig> <pub-key>}"
    pubkey="${3:?usage: release-sign.sh verify <manifest-file> <sig> <pub-key>}"
    allowed="$(mktemp)"
    printf 'rapid-release %s\n' "$(cat "$pubkey")" > "$allowed"
    ssh-keygen -Y verify -f "$allowed" -I rapid-release \
      -n rapid-release -s "$sig" < "$manifest"
    rm -f "$allowed"
    echo "verified: $manifest signature matches $pubkey"
    ;;
  *)
    echo "usage: release-sign.sh sign <manifest-file> [key] | verify <manifest> <sig> <pub>" >&2
    exit 2
    ;;
esac
