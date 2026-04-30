# Security

## Reporting a vulnerability

If you find a security issue in fast-grep, **do not open a public issue**.
Email Gaston Milano at <gmilano@genexus.com> with a description and, if
possible, a minimal reproduction. We aim to acknowledge within 48 hours.

If the issue is in a transitive dependency, please also report it upstream
to the affected crate so the broader Rust ecosystem benefits.

## Supported versions

We provide fixes for the latest published version on crates.io. Older
versions are not patched — pin to the latest minor.

## Threat model (summary)

fast-grep is a **local CLI tool** that operates on files the running user
already has read access to. The trust boundary is the local filesystem:
fast-grep does not authenticate paths, network endpoints, or running
processes beyond what the OS already enforces.

**In scope (we treat as bugs):**
- Memory-safety violations beyond the documented `unsafe { Mmap::map }`
  contract.
- Path-traversal where a CLI argument escapes the user-supplied root.
- Daemon TCP commands that allow a co-located process to do anything
  beyond stop / status / flush against the index in the user's own home.
- XSS / supply-chain on the documentation site at
  <https://gmilano.github.io/fast-grep-rust/>.
- Crashes / panics on attacker-controlled regex patterns or attacker-
  controlled file content.

**Explicitly out of scope:**
- Tampering with the on-disk index (`.fgr/`). The index is a cache; if
  another local user with write access corrupts it, fast-grep may panic on
  load. The fix is to delete the directory and rebuild. We do not consider
  this a security issue — same trust model as any other on-disk cache.
- DoS via huge files. fast-grep mmaps files; reading a deliberately
  fragmented multi-GB file can be slow. Use OS quotas or pre-filter the
  walk via `--type` / `--exclude`.
- mmap SIGBUS when a file is truncated mid-read. This is an
  unavoidable consequence of mmap-based readers (ripgrep has it too).
  Crashing fast-grep does not affect any other process.

## Hardening notes

- The daemon's TCP listener binds to `127.0.0.1` only. The command set is
  closed: `status`, `flush`, `stop`. There is no path or pattern argument
  passed over the socket — anything else returns `error: unknown command`.
- The CLI does not follow symlinks by default (`ignore::WalkBuilder`'s
  default).
- `cargo-audit` runs on every push and PR (`.github/workflows/ci.yml`)
  and fails the build on any vulnerability advisory. Two transitive
  unmaintained-only warnings (`instant`, `paste`) are tracked and
  suppressed in `.cargo/audit.toml`; they are not vulnerabilities.
- The interactive site loads Mermaid from jsDelivr pinned to an exact
  version with an SRI integrity hash, so a registry compromise cannot
  inject arbitrary JS into visitors' browsers.
- CI workflow runs with `permissions: contents: read` — the build cannot
  push code, comment on PRs, or write packages. The release workflow
  scopes `permissions: contents: write` only because it must publish a
  GitHub Release on tag push.

## Known accepted risks

| Item | Why we accept it |
|------|------------------|
| `unsafe { Mmap::map(&file) }` | Required by `memmap2`. We document the contract: do not rebuild the index in-place while a search is running. |
| No CRC validation at index load | Index is a local cache. A user able to overwrite it can already do worse to their own filesystem. |
| `instant` (transitive, unmaintained) | Pulled in by `notify` 7.x for the daemon FS watcher. Re-evaluate when `notify` removes the dependency. |
| `paste` (transitive, unmaintained) | Pulled in by `metal` 0.29 for the macOS GPU verify scaffold. Re-evaluate when `metal-rs` removes it. |
