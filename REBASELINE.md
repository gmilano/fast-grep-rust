# Index rebaseline (delta → primary compaction)

## The problem

A persistent index is a frozen **primary** baseline plus a mutable **delta**
overlay. `fgr update` never rewrites the primary: it appends changed files to
the delta and tombstones the stale primary docs (in `deleted.bin`). Searches
read primary + delta together and filter tombstones, so results stay correct.

But the delta only grows. Three costs accumulate as the working tree diverges
from the frozen baseline (all measured on the 79K-file Linux kernel):

1. **Every update re-pays the whole delta.** An incremental update rewrites the
   delta from scratch, re-reading and re-trigramming every *carried* delta file
   (~1 ms/file) — a 2,000-file delta adds ~2 s to every subsequent update,
   forever, until a rebaseline resets it.
2. **Selective queries full-scan the delta.** Delta docs have no Roaring
   bitmap, so they are force-added to every query's candidate set. While the
   candidate count stays below the selective-bitmap fast-path threshold
   (`max(500, 0.7%·docs)`), that fast path scans candidate *files* whole — all
   of the delta, every query (~0.14 ms/file/query: +55 ms at a 400-file
   delta). Past the threshold the query falls back to posting intersection,
   which handles the delta precisely — so the penalty is a hump that peaks
   just below the threshold, exactly the zone a growing delta lives in.
3. **Tombstone garbage.** Modified/deleted files leave dead postings and stale
   bitmap entries in the primary — wasted lookups and disk bloat.

Rebaselining folds the delta and drops the tombstones back into a fresh, dense
primary — cheaply, and without disturbing concurrent searches.

## Why not just rebuild?

A full `fgr index` re-reads and re-trigrams every file (I/O-bound; on Windows
also Defender-bound) and peaks memory building the whole map. Compaction instead
**reuses the postings already on disk** — it only remaps doc-ids and re-encodes
integers, in parallel across trigrams, overlapping the encode with the file
writes. Measured on the 79K-file Linux kernel (2.8 GB of postings, 28 cores):

| Operation | Time |
|---|---|
| Full rebuild (`fgr index`) | ~183 s |
| Compaction (`fgr compact`) | **~3 s** (~60× faster) |

Phase split (via `FGR_TIMING=1`): parallel encode ≈ 1.5 s, file write ≈ 2.1 s
(overlapped with the encode; ~1.4 GB/s — the disk's sequential write speed, i.e.
the hard floor for rewriting a 2.8 GB baseline).

## Two-slot layout + `current` pointer

The index *root* (the `.fgr` dir) holds only coordination files: the `current`
pointer, `config.toml`, the daemon pid/port, and the `lock`. The actual index
content lives in a **slot** subdirectory named by `current`:

```
.fgr/
  current            # -> "slot-a" | "slot-b"
  config.toml        # editable; never rewritten by build/update/compact
  slot-a/  ngrams.* ngrams.ci.* docids.bin meta.json delta.* deleted.bin
  slot-b/
  lock  daemon.pid  daemon.port
```

A writer stages a fresh baseline into the **non-live** slot and then flips
`current` atomically, so a reader mapped on the live slot is never disturbed.
Absent `current` → legacy flat layout (content directly in the root); the loader
falls back to it, so pre-slot indexes keep working without a rebuild.

## The compaction algorithm (`compact_into`)

1. **Dense remap.** Walk old doc-ids in order (all primary ids before all delta
   ids), skip tombstones, assign sequential new ids. Live primary ids land below
   live delta ids.
2. **Per trigram (merge-join over the primary + delta lookups, both
   hash-sorted).** Decode the primary postings, drop tombstones, remap; decode
   the delta postings, remap; concatenate (primary-remapped ids are all below
   delta-remapped ids, so the result stays globally sorted); re-encode and
   rebuild the Roaring bitmap. Trigrams left empty after dropping tombstones are
   omitted. This per-trigram work runs **in parallel** (rayon) in bounded
   chunks — order-preserving, so the lookup tables stay hash-sorted — and is
   decode→encode **streaming** (no intermediate tuple list is materialized;
   the bitmap gets one insert per doc run, not per line). A dedicated writer
   thread drains a small bounded channel, so the hash-order file write overlaps
   the encode: total ≈ max(encode, write) instead of their sum, and the write
   runs at the disk's sequential speed. Parallelizing the write itself would
   not help — trigram offsets depend on all previous sizes (so positional
   parallel writes need either the whole 2.8 GB in RAM or per-chunk temp files
   plus a serial concat), and N threads writing one file don't make the disk
   faster than saturated sequential streaming.
