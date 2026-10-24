#!/bin/sh
# Tests for scripts/install.sh, run against a release laid out on local disk
# (RAPIDLM_RELEASE_BASE=file://...). Nothing is downloaded from the network.
#
#   sh scripts/test-install.sh
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
INSTALL="$HERE/install.sh"
ROOT="$(mktemp -d "${TMPDIR:-/tmp}/rapid-install-test.XXXXXX")"
trap 'rm -rf "$ROOT"' EXIT
FAILS=0

pass() { printf 'ok    %s\n' "$1"; }
bad() { printf 'FAIL  %s\n' "$1"; FAILS=$((FAILS + 1)); }
check() { # check <name> <command...>
    name="$1"; shift
    if "$@"; then pass "$name"; else bad "$name"; fi
}

if command -v sha256sum >/dev/null 2>&1; then SUM="sha256sum"; else SUM="shasum -a 256"; fi
TARGET="x86_64-unknown-linux-gnu"
ASSET="rapid-$TARGET.tar.gz"

# A release: the archive holds a `rapid` that prints a version.
make_release() { # make_release <dir> <version-text>
    dir="$1"; mkdir -p "$dir/pkg"
    printf '#!/bin/sh\necho "rapid %s"\n' "$2" > "$dir/pkg/rapid"
    chmod 755 "$dir/pkg/rapid"
    tar -C "$dir/pkg" -czf "$dir/$ASSET" rapid
    (cd "$dir" && $SUM "$ASSET" > "$ASSET.sha256")
}

run_install() { # run_install <release-dir> <args...>
    rel="$1"; shift
    RAPIDLM_RELEASE_BASE="file://$rel" sh "$INSTALL" --target "$TARGET" "$@"
}

# --- 1. the happy path -----------------------------------------------------------
REL1="$ROOT/rel1"; make_release "$REL1" "1.2.3"
DEST1="$ROOT/dest one"   # a directory name with a space, on purpose
out="$(run_install "$REL1" --dir "$DEST1" 2>&1)" || { echo "$out"; bad "install succeeds"; }
check "installs an executable rapid" test -x "$DEST1/rapid"
check "the installed binary runs" test "$("$DEST1/rapid")" = "rapid 1.2.3"
check "reports the checksum and the version" sh -c 'printf "%s" "$1" | grep -q "checksum ok" && printf "%s" "$1" | grep -q "rapid 1.2.3"' _ "$out"
check "leaves no staging file behind" test -z "$(ls -A "$DEST1" | grep -v '^rapid$' || true)"
check "warns that the directory is not on PATH" sh -c 'printf "%s" "$1" | grep -q "not on your PATH"' _ "$out"

# --- 2. a dry run changes nothing -----------------------------------------------
DEST2="$ROOT/dest2"
out="$(run_install "$REL1" --dir "$DEST2" --dry-run 2>&1)"
check "dry run installs nothing" test ! -e "$DEST2"
check "dry run says where it would install" sh -c 'printf "%s" "$1" | grep -q "dry run"' _ "$out"

# --- 3. a wrong checksum is refused and nothing is touched ----------------------
REL3="$ROOT/rel3"; make_release "$REL3" "9.9.9"
printf '%064d  %s\n' 0 "$ASSET" > "$REL3/$ASSET.sha256"
DEST3="$ROOT/dest3"; mkdir -p "$DEST3"; printf '#!/bin/sh\necho old\n' > "$DEST3/rapid"; chmod 755 "$DEST3/rapid"
if out="$(run_install "$REL3" --dir "$DEST3" 2>&1)"; then bad "a bad checksum is refused"; else pass "a bad checksum is refused"; fi
check "the message names the mismatch" sh -c 'printf "%s" "$1" | grep -q "checksum mismatch"' _ "$out"
check "the existing binary is untouched" test "$("$DEST3/rapid")" = "old"

# --- 4. a checksum file for a different asset is refused --------------------------
REL4="$ROOT/rel4"; make_release "$REL4" "1.0.0"
sed "s/$ASSET/other-asset.tar.gz/" "$REL4/$ASSET.sha256" > "$REL4/x" && mv "$REL4/x" "$REL4/$ASSET.sha256"
if run_install "$REL4" --dir "$ROOT/dest4" >/dev/null 2>&1; then bad "a checksum for another file is refused"; else pass "a checksum for another file is refused"; fi
check "and nothing was installed" test ! -e "$ROOT/dest4/rapid"

# --- 5. garbage where the checksum should be ------------------------------------
REL5="$ROOT/rel5"; make_release "$REL5" "1.0.0"
printf 'not a checksum\n' > "$REL5/$ASSET.sha256"
if run_install "$REL5" --dir "$ROOT/dest5" >/dev/null 2>&1; then bad "a malformed checksum is refused"; else pass "a malformed checksum is refused"; fi

# --- 6. an uppercase published checksum still verifies ----------------------------
REL6="$ROOT/rel6"; make_release "$REL6" "6.0.0"
# Only the hash is upper-cased; the file name keeps its case.
awk -v asset="$ASSET" 'NR==1 { print toupper($1) "  " asset }' "$REL6/$ASSET.sha256" > "$REL6/x" && mv "$REL6/x" "$REL6/$ASSET.sha256"
if run_install "$REL6" --dir "$ROOT/dest6" >/dev/null 2>&1; then pass "an upper-case checksum verifies"; else bad "an upper-case checksum verifies"; fi

