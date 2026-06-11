# fast-grep

> Indexed regex search. 6–25x faster than ripgrep, 2–10x faster than ugrep.
> **Interactive walkthrough + C4 diagrams + benchmarks → [gmilano.github.io/fast-grep-rust](https://gmilano.github.io/fast-grep-rust/)**

Built at **[Globant](https://www.globant.com)** for agent harnesses and large codebases where grep is the bottleneck. A one-time index build turns every subsequent search into a sub-200ms lookup instead of a multi-second full scan. An optional background daemon keeps the index in sync with the filesystem so it never goes stale.

## Benchmarks — Linux kernel 6.6 (81,690 files)

**Apple M1 Pro, 32 GB RAM — warm cache**

### vs ripgrep (no index)

| Pattern | fast-grep | ripgrep | Speedup |
|---------|-----------|---------|---------|
| `TODO` | **97ms** | 2,463ms | **25x** |
| `printk` | **172ms** | 2,492ms | **14x** |
| `EXPORT_SYMBOL` | **197ms** | 1,553ms | **8x** |
| `container_of` | **344ms** | 2,440ms | **7x** |
| `static.*inline` | **394ms** | 2,369ms | **6x** |

### vs ugrep (indexed)

| Pattern | fast-grep | ugrep | Speedup |
|---------|-----------|-------|---------|
| `EXPORT_SYMBOL` | **197ms** | 1,898ms | **9.6x** |
| `TODO` | **97ms** | 599ms | **6.2x** |
| `static.*inline` | **394ms** | 1,595ms | **4.0x** |
| `printk` | **172ms** | 645ms | **3.8x** |
| `container_of` | **344ms** | 656ms | **1.9x** |

**Without index:** comparable to ripgrep (~2–2.5s full scan).

### Index cost

| Metric | Value |
|--------|-------|
| Full build | ~60s (one-time) |
| Incremental update | <1s for 10–100 files (75x faster than rebuild) |
| Index load (mmap) | 17ms |
| Index size | ~2.7 GB postings + 162 MB bitmaps |

## How it works

Four techniques combine to eliminate >99% of I/O before the regex engine runs:

1. **Trigram inverted index** — Each 3-byte trigram maps to the lines that contain it. A query is decomposed into the trigrams it must contain, so only lines carrying *all* of them survive as candidates — typically a tiny fraction of the corpus.

2. **Two-tier Roaring bitmaps** — Tier 1 is a compressed doc-id set per trigram: the file-level intersection runs as a Roaring-bitmap AND, skipping most files before any posting list is read. Only the surviving files load Tier 2 (the line-level postings).

3. **Persistent index with mmap** — Binary posting lists memory-mapped at query time. 17ms load regardless of corpus size; the OS pages in only the lists you touch.

4. **Line-level index with byte offsets** — Index stores line positions, not just file IDs. Verification jumps directly to candidate lines instead of scanning entire files.

## Installation

The binary name is `fgr`.

### Precompiled binaries

Download the archive for your platform from the
[latest release](https://github.com/gmilano/fast-grep-rust/releases/latest) and
put `fgr` somewhere on your `PATH`. SHA256 sidecars (`*.sha256`) and a
`SHA256SUMS` file are attached to every release.

Targets published on each release:

| OS      | Architecture | Archive                                                  |
| ------- | ------------ | -------------------------------------------------------- |
| macOS   | aarch64      | `fast-grep-vX.Y.Z-aarch64-apple-darwin.tar.gz`           |
| macOS   | x86_64       | `fast-grep-vX.Y.Z-x86_64-apple-darwin.tar.gz`            |
| Linux   | aarch64 (gnu)| `fast-grep-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz`      |
| Linux   | aarch64 (musl)| `fast-grep-vX.Y.Z-aarch64-unknown-linux-musl.tar.gz`    |
| Linux   | x86_64 (gnu) | `fast-grep-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz`       |
| Linux   | x86_64 (musl)| `fast-grep-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz`      |
| Windows | x86_64       | `fast-grep-vX.Y.Z-x86_64-pc-windows-msvc.zip`            |

### Cargo (any platform with a Rust toolchain)

```bash
cargo install fast-grep
```

Or download a prebuilt binary into Cargo's bin dir without compiling:

```bash
cargo binstall fast-grep
```

### Homebrew (macOS / Linux)

```bash
brew install gmilano/fast-grep/fast-grep
```

The tap lives at [`gmilano/homebrew-fast-grep`](https://github.com/gmilano/homebrew-fast-grep).

### Scoop (Windows)

```powershell
scoop bucket add fast-grep https://github.com/gmilano/scoop-fast-grep
scoop install fast-grep
```

### Debian / Ubuntu (.deb)

A `.deb` package is attached to every release for `amd64` and `arm64`:

```bash
curl -LO https://github.com/gmilano/fast-grep-rust/releases/latest/download/fast-grep_0.3.1-1_amd64.deb
sudo dpkg -i fast-grep_*_amd64.deb
```

### Build from source

```bash
git clone https://github.com/gmilano/fast-grep-rust
cd fast-grep-rust
cargo build --release
# binary at ./target/release/fgr
```

SIMD (AVX2/NEON) auto-enabled via `.cargo/config.toml` (`target-cpu=native`),
so a from-source build is tuned to your machine. Distributed binaries are
built without `target-cpu=native` so they work on any CPU of the target arch.

### Not yet packaged

These channels are not officially packaged yet — they need a community
maintainer in each ecosystem (the same path ripgrep took). PRs welcome.

- `apt` on Debian / Ubuntu (official repos)
- `dnf` on Fedora / `yum` on RHEL
- `pacman` on Arch / AUR
- MacPorts
- Chocolatey / Winget
- FreeBSD `pkg`, OpenBSD `pkg_add`, NetBSD `pkgin`
- Nix / Guix / Flox
- Void Linux / Gentoo

In the meantime, `cargo install fast-grep` works on all of them.

## Usage

Searching is the default — pass PATTERN and PATH as positional args. Other
operations (`index`, `update`, `bench`, `stats`, `daemon`) are subcommands.

```bash
# Build index (one-time, ~60s for Linux kernel)
fgr index /path/to/codebase --output .fgr

# Build with a case-insensitive companion so `-i` searches stay indexed
fgr index -i /path/to/codebase --output .fgr

# Search with index — index is auto-built on first use if missing
fgr "EXPORT_SYMBOL" /path/to/codebase --index .fgr

# Case-insensitive indexed search (uses the CI companion if the index has one)
fgr -i "export_symbol" /path/to/codebase --index .fgr

# Search without index (ripgrep-equivalent full scan)
fgr "EXPORT_SYMBOL" /path/to/codebase

# Incrementally update an existing index after files changed
fgr update /path/to/codebase --index .fgr

# Benchmark against ripgrep
fgr bench "static.*inline" /path/to/codebase

# Index stats
fgr stats --index .fgr
```

### Daemon mode (auto-incremental updates)

Run a background watcher that observes filesystem changes and applies
debounced incremental index updates. Searches automatically flush pending
changes before running, so the index never lags behind your edits.

```bash
# Build index and start the daemon in one step
fgr index /path/to/codebase --output .fgr --daemon

# Or start the daemon against an existing index
fgr daemon start /path/to/codebase --output .fgr

# Status / stop
fgr daemon status /path/to/codebase --output .fgr
fgr daemon stop   /path/to/codebase --output .fgr
```

The daemon debounces FS events by 3 seconds, so a burst of writes triggers a
single update. State is exchanged over a localhost TCP socket recorded in
`<index>/daemon.port`.

### Flags

| Flag | Description |
|------|-------------|
| `--index <path>` | Use persistent index |
| `--files-only` | Print file paths only |
| `--count` | Print match count |
| `--type <ext>` | Filter by extension (`c`, `rs`, `ts`) |
| `--no-ignore` | Don't respect `.gitignore` |

## Why this matters for agents

LLM coding agents (Cursor, Claude Code, Aider) spend significant time grepping large repos. Every search blocks the agent's next reasoning step. fast-grep turns 2.5s waits into <200ms lookups — a 10x reduction in tool-call latency that compounds across an entire coding session.

## Related work

| Project | Approach | Notes |
|---------|----------|-------|
| [ripgrep](https://github.com/BurntSushi/ripgrep) | SIMD scan, no index | Best no-index grep |
| [ugrep](https://github.com/Genivia/ugrep) | Index + scan | Previously fastest indexed grep |
| [zoekt](https://github.com/sourcegraph/zoekt) | Trigram index (Go) | Powers Sourcegraph |
| [Cursor](https://cursor.com/blog/fast-regex-search) | Sparse n-gram (closed) | Inspiration for this project |

## Releasing

Release procedure (version bump, tagging, post-tag updates to Homebrew tap
and Scoop bucket, common failures): [`RELEASING.md`](RELEASING.md).

## Credits

Created and maintained at **[Globant](https://www.globant.com)**.

## License

MIT
