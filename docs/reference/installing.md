# Installing

Each `v*` tag publishes a GitHub Release with one archive per supported target and a SHA-256 beside it
(`.github/workflows/release-matrix.yml`). The installers fetch exactly those.

## macOS and Linux

```sh
curl -fsSL https://raw.githubusercontent.com/moss101/Rapidlm_CLI/main/scripts/install.sh | sh
```

Prebuilt targets: Apple silicon macOS (`aarch64-apple-darwin`) and x86_64 glibc Linux
(`x86_64-unknown-linux-gnu`). Any other system is told so, with the build-from-source command.

| Option | Meaning |
|---|---|
| `--version <tag>` | a release tag such as `v0.1.0` (default: the latest release) |
| `--dir <path>` | where to put `rapid` (default: `$RAPIDLM_INSTALL_DIR`, else `~/.local/bin`) |
| `--dry-run` | print what would be fetched and installed; change nothing |
| `--target <triple>` | override target detection |
| `-h`, `--help` | usage |

To read the script before running it, download it first: `curl -fsSLO <url>`, then `sh install.sh`. With options through
a pipe: `curl -fsSL <url> | sh -s -- --version v0.1.0 --dir ~/bin`.

## Windows (PowerShell)

```powershell
irm https://raw.githubusercontent.com/moss101/Rapidlm_CLI/main/scripts/install.ps1 | iex
```

Installs `rapid.exe` to `%LOCALAPPDATA%\Programs\rapidlm`. Parameters: `-Version`, `-Dir`, `-DryRun`, and `-AddToPath`
(user PATH only). Windows on ARM runs the x86_64 binary under emulation.

## What the installers do, and do not

In order: work out the target; download the archive and its `.sha256`; **refuse to continue unless the archive's
SHA-256 is the published one**; unpack in a temporary directory and check it holds a regular `rapid`/`rapid.exe` (not
a link, not empty); copy it beside the destination and rename it into place, so a failure part-way leaves the previous
binary untouched; run `rapid --version` once; say if the directory is not on `PATH`.

They need no administrator rights, write nowhere else, edit no shell profile (and no registry key unless
`-AddToPath`), and have **no option to skip the checksum**. The version and repository names are checked against what
a tag and an `owner/name` look like before they go into a URL.

`RAPIDLM_REPO` (default `moss101/Rapidlm_CLI`) and `RAPIDLM_RELEASE_BASE` (a base URL holding the assets, `file://` or a
directory for a mirror) redirect the download; the checksum is still fetched from the same base and still checked.

## Verified by hand instead

```sh
tar -xzf rapid-aarch64-apple-darwin.tar.gz
shasum -a 256 -c rapid-aarch64-apple-darwin.tar.gz.sha256
mv rapid ~/.local/bin/
```

## From source

```sh
cargo install --path apps/rapid --locked
```

`scripts/test-install.sh` tests `install.sh` against a release laid out on local disk (`cargo test -p rapid --test
installer` runs it). `install.ps1` mirrors it step for step but has not been executed in CI.
