#!/bin/sh
# Install the `rapid` binary from a GitHub Release.
#
#   curl -fsSL https://raw.githubusercontent.com/moss101/Rapidlm_CLI/main/scripts/install.sh | sh
#   sh install.sh --version v0.1.0 --dir "$HOME/bin"
#
# What it does, in order, and nothing else:
#   1. works out which prebuilt target this machine can run;
#   2. downloads that target's archive and its .sha256 from the release;
#   3. refuses to go on unless the archive's SHA-256 is the one published;
#   4. unpacks it in a temporary directory and checks it holds a regular `rapid`;
#   5. installs it atomically (a copy beside the old file, then a rename), so a
#      failure part-way leaves the previous binary untouched;
#   6. runs `rapid --version` once, and tells you if the directory is not on PATH.
# It needs no root, writes nowhere else, edits no shell profile, and there is no
# option to skip step 3.
#
# Options
#   --version <tag>   a release tag such as v0.1.0 (default: the latest release)
#   --dir <path>      where to put `rapid` (default: $RAPIDLM_INSTALL_DIR, else ~/.local/bin)
#   --target <triple> override target detection (for a mirror or a test)
#   --dry-run         say what would be downloaded and installed; change nothing
#   --force           replace a `rapid` that is already there without comment
#   -h, --help        this text
#
# Environment
#   RAPIDLM_REPO          owner/name of the repository (default: moss101/Rapidlm_CLI)
#   RAPIDLM_RELEASE_BASE  base URL holding the assets, replacing the GitHub release URL.
#                         A file:// URL works, for a mirror on disk.
#   RAPIDLM_INSTALL_DIR   default for --dir

set -eu

REPO="${RAPIDLM_REPO:-moss101/Rapidlm_CLI}"
VERSION="latest"
DIR="${RAPIDLM_INSTALL_DIR:-}"
TARGET=""
DRY_RUN=0
FORCE=0

say() { printf '%s\n' "$*"; }
fail() { printf 'install: %s\n' "$*" >&2; exit 1; }

usage() {
    # The block of comments at the top, without the shebang and the comment marks.
    sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version) [ $# -ge 2 ] || fail "--version needs a value"; VERSION="$2"; shift 2 ;;
        --version=*) VERSION="${1#--version=}"; shift ;;
        --dir) [ $# -ge 2 ] || fail "--dir needs a value"; DIR="$2"; shift 2 ;;
        --dir=*) DIR="${1#--dir=}"; shift ;;
        --target) [ $# -ge 2 ] || fail "--target needs a value"; TARGET="$2"; shift 2 ;;
        --target=*) TARGET="${1#--target=}"; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        --force) FORCE=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) fail "unknown option: $1 (try --help)" ;;
    esac
done

# --- a tag is data we put in a URL: keep it to what a tag looks like ----------
case "$VERSION" in
    latest) ;;
    *[!A-Za-z0-9._-]*|"") fail "the version '$VERSION' is not a release tag" ;;
