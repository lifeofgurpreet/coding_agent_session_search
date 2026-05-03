# Carried frankensearch patches for CASS

CASS depends on frankensearch HEAD `10383e3d` (pinned in `Cargo.toml`) plus
the patches in this directory. Without these patches, CASS HNSW builds either
fail with `anndists assertion failed: dot >= 0` or hang in futex during
parallel insertion at scale.

## Patch list

| File | Upstream parent | Purpose |
|---|---|---|
| `0001-cass-hnsw-distcosine-native-sidecars-chunked-insert.patch` | `10383e3d` | DistDot → DistCosine, native hnsw_rs sidecars, chunked parallel_insert |

## What's in 0001

Three changes to `crates/frankensearch-index/src/hnsw.rs` (+ two-line wire-ups
in `lib.rs` and `two_tier.rs`):

1. **DistDot → DistCosine.** Stock frankensearch uses `DistDot`, which assumes
   pre-normalized vectors. CASS's FSVI population path didn't satisfy that
   precondition, causing `anndists assertion failed: dot >= 0` ~22 minutes
   into a real build. `DistCosine` normalizes internally; safer for any
   FSVI source that may not pre-normalize.

2. **Native `hnsw_rs` sidecar dump.** Stock frankensearch persisted only
   metadata + row-ordered vectors and rebuilt the graph on load. This costs
   minutes for an 800K-vector graph. The patch adds `hnsw_rs::dump_to_disk`
   for `*.hnsw-rs.hnsw.graph` + `*.hnsw-rs.hnsw.data` so loading the index
   is read-and-mmap rather than rebuild-from-scratch. Backwards-compatible:
   metadata-only sidecars are still loaded by rebuilding the graph.

3. **Chunked `parallel_insert`.** Submitting 800K vectors to a single
   `parallel_insert` call hangs in futex when worker threads park behind
   internal graph locks. The patch caps each insertion epoch at
   `HnswConfig.insert_batch_size` (default 4096), making each batch
   independent. Also exposed via `CASS_HNSW_INSERT_BATCH_SIZE` env in cass.

## Forward-port plan

When bumping the pinned frankensearch revision:

1. Update `Cargo.toml` to the new rev.
2. `cd ../frankensearch && git fetch && git rebase <new-rev>` — patches may
   conflict if upstream touched `hnsw.rs`.
3. Resolve conflicts. The DistDot→DistCosine change is the most likely
   to conflict (upstream may have added their own normalization handling).
4. Re-export the patch:
   ```
   git format-patch <new-rev>..HEAD --stdout > \
     vendor/frankensearch-patches/0001-cass-hnsw-*.patch
   ```
5. Run a small HNSW build on wk08 to confirm none of the three lessons
   regressed.

## Upstream PR (TODO, tracked in cass bead `coding_agent_session_search-uh4ml`)

Reasons each change should land upstream:

- **DistCosine** — frankensearch users without strict pre-normalization
  guarantees will hit the same assertion. Either normalize inputs at the
  HNSW layer or pick the cosine distance variant.
- **Native sidecars** — minutes-faster reload is a generic win.
- **Chunked parallel_insert** — at scale (>200K vectors) the futex hang is
  reproducible. Both a doc note and a chunked path help.

If upstream prefers a smaller surface, splitting into 3 PRs is fine. The
DistCosine and chunked-insert changes are independent of the sidecar dump.

## Why we use `[patch]` instead of forking

The `[patch."https://github.com/Dicklesworthstone/frankensearch"]` block in
CASS's `Cargo.toml` points to a local checkout at `../frankensearch-main/`
(symlink/clone of `~/src/frankensearch`). This keeps CASS's pinned `rev` in
the dependency declaration honest while letting agents iterate on the
patch quickly. Once upstreamed, the `[patch]` block can be removed.