3. The case-insensitive companion (`ngrams.ci.*`) folds in lockstep with the
   same remap.
4. `docids.bin` is rewritten in the dense order; `meta.json` gets
   `main_num_docs = live_total` and empty delta/deleted. mtimes are **carried
   forward for survivors only** (never re-stat'd) — compaction reuses existing
   postings, so the recorded mtime must match the indexed content; a file that
   changed since it was indexed keeps its old mtime and is caught by the next
   stale check.

## The swap (delete, never rename)

`compact()` runs under the index write-lock (readers never take it, so search is
never blocked). It stages into the non-live slot, then flips `current`, then
reclaims the old slot. The reclaim uses **delete, never rename**, per measured
Windows behavior:

- Deleting (`remove_file` / `remove_dir_all`) a file/dir that another process
  currently mmaps **succeeds**; the reader keeps reading its existing mapping.
- **Renaming** a directory with a mapped child **fails** (`ERROR_ACCESS_DENIED`).
- You can even delete a slot and immediately recreate one with the same name
  while an old reader still maps the old files — the old reader keeps the old
  content (independent inode). So two fixed slots suffice with zero starvation.

This depends on POSIX-delete semantics (Windows 10 1709+/11); on older Windows a
delete could go "delete-pending" and same-name recreation could fail.

Crash safety: the new slot is written fully before the atomic `current` flip
(the last step), so a crash mid-compaction leaves the old baseline intact and
orphans the half-written slot, which is reclaimed on the next build/compact.

## Triggers

One primitive, three entry points:

- **`fgr compact`** — explicit; always folds whatever is pending, ignoring the
  config thresholds.
- **`fgr update`** — after writing the delta, if the config says divergence is
  over threshold, folds in-place under the lock it already holds (`--no-compact`
  opts out). Cost paid by the updater, never by a search.
- **Daemon** — after each debounced update, spawns the compaction on a **worker
  thread** so the event loop keeps serving the socket. `run_update` takes the
  lock non-blockingly (`try_acquire_index_lock`): while the worker holds the
  lock for the fold, the loop simply skips the next update round and retries, so
  only one compaction runs at a time with no extra bookkeeping. Moving the
  multi-second compaction off the loop bounds the worst-case daemon stall by the
  (smaller) on-loop update phase, instead of update + compaction combined.

## Configuration

Thresholds live in `<index>/config.toml`, written with commented defaults on
first build and never clobbered afterwards (hand edits stick):

```toml
[compaction]
auto = true              # gates the automatic triggers (not `fgr compact`)
delta_docs_abs = 500     # compact once the live delta exceeds this many docs
delta_docs_ratio = 0.05  # ...or this fraction of the baseline
tombstone_ratio = 0.10   # ...or once tombstones exceed this fraction of it
min_main_docs = 500      # never auto-compact a baseline smaller than this
```

The defaults follow from the measured costs above: at 500 delta docs the
carried-delta tax on every update is capped at ~0.5 s and the selective-query
scan hump at ~70 ms, while the fold that resets both costs ~3 s (background in
the daemon, updater-paid otherwise). They were originally 4× higher — tuned for
a ~31 s serial compaction; the parallel + overlapped rewrite made folding cheap
enough to trigger 4× earlier. `fgr stats` reports the current `Delta docs` /
`Tombstones` and whether `Compaction due`.

## Known limitation

Compaction rewrites the whole baseline (2.8 GB on the Linux kernel), so for its
~3 s it saturates disk I/O regardless of the worker thread — anything else
touching the disk on the box (including a cold-cache search load) is slower
while it runs. The thresholds keep that to once per ~500 changed files; raise
them (or set `auto = false` in favor of scheduled `fgr compact`) if the write
churn matters more than update/query latency on your box.