esac
case "$REPO" in
    */*) ;;
    *) fail "RAPIDLM_REPO must be owner/name" ;;
esac
case "$REPO" in
    *[!A-Za-z0-9._/-]*) fail "RAPIDLM_REPO has characters a repository name cannot" ;;
esac

# --- which target -------------------------------------------------------------
detect_target() {
    os="$(uname -s 2>/dev/null || echo unknown)"
    arch="$(uname -m 2>/dev/null || echo unknown)"
    case "$os" in
        Darwin)
            case "$arch" in
                arm64|aarch64) echo aarch64-apple-darwin ;;
                *) echo "unsupported:there is no prebuilt binary for $os/$arch; build from source with: cargo install --path apps/rapid --locked" ;;
            esac ;;
        Linux)
            case "$arch" in
                x86_64|amd64)
                    if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
                        echo "unsupported:the prebuilt Linux binary is built for glibc and this system uses musl; build from source with: cargo install --path apps/rapid --locked"
                    else
                        echo x86_64-unknown-linux-gnu
                    fi ;;
                *) echo "unsupported:there is no prebuilt binary for $os/$arch; build from source with: cargo install --path apps/rapid --locked" ;;
            esac ;;
        MINGW*|MSYS*|CYGWIN*)
            echo "unsupported:on Windows use scripts/install.ps1 from PowerShell" ;;
        *) echo "unsupported:there is no prebuilt binary for $os/$arch" ;;
    esac
}

if [ -z "$TARGET" ]; then
    detected="$(detect_target)"
    case "$detected" in
        unsupported:*) fail "${detected#unsupported:}" ;;
        *) TARGET="$detected" ;;
    esac
fi
case "$TARGET" in
    *[!A-Za-z0-9._-]*|"") fail "the target '$TARGET' is not a target triple" ;;
esac

# --- where from, where to ------------------------------------------------------
ASSET="rapid-${TARGET}.tar.gz"
if [ -n "${RAPIDLM_RELEASE_BASE:-}" ]; then
    BASE="${RAPIDLM_RELEASE_BASE%/}"
elif [ "$VERSION" = "latest" ]; then
    BASE="https://github.com/${REPO}/releases/latest/download"
else
    BASE="https://github.com/${REPO}/releases/download/${VERSION}"
fi

if [ -z "$DIR" ]; then
    [ -n "${HOME:-}" ] || fail "HOME is not set; pass --dir"
    DIR="$HOME/.local/bin"
fi

say "rapid: ${VERSION} for ${TARGET}"
say "  from  ${BASE}/${ASSET}"
say "  to    ${DIR}/rapid"
if [ "$DRY_RUN" -eq 1 ]; then
    say "(dry run: nothing was downloaded or installed)"
    exit 0
fi

# --- tools ---------------------------------------------------------------------
if command -v curl >/dev/null 2>&1; then
    FETCH=curl
elif command -v wget >/dev/null 2>&1; then
    FETCH=wget
else
    fail "curl or wget is needed to download the release"
fi
if command -v sha256sum >/dev/null 2>&1; then
    SHA=sha256sum
elif command -v shasum >/dev/null 2>&1; then
    SHA="shasum -a 256"
else
    fail "sha256sum or shasum is needed to verify the download"
fi

fetch() {
    # fetch <url> <output-file>
    case "$1" in
        file://*) cp "${1#file://}" "$2" ;;
        *)
            if [ "$FETCH" = curl ]; then
                curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 --output "$2" "$1"
            else
                wget --quiet --https-only --output-document="$2" "$1"
            fi ;;
    esac
}

WORK="$(mktemp -d "${TMPDIR:-/tmp}/rapid-install.XXXXXX")" || fail "could not make a temporary directory"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT INT TERM HUP

# --- download and verify -------------------------------------------------------
fetch "${BASE}/${ASSET}" "$WORK/$ASSET" || fail "could not download ${BASE}/${ASSET}"
fetch "${BASE}/${ASSET}.sha256" "$WORK/$ASSET.sha256" || fail "could not download ${BASE}/${ASSET}.sha256"

expected="$(awk 'NR==1 {print $1}' "$WORK/$ASSET.sha256")"
named="$(awk 'NR==1 {print $2}' "$WORK/$ASSET.sha256" | sed 's/^\*//')"
case "$expected" in
    *[!0-9a-fA-F]*|"") fail "the published checksum file does not hold a SHA-256" ;;
esac
[ "${#expected}" -eq 64 ] || fail "the published checksum is not 64 hex digits"
if [ -n "$named" ] && [ "$named" != "$ASSET" ]; then
    fail "the checksum file is for '$named', not '$ASSET'"
fi
actual="$($SHA "$WORK/$ASSET" | awk '{print $1}')"
# Compared in lower case: sha256sum prints lower case, a hand-edited file may not.
expected_lc="$(printf '%s' "$expected" | tr 'A-F' 'a-f')"
if [ "$actual" != "$expected_lc" ]; then
    fail "checksum mismatch for $ASSET: expected $expected_lc, got $actual. Nothing was installed."
fi
say "  checksum ok  $actual"

# --- unpack ---------------------------------------------------------------------
mkdir "$WORK/unpacked"
tar -xzf "$WORK/$ASSET" -C "$WORK/unpacked" || fail "could not unpack $ASSET"
if [ ! -f "$WORK/unpacked/rapid" ] || [ -L "$WORK/unpacked/rapid" ]; then
    fail "the archive does not hold a regular file named rapid"
fi
[ -s "$WORK/unpacked/rapid" ] || fail "the archive's rapid is empty"
chmod 0755 "$WORK/unpacked/rapid"

# --- install --------------------------------------------------------------------
if [ -e "$DIR/rapid" ] && [ "$FORCE" -eq 0 ]; then
    say "  replacing the rapid already in $DIR"
fi
mkdir -p "$DIR" || fail "could not create $DIR"
# Beside the destination, so the final step is a rename within one filesystem.
STAGED="$DIR/.rapid.install.$$"
cp "$WORK/unpacked/rapid" "$STAGED" || { rm -f "$STAGED"; fail "could not write to $DIR"; }
chmod 0755 "$STAGED"
mv -f "$STAGED" "$DIR/rapid" || { rm -f "$STAGED"; fail "could not move rapid into place in $DIR"; }

installed="$("$DIR/rapid" --version 2>&1 | head -n 1 || true)"
say "  installed    ${installed:-rapid}"

case ":${PATH:-}:" in
    *":$DIR:"*) ;;
    *)
        say ""
        say "$DIR is not on your PATH. Add it, for example:"
        say "  export PATH=\"$DIR:\$PATH\""
        ;;
esac