# --- 7. a missing asset -----------------------------------------------------------
if run_install "$ROOT/does-not-exist" --dir "$ROOT/dest7" >/dev/null 2>&1; then bad "a missing release fails"; else pass "a missing release fails"; fi
check "and creates nothing" test ! -e "$ROOT/dest7/rapid"

# --- 8. an archive without a regular rapid ----------------------------------------
REL8="$ROOT/rel8"; mkdir -p "$REL8/pkg"; ln -s /bin/sh "$REL8/pkg/rapid"
tar -C "$REL8/pkg" -czf "$REL8/$ASSET" rapid
(cd "$REL8" && $SUM "$ASSET" > "$ASSET.sha256")
if run_install "$REL8" --dir "$ROOT/dest8" >/dev/null 2>&1; then bad "a symlink is not accepted as the binary"; else pass "a symlink is not accepted as the binary"; fi

# --- 9. an upgrade replaces the old binary --------------------------------------
REL9="$ROOT/rel9"; make_release "$REL9" "2.0.0"
run_install "$REL9" --dir "$DEST1" >/dev/null 2>&1
check "an upgrade replaces the binary" test "$("$DEST1/rapid")" = "rapid 2.0.0"

# --- 10. the inputs that become part of a URL or a path are checked ---------------
for bad_version in 'v1/../x' 'v1 x' 'v1;rm' '$(id)'; do
    if RAPIDLM_RELEASE_BASE= sh "$INSTALL" --target "$TARGET" --version "$bad_version" --dir "$ROOT/d" --dry-run >/dev/null 2>&1; then
        bad "version '$bad_version' is refused"
    else
        pass "version '$bad_version' is refused"
    fi
done
if sh "$INSTALL" --target 'x;y' --dir "$ROOT/d" --dry-run >/dev/null 2>&1; then bad "a bad target is refused"; else pass "a bad target is refused"; fi
check "a tag is accepted" sh -c 'RAPIDLM_RELEASE_BASE= sh "$1" --target "$2" --version v0.1.0 --dir "$3" --dry-run 2>&1 | grep -q "releases/download/v0.1.0/rapid-"' _ "$INSTALL" "$TARGET" "$ROOT/d"
check "latest uses the latest-download URL" sh -c 'RAPIDLM_RELEASE_BASE= sh "$1" --target "$2" --dir "$3" --dry-run 2>&1 | grep -q "releases/latest/download/rapid-"' _ "$INSTALL" "$TARGET" "$ROOT/d"

# --- 10b. a directory where the binary should go, and a binary that will not run ----
REL10="$ROOT/rel10"; make_release "$REL10" "1.0.0"
mkdir -p "$ROOT/dest10/rapid"
if run_install "$REL10" --dir "$ROOT/dest10" >/dev/null 2>&1; then bad "a directory at the destination is refused"; else pass "a directory at the destination is refused"; fi
check "and nothing was put inside it" test -z "$(ls -A "$ROOT/dest10/rapid")"
REL11="$ROOT/rel11"; mkdir -p "$REL11/pkg"
printf '#!/nonexistent/interpreter\n' > "$REL11/pkg/rapid"; chmod 755 "$REL11/pkg/rapid"
tar -C "$REL11/pkg" -czf "$REL11/$ASSET" rapid; (cd "$REL11" && $SUM "$ASSET" > "$ASSET.sha256")
out="$(run_install "$REL11" --dir "$ROOT/dest11" 2>&1 || true)"
check "a binary that cannot run is not reported as installed" sh -c '! printf "%s" "$1" | grep -q "installed   " && printf "%s" "$1" | grep -q "does not run"' _ "$out"
check "--help works when the script is piped" sh -c 'sh < "$1" 2>&1 | head -1 >/dev/null; printf "%s" "$(sh -s -- --help < "$1")" | grep -q "Install the .rapid. binary"' _ "$INSTALL"
out="$(RAPIDLM_RELEASE_BASE="file://$REL1" sh "$INSTALL" --target "$TARGET" --version v9.9.9 --dir "$ROOT/d" --dry-run 2>&1)"
check "a mirror override says it is ignoring --version" sh -c 'printf "%s" "$1" | grep -q "ignored"' _ "$out"
check "no staging file is left on failure" test -z "$(ls -A "$ROOT/dest11" 2>/dev/null | grep rapid.install || true)"

# --- 11. options ----------------------------------------------------------------
if sh "$INSTALL" --nonsense >/dev/null 2>&1; then bad "an unknown option is refused"; else pass "an unknown option is refused"; fi
check "--help prints the usage" sh -c 'sh "$1" --help | grep -q "Install the .rapid. binary"' _ "$INSTALL"

# --- 12. a platform with no prebuilt binary says what to do -----------------------
FAKEBIN="$ROOT/fakebin"; mkdir -p "$FAKEBIN"
printf '#!/bin/sh\necho Plan9\n' > "$FAKEBIN/uname"; chmod 755 "$FAKEBIN/uname"
out="$(PATH="$FAKEBIN:$PATH" sh "$INSTALL" --dir "$ROOT/d" --dry-run 2>&1 || true)"
check "an unsupported system gets a reason" sh -c 'printf "%s" "$1" | grep -q "no prebuilt binary"' _ "$out"

printf '%s\n' "----"
if [ "$FAILS" -ne 0 ]; then printf '%s failed\n' "$FAILS"; exit 1; fi
printf 'all passed\n'
