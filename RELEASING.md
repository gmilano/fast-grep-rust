# Releasing fast-grep

How to cut and publish a new version. Most of the work is automated by
`.github/workflows/release.yml` — this doc covers the human steps before and
after the tag.

## TL;DR

```bash
# 1. on master, make sure everything builds
cargo check
cargo test

# 2. bump version in Cargo.toml (and any version strings in README that aren't auto-updated)
$EDITOR Cargo.toml         # bump `version = "X.Y.Z"`
cargo check                # refreshes Cargo.lock with the new version
$EDITOR README.md          # bump the .deb URL line if its version is hardcoded

# 3. commit + push
git add Cargo.toml Cargo.lock README.md
git commit -m "release: X.Y.Z — <one-line summary>"
git push origin master

# 4. tag + push (this fires the release workflow)
git tag -a vX.Y.Z -m "fast-grep X.Y.Z"
git push origin vX.Y.Z

# 5. wait ~10 min for the workflow, then update Homebrew tap and Scoop bucket
gh run watch --repo gmilano/fast-grep-rust  # or just open Actions in the browser
OUTPUT=/tmp/fast-grep.rb   packaging/homebrew/update-formula.sh vX.Y.Z
OUTPUT=/tmp/fast-grep.json packaging/scoop/update-manifest.sh   vX.Y.Z
# (then commit /tmp/fast-grep.rb to gmilano/homebrew-fast-grep
#  and /tmp/fast-grep.json to gmilano/scoop-fast-grep)
```

## Versioning

Semver. Roughly:

- patch (`0.2.0` → `0.2.1`): bug fixes, doc-only changes
- minor (`0.2.0` → `0.3.0`): new feature, new flag, new subcommand
- major (`0.x` → `1.0`): breaking CLI/index format change

If the index file format changes in a non-backward-compatible way, bump
minor (pre-1.0) or major (post-1.0) **and** mention it in the release notes —
users will need to rebuild their `.fgr` indices.

## What the workflow does on tag push

`.github/workflows/release.yml` triggers on any tag matching `v*.*.*`:

1. **Resolves version** from the tag.
2. **Builds 7 binaries** in parallel:
   - macOS: `x86_64-apple-darwin`, `aarch64-apple-darwin`
   - Linux: `x86_64-unknown-linux-{gnu,musl}`, `aarch64-unknown-linux-{gnu,musl}`
   - Windows: `x86_64-pc-windows-msvc`
   Archives are named `fast-grep-vX.Y.Z-<target>.{tar.gz,zip}` (cargo-binstall
   compatible) with `.sha256` sidecars.
3. **Builds 2 `.deb` packages** (amd64, arm64) via `cargo-deb`. Cross-arch
   builds use `--no-strip` and a hardcoded `libc6` dependency to avoid
   `dpkg-shlibdeps` and the host's `strip` choking on foreign-arch ELF.
4. **Creates the GitHub Release** with all archives, `.deb` files, `.sha256`
   sidecars, and a combined `SHA256SUMS`.
5. **Publishes to crates.io** (idempotent — silently skipped if the version
   is already there).

Total runtime: ~10 minutes.

## After the workflow finishes

The release workflow does **not** push to the Homebrew tap or Scoop bucket —
those are external repos and need manual updates with the real SHA256s.

```bash
# Homebrew tap
git clone https://github.com/gmilano/homebrew-fast-grep /tmp/tap
OUTPUT=/tmp/tap/Formula/fast-grep.rb \
    packaging/homebrew/update-formula.sh vX.Y.Z
(cd /tmp/tap && git add Formula/fast-grep.rb \
   && git commit -m "fast-grep X.Y.Z" && git push)

# Scoop bucket
git clone https://github.com/gmilano/scoop-fast-grep /tmp/bucket
OUTPUT=/tmp/bucket/fast-grep.json \
    packaging/scoop/update-manifest.sh vX.Y.Z
(cd /tmp/bucket && git add fast-grep.json \
   && git commit -m "fast-grep X.Y.Z" && git push)
```

## Verifying

```bash
# crates.io
curl -fsS https://crates.io/api/v1/crates/fast-grep | jq '.crate.max_version'

# Homebrew tap
curl -fsS https://raw.githubusercontent.com/gmilano/homebrew-fast-grep/master/Formula/fast-grep.rb \
    | grep -E '^\s+version'

# Scoop bucket
curl -fsS https://raw.githubusercontent.com/gmilano/scoop-fast-grep/master/fast-grep.json \
    | jq -r .version

# Try a clean install on a throwaway machine / container
cargo install fast-grep && fgr --version
```

## Common failure modes

### `build .deb (aarch64-...)` fails with `dpkg-shlibdeps` or `strip` errors

You changed the deb metadata or removed `--no-strip`. The aarch64 deb build
runs on an x86_64 host, so:

- `depends = "$auto"` will fail because `dpkg-shlibdeps` can't resolve
  aarch64 shared libs on the host. Keep it as `depends = "libc6"` (or
  whatever shared libs we actually link against).
- `cargo deb` without `--no-strip` will invoke the host's x86_64 `strip` on
  the aarch64 ELF, which fails. Keep `--no-strip` in the workflow step.

### `publish to crates.io` fails

Likely causes:

- `CRATES_IO_TOKEN` repo secret is unset, expired, or revoked. Generate a new
  token at <https://crates.io/me/tokens> with scopes `publish-new` and
  `publish-update`, then:
  ```bash
  printf '%s' '<token>' | gh secret set CRATES_IO_TOKEN \
      --repo gmilano/fast-grep-rust
  ```
- A new dependency was added without verifying it's `cargo publish`-able
  (e.g. path or git deps in `[dependencies]`). Run `cargo publish --dry-run`
  locally before tagging.
- The version is already on crates.io. The workflow detects this and exits 0
  on its own — if it doesn't, the matcher in the workflow's "cargo publish"
  step needs updating to match the new error string.

### Cross-compiled binary fails to run on the user's machine

We strip `RUSTFLAGS` in CI so distributed binaries don't bake in
`target-cpu=native`. If you add target-specific rustflags to
`.cargo/config.toml`, make sure they don't accidentally apply to release
builds — the `RUSTFLAGS=""` env in the workflow only overrides
`build.rustflags`, not `target.<triple>.rustflags`.

### Push fails with `Permission denied to gaston-milano_globant`

Two GitHub accounts in `gh` keychain. Switch the active account and push
with the explicit token, or fix the OS keychain entry once:

```bash
gh auth switch -u gmilano
tok=$(gh auth token -u gmilano)
git push "https://x-access-token:${tok}@github.com/gmilano/fast-grep-rust.git" master
git push "https://x-access-token:${tok}@github.com/gmilano/fast-grep-rust.git" vX.Y.Z
```

## First-time setup (one-shot, already done for v0.1.0)

Captured here so a future maintainer can re-bootstrap the channels:

1. **crates.io account**: log in with GitHub at <https://crates.io>, verify
   email at <https://crates.io/settings/profile>, create an API token, then
   `cargo login <token>` and `cargo publish` once to reserve the name.
2. **Homebrew tap repo**: `gh repo create gmilano/homebrew-fast-grep --public`,
   commit `Formula/fast-grep.rb` (the template in `packaging/homebrew/`).
3. **Scoop bucket repo**: `gh repo create gmilano/scoop-fast-grep --public`,
   commit `fast-grep.json` (the template in `packaging/scoop/`).
4. **Repo secret**: `printf '%s' '<crates-io-token>' | gh secret set CRATES_IO_TOKEN -R gmilano/fast-grep-rust`.
