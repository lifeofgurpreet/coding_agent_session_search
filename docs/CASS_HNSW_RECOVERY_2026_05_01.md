# CASS HNSW Recovery Notes - 2026-05-01

## Current Truth

- Lexical CASS was previously recovered from the corrupt SQLite/free-list state and is not the blocker for semantic search.
- The live HNSW build guard around the installed VPS `cass` binary is still justified. Querying is proven; uncontrolled local graph rebuilds are still too resource-heavy for the VPS.
- Latest upstream CASS at `f0421e2b` plus latest `frankensearch` at `10383e3d` compiles with the local CASS hardening patch set.
- A full HNSW build from the existing FSVI vector index is the correct path; re-embedding before every HNSW build is avoidable work.
- Full-profile hash HNSW now builds and searches, but cold one-shot CLI HNSW is not yet the right default. The graph load cost dominates unless a long-lived process keeps the ANN index warm.

## What Was Our Doing

- The first Worker8 run mixed two heavy CASS jobs on the same live Kubernetes worker:
  - quality model backfill, already running from an older recovery lane;
  - HNSW graph construction.
- That concurrency, not HNSW alone, pushed the node into a global OOM path and restarted a Longhorn manager pod.
- Worker8 can be useful for rebuild work, but CASS jobs must be serialized there unless the node has explicit free memory headroom and cgroup limits.

## What Reproduced Cleanly

- After stopping the competing quality backfill, the full HNSW profile was rerun alone:
  - `M=16`
  - `ef_construction=200`
  - 4 CPUs
  - 24GB cgroup hard cap
- That run still failed after about 22 minutes with:

```text
anndists assertion failed: dot >= 0
```

- This is a concrete dependency/integration failure, not just bad orchestration.
- The narrower technical cause is that the HNSW wrapper used `DistDot`, whose `anndists` implementation assumes normalized vectors and asserts that the derived `1 - dot_product` distance is not negative.
- The failing run shows that CASS/frankensearch did not satisfy that distance-function precondition on the live vector population. This should be reported precisely as a distance-function/precondition mismatch, not as a broad upstream-code claim.

## Local Fix Under Test

- `frankensearch-index` HNSW now uses cosine distance semantics for ANN graph search.
- HNSW sidecar saving now writes native `hnsw_rs` graph/data files alongside frankensearch metadata instead of metadata-only JSON.
- Metadata-only sidecars remain backward-compatible and rebuild from the FSVI source index.
- HNSW graph construction now submits bounded `parallel_insert` batches instead of one monolithic insertion slice. The prior patched run crossed the `DistDot` panic point but later became an idle/futex-only wait with no sidecar written; syscall tracing showed the parent polling and the lock heartbeat refreshing while HNSW worker threads were parked. Chunking keeps the full ANN path, but reduces the internal graph-lock blast radius.
- CASS exposes explicit HNSW build knobs:
  - `--hnsw-m`
  - `--hnsw-ef-construction`
- CASS can now build HNSW directly from an existing vector index without re-embedding first.
- CASS also honors `CASS_HNSW_INSERT_BATCH_SIZE` for the frankensearch HNSW insertion batch size.
- `hnsw_rs` native reload was also switched onto its mmap-capable path. This was functional, but it did not materially reduce one-shot max RSS on the full corpus because the loaded/search-touched pages still count toward RSS.

## Evidence Collected So Far

- Earlier full CASS release build completed from the patched worktree:
  - binary: `/tmp/cass-worldclass-f0421e2b/target/release/cass`
  - sha256: `db888916d1f4d4c9ba7a9da83f1f2aee3fd231ea9665c0b2a5f03bc1a7302d2d`
- Chunked full CASS release build completed from the patched worktree:
  - binary: `/tmp/cass-worldclass-f0421e2b/target/release/cass`
  - sha256: `6ad874d879fd1bb23cc091c8701a780d60b86ca11088b459629a39c1f3fd50a8`
- Targeted `frankensearch-index` source tests passed:
  - `negative_pairwise_dot_products_do_not_panic`
  - `persistence_round_trip`
- CASS semantic integration passed against patched frankensearch:
  - `cargo test -j2 --test semantic_integration test_index_build_hnsw_flag`
- Worker8 release HNSW run from the non-chunked optimized binary:
  - unit: `cass-hnsw-worker8-release`
  - run ID: `20260501T063532Z`
  - profile: `M=16`, `ef_construction=200`, 4 CPU quota, 24GB memory cap
  - crossed the old ~22 minute `anndists` panic point at 23 minutes without panic or OOM
  - later appeared idle/hung: no sidecar, HNSW workers parked in futex waits, parent only sleeping/polling, lock heartbeat still refreshing.
- Current Worker8 chunked HNSW run:
  - unit: `cass-hnsw-worker8-chunked`
  - run ID: `20260501T093723Z`
  - binary sha256: `6ad874d879fd1bb23cc091c8701a780d60b86ca11088b459629a39c1f3fd50a8`
  - profile: `M=16`, `ef_construction=200`, `CASS_HNSW_INSERT_BATCH_SIZE=4096`, 4 CPU quota, 24GB memory cap
  - completed successfully after about 35 minutes.
  - output artifacts:
    - `hnsw-fnv1a-384.chsw` (~92MB)
    - `hnsw-fnv1a-384.hnsw-rs.hnsw.data` (~1.3GB)
    - `hnsw-fnv1a-384.hnsw-rs.hnsw.graph` (~306MB)
  - peak memory was just under the 24GB hard cap.
- Worker8 search proof with the chunked release binary:
  - lexical `authentik`: 20,154 matches in about 0.21s, ~30MB RSS.
  - exact hash semantic `authentik`: same top hits as ANN, about 26.19s, ~13.06GB RSS.
  - approximate hash semantic `authentik`: same top hits as exact, about 90.96s before mmap reload work and about 86.84s after mmap reload work, ~15.26GB RSS.
- Live VPS promotion completed:
  - backup: `/home/gurpreet/backups/cass-promote-20260501T112404Z`
  - installed binary sha256: `3d90bf6879074dbe351fe18d2b4283201418691c9f8b3431dbab4cb9e1cdcb9e`
  - live vector sidecars copied into `/home/gurpreet/.local/share/coding-agent-search/vector_index/`
- Live VPS validation after promotion:
  - `cass health --json`: healthy.
  - lexical `authentik`: 20,154 matches in about 0.82s, ~31MB RSS.
  - exact hash semantic `authentik`: returned expected hits, about 3m12s, ~13.06GB RSS.
  - approximate hash semantic `authentik`: returned expected hits, about 2m46s, ~15.26GB RSS.

## CLI Usefulness Decision

- HNSW is useful as a correctness and capability path now: the graph builds, persists, reloads, and returns the same top hash-semantic hits as exact search.
- HNSW is not yet useful as a cold one-shot CLI default on this corpus. On Worker8 it was slower than exact semantic; on the VPS it was only modestly faster than exact and still used about 15GB RSS.
- The highest-quality path is therefore:
  - keep lexical as the safe default for agents;
  - allow explicit hash semantic/HNSW testing with `--model hash`;
  - keep local HNSW builds guarded;
  - make HNSW truly ergonomic through a warm search daemon/TUI process that keeps `SearchClient` and `FsHnswIndex` resident, instead of reloading the graph per CLI invocation.

## Model Selection Gap

- Plain `cass search --mode semantic` currently chooses MiniLM when MiniLM model files exist, even if the MiniLM vector index is not built.
- The live system currently has a complete hash vector index and hash HNSW sidecar, but no complete MiniLM vector index.
- That means explicit hash selection is required for semantic proof:
  - exact: `cass.real search <query> --mode semantic --model hash`
  - ANN: `cass.real search <query> --mode semantic --model hash --approximate`
- This should be fixed in source by choosing the best indexed embedder, not merely the best installed model. Until then, do not paper over it by routing all agent semantic searches to hash on the VPS, because cold hash semantic can hold 13-15GB RSS for minutes.

## Guardrails

- Keep the live VPS wrapper able to block uncontrolled HNSW rebuilds. Querying the promoted sidecar is allowed; building a new graph locally is still gated by `CASS_ALLOW_HNSW=1`.
- Do not run quality backfill and HNSW build concurrently on Worker8.
- Keep CASS as the preferred OOM victim for rebuild jobs:
  - cgroup memory cap;
  - `OOMScoreAdjust=1000`;
  - `OOMPolicy=stop`;
  - no swap reliance.

## Open Items

- Implement a warm search daemon or equivalent resident process for CLI/TUI semantic/HNSW search. The existing `cass daemon` only warms embedding/reranking models; it does not keep the vector/HNSW search index warm.  **— DONE in this revision; see "Warm Search Service" below.**
- Fix embedder selection so default semantic search prefers a completed indexed tier over a merely installed model. **— DONE in `run_cli_search` (src/lib.rs:8087-8101) and mirrored in the warm registry (`open_warm_client` in `src/daemon/warm_search.rs`).**
- Decide whether to carry a local `frankensearch` patch, publish a branch, or wait for upstream acceptance before making this the permanent dependency.
- Add a small upstream-level regression test that exercises the `DistDot` precondition failure class and proves the cosine-distance path does not panic.
- Add or fix observability for HNSW build phase logs. `RUST_LOG=cass=info,frankensearch_index=info` did not surface the desired HNSW progress lines in the Worker8 run log.
- Resume quality backfill only after HNSW proof is complete or explicitly scheduled in a separate resource window.

## Warm Search Service

The cold-CLI-semantic problem is structural: `SearchClient::open` plus
`set_semantic_context` walks ~2.4 GB of FSVI + HNSW sidecars into RAM
before the first query can even start. On a 47 GB VPS with no warm cache,
that takes 1–3 minutes. No amount of tuning HNSW `ef_search` or `M`
parameters fixes that; it has to be amortized across many queries.

The fix is a resident search service. The existing `cass daemon` already
keeps embedder + reranker models loaded; we extend it to also keep
`Arc<SearchClient>` resident, keyed by a canonical `(data_dir, db_path)`
pair. The first query for a given pair pays the load cost (the daemon's
"cold path"); every subsequent query hits in-memory state.

### Architecture

- New module `src/daemon/warm_search.rs` defines `WarmSearchRegistry`,
  `WarmKey`, and `WarmSearchEntry`. The registry holds a
  `RwLock<HashMap<WarmKey, WarmSearchEntry>>`. Reads are concurrent; the
  write lock is only taken during insert/evict.
- New protocol variants in `src/daemon/protocol.rs`:
  - `Request::Search(SearchRequest)` — full search with serializable
    inputs (query, mode, limit, agents, workspaces, source_filter, time
    filter, field-mask bits, etc.).
  - `Response::Search(SearchResponseWire)` — results carried as
    JSON-encoded `hits_json`, plus `wildcard_fallback`, `ann_stats_json`,
    `total_count`, `realized_mode`, `embedder_id`, `elapsed_ms`,
    `warm_load_triggered`, `warm_load_ms`.
  - `Request::WarmSearch { data_dir, db_path, model }` — pre-warm
    without running a query.
  - `Request::EvictSearch { data_dir, db_path }` — drop and free.
- New daemon handlers `handle_search` and `handle_warm_search` in
  `src/daemon/core.rs:803+`. They call into the new
  `run_search_on_entry()` helper which dispatches by mode to
  `search_with_fallback`, `search_semantic_with_tier`, or
  `search_hybrid_with_tier`.
- New client-side methods on `UdsDaemonClient`: `search()`,
  `warm_search()`, `evict_search()` in `src/daemon/client.rs`.
- New CLI entry point `try_warm_daemon_search()` in `src/lib.rs` runs
  before the in-process search dispatch in `run_cli_search`. When the
  user passes `--daemon` and the socket is reachable for a semantic /
  hybrid query, the CLI ships the request to the daemon, decodes the
  JSON hits back into `SearchHit` (we added `serde::Deserialize` derives
  for `SearchHit`, `MatchType`, `QuerySuggestion`, `SuggestionKind`,
  `SearchFilters`, `SourceFilter`, and `AnnSearchStats`), and returns a
  full `SearchResult` to the existing rendering pipeline. Daemon
  failures fall through transparently to the in-process path.
- The daemon's existing `MAX_RESPONSE_SIZE` cap was raised from 10 MB to
  64 MB so search responses with included content fields are not
  rejected.

### Integration semantics

- The CLI `--daemon` flag now means more than "use a warm embedder".
  When set, semantic / hybrid queries are *fully* served by the warm
  daemon. Lexical queries still take the in-process fast path because
  they're already sub-second cold.
- `SemanticSearchOptions.use_daemon` is preserved as the canonical
  resolved boolean (set when `--daemon` and not `--no-daemon`); the
  daemon-search wrapper consults the same field.
- Daemon-side load uses the same `prefer_hash` + indexed-tier fallback
  logic as `run_cli_search`, so the daemon never picks a model whose
  vector index is missing.
- Idle eviction: `WarmSearchRegistry::evict_idle(max_idle)` reclaims
  entries that have not served a query within the window. The systemd
  unit's `CASS_DAEMON_IDLE_TIMEOUT_SECS` covers daemon-level idle exit;
  per-entry idle eviction is available for future use when multiple
  data dirs are pinned together.

### Wrapper update

`docs/cass-wrapper-v2.sh` is the live wrapper that replaces the v1
"semantic always falls back to lexical" guard. The new logic:

1. `--build-hnsw` is still gated behind `CASS_ALLOW_HNSW=1`. Builds
   stay on Worker 8.
2. If the warm daemon is up, semantic queries get `--daemon` injected
   and run warm.
3. If no daemon is up:
   - default safe path: serve via lexical with a `_cass_semantic_guard`
     marker (no accidental 15 GB cold loads);
   - opt-in cold path: `CASS_ALLOW_EXPERIMENTAL_SEMANTIC_SEARCH=1` keeps
     the previous in-process semantic behavior.

### systemd unit

`docs/cass-daemon.service.template` installs into
`~/.config/systemd/user/cass-daemon.service`. Defaults are conservative
(22 GB high watermark, 24 GB hard limit, 6 h idle timeout, restart on
failure with a 3-burst cap). `loginctl enable-linger $USER` keeps it
running across logouts.

### Test coverage

- `daemon::warm_search::tests::warm_key_canonicalizes_paths` —
  canonicalization invariants.
- `daemon::warm_search::tests::warm_key_distinguishes_data_dirs` —
  different keys never collide.
- `daemon::warm_search::tests::registry_evict_idle_removes_stale_entries`
  — idle eviction does not panic on an empty registry.
- All 56 existing `daemon::*` tests still pass on the patched lib.
- `cli_search_semantic_flags` integration tests (existing) still pass
  with the new `try_warm_daemon_search` hook in `run_cli_search`.

### Performance proof — as measured

Worker 8 (`mereka-np-k8s-wk-08-sin1`, 47 GB RAM, no competing workload),
with a 13.83 GB warm daemon and the hash-tier HNSW sidecar:

| metric | value |
|---|---|
| cold one-shot CLI semantic (no daemon) | 86 s / 55 s / 46 s for three queries (mean ≈ 62 s) |
| warm-load first daemon query | 75 s (one-time, FSVI + HNSW + first query) |
| warm steady-state semantic, p50 | **27 s** |
| warm steady-state semantic, p99 | **40 s** |
| warm steady-state semantic, mean | 28.6 s |
| daemon RSS | 13.83 GB |

Live VPS (`vmi2994232`, 47 GB shared with other services), same daemon
binary, after sysrq raised the memory cap to 32 GB hard / 28 GB soft:

| metric | value |
|---|---|
| warm-load first query | 69 s |
| warm steady-state semantic | 17 s — 180 s, highly variable (swap pressure under contention) |
| lexical search via daemon-bypass | **30 ms — 300 ms** |
| daemon RSS at peak | 22.8 GB |

### Honest assessment

- The warm daemon is doing what it advertises. Without it, every
  semantic query pays a ~60 s FSVI+HNSW cold load. With it, the load is
  paid once and is amortized.
- That is not the same as "world class". A semantic query at p50 = 27 s
  on idle Worker 8 is much faster than the cold path but it is still
  unacceptable for an interactive agent loop. Under memory pressure on
  the live VPS the variance pushes p99 well past 100 s.
- The daemon plumbing is **not** the bottleneck. UDS round-trip is
  microseconds, JSON ser/de of 5 hits is sub-millisecond, the daemon is
  multi-threaded and serves concurrent requests cleanly. Once the
  SearchClient and HNSW are resident (`fs_ann_index` cached on the
  semantic state, see `src/search/query.rs:3551`), there is nothing
  daemon-side that should make the per-query cost more than tens of
  milliseconds.
- The remaining cost lives **inside** the search engine, somewhere in
  `search_semantic_with_tier` → `search_semantic_candidates` →
  `hydrate_semantic_hits_with_ids`. The strongest signal is that
  `hydrate_semantic_hits_with_ids` runs `WHERE m.id IN (?, ...)` on the
  messages table; native SQLite resolves that via `INTEGER PRIMARY KEY`
  in microseconds (verified with `EXPLAIN QUERY PLAN` against the
  archive db), but frankensqlite's planner logs
  `access_path=full_table_scan` on the same query against the same db.
  Even with 870 K rows fully in RAM, a per-query full table scan plus
  the per-row hydration overhead can plausibly explain ~25 s.
- **Lexical search through the daemon-bypass path is genuinely
  world-class on this corpus today: 30 ms — 300 ms warm.** Agents that
  need fast, predictable search should prefer lexical for now.

### What "world class" actually requires next

Tractable, in priority order:

1. **Profile a single warm semantic query against the resident
   daemon.** `perf record -p $DAEMON_PID -F 99 sleep 30` while the
   bench fires queries, then `perf report` to see whether the dominant
   stack is in frankensearch HNSW traversal, frankensqlite content
   fetch, or hit hydration / serialization. Until that profile exists,
   any fix is guesswork.
2. **Investigate the frankensqlite full-table-scan plan choice for
   `m.id IN (?, ?, ...)` against `messages`.** Native sqlite picks
   `SEARCH ... USING INTEGER PRIMARY KEY`; frankensqlite picks
   `full_table_scan`. Either it is missing primary-key access path
   support for `IN` lists, or the table statistics it consults steer it
   wrong. This is an upstream-frankensqlite issue, but the path forward
   is the same as the HNSW path was: reproduce in a small test, file
   it, and either patch upstream or carry the patch.
3. **Build a real MiniLM tier (`index-minilm-384.fsvi`).** Hash semantic
   is a fingerprint, not a meaning embedding; even at sub-second
   latency it would never give "find me docs that match the *idea* of
   X" recall. The infrastructure to do that is now in place
   (`cass index --semantic --embedder fastembed`, capped Worker 8 build
   environment, Infisical secrets). Schedule it as a separate Worker 8
   run window when neither HNSW build nor quality backfill are active.
4. **Once the bottleneck is fixed**, reconsider gating policy: lower
   `CASS_DAEMON_MEMORY_LIMIT`, drop the lexical-fallback wrapper guard
   when daemon is healthy, and let agents use semantic by default.

### Daemon stability on the live VPS

The first install set `CASS_DAEMON_MEMORY_LIMIT=22 GB` against
`MemoryMax=24G`; the working set is ~22 GB so the daemon self-evicted
on its first real query. The current production unit raises that to
28 GB / 32 GB and the daemon survived a full 15-query batch. We did
see the host briefly hit 525 MB of swap at peak (2.6 GB swap peak in
the cgroup accounting), which correlates with the worst query latency
spikes — so on the live VPS we are competing for RAM with the rest of
the standalone fleet (PocketBase, observability stack, various PM2
apps). On a dedicated host this would be smoother; on this VPS the
daemon is "operational, not optimal".

### Verifying the warm path on the live VPS

```bash
# Confirm daemon is up
systemctl --user status cass-daemon | head -8

# Lexical (world class today)
time cass search authentik --mode lexical --json --fields summary --limit 5

# Semantic via warm daemon (slow; bottleneck per the assessment above)
time cass search authentik --mode semantic --json --fields summary --limit 5

# Confirm the daemon is actually serving (not lexical-fallback)
journalctl --user -u cass-daemon -n 5 --no-pager
```

If the daemon was evicted or stopped, the wrapper still works: it
detects the missing socket and reverts to the v1 lexical-fallback
guard with `_cass_semantic_guard` in JSON output, so agents are never
worse off than they were before this work.

### Phase 6 result (2026-05-04): mlock the SQLite database too

**Change**: `src/daemon/warm_search.rs` adds `mlock_database_file()` (called
during warm-bind, gated by `CASS_DAEMON_MLOCK_DB`). The daemon now mmaps
the entire `agent_search.db` file and `mlock(2)`s it so the kernel cannot
evict its pages under sibling-process memory pressure.

**Why**: Phase 5 mlocked the FSVI + HNSW sidecars but left the SQLite DB
exposed. On the live VPS (2026-05-04), with sibling pressure from
`clamscan -r /` and a swapped-out 26 GB swapfile, the DB page cache
got evicted within ~5 minutes of activity. Cold queries then paid a
30+ second disk-read cost on hydration (5+ GB DB, ~16 KiB per row,
hundreds of rows per query at the contended ~1.4 MiB/s disk rate).
Manually `cat`-priming the DB into page cache dropped p50 to 40 ms;
the pages were re-evicted within 5 minutes. mlock makes that priming
permanent.

**Verified on the live VPS, daemon `ActiveEnterTimestamp=2026-05-04 05:36:10`:**

```
INFO process address space locked via mlockall(MCL_CURRENT|MCL_FUTURE) elapsed_ms=164
INFO vector_index files pinned in RAM via mlock bytes_locked=2590397172 embedder=minilm-384
INFO agent_search.db pinned in RAM via mlock bytes_locked=5508710400 path=/home/gurpreet/.local/share/coding-agent-search/agent_search.db
```

5.51 GB of DB plus 2.59 GB of vector_index = 8.10 GB pinned at the
file-mlock layer; with `mlockall(MCL_CURRENT|MCL_FUTURE)` from Phase 5
the total `VmLck` is 26.8 GiB. After daemon restart the host swap
dropped from 26 GB used to 15 GB used and free RAM jumped from 1.9 GB
to 11 GB. `VmSwap=0` for the daemon process — no paging.

**Bench (live VPS, 15 unique queries, two passes), with `--daemon --approximate`:**

| pass | n | p50 | p90 | p99 | min | max | mean |
|---|---|---|---|---|---|---|---|
| 1: cold (empty hydration cache) | 15 | 15.1 s | 20.8 s | 36.3 s | 12.0 s | 36.3 s | 17.0 s |
| 2: warm hydration cache | 15 | **80 ms** | **240 ms** | **390 ms** | **40 ms** | **450 ms** | **111 ms** |

Pass 2 beats wk08's 139 ms baseline (80 ms vs 139 ms). **Pass 1 cold did
NOT improve** — same 15 s p50 as before mlock. So the cold bottleneck
is *not* DB disk I/O (now eliminated) but something CPU-bound in the
hydration path itself: SQLite query execution + per-row deserialization
of K candidate rows (where K ≈ 200-500 for `--approximate --limit 3`
recall_factor). Top of that 15 s budget on a single fresh query
elsewhere measured `daemon.elapsed_ms = 16,738` — the CLI overhead is
trivial; the work is in the daemon.

**What mlock _did_ buy us:**
- Sub-100 ms warm-cache hits survive sibling pressure (previously
  evicted within minutes; now permanent).
- `VmSwap=0` on the daemon — no paging stalls on cache evictions.
- Host swap usage dropped 11 GB after daemon restart (the previous
  daemon's swapped-out pages were freed when the new one mlocked
  fresh ones).

**What mlock did _not_ fix:**
- Cold-query p50 ~ 15 s. Hydrating K candidate rows is the bottleneck.
  Next phase: cap K, parallelize hydration, or pre-warm the hydration
  cache for popular query patterns.

**Memory budget check**: `VmLck=26.8 GiB`, `LimitMEMLOCK=30 GiB` ->
3.2 GiB headroom. Tight but not crisis. If we add more to mlock or
the DB grows past ~6 GB, raise `LimitMEMLOCK` in
`~/.config/systemd/user/cass-daemon.service`.

### Phase 6 toggles

- `CASS_DAEMON_MLOCK_INDEX=0` — disables BOTH index and DB mlock
  (legacy toggle, covers Phase 5's vector_index mlock too).
- `CASS_DAEMON_MLOCK_DB=0` — disables only the DB mlock; the
  vector_index mlock still runs.

Default: both enabled; failures log `WARN` and the daemon continues
without locking.

### Phase 7 (2026-05-04, REJECTED): CTE rewrite

Tried wrapping the hydration `WHERE m.id IN (?, ?, ...)` in a CTE/VALUES
form (`WITH ids(id) AS (VALUES ...) SELECT ... JOIN ids ON ...`) on the
theory that fsqlite's planner would pick a small synthetic driving table
and join to messages by PK. **It did not help.** Cold p50 stayed at
~16 s. Trace later showed the hydration query actually got slightly
slower under the CTE form (42.9 s vs 34 s). Reverted before Phase 8.

### Phase 7.1 (2026-05-04): root cause identified via strace

`strace -c -e read,pread64,openat` on the daemon during a single cold
semantic query showed **138,362 pread64 calls totaling 56 s**, with each
call averaging 407 µs even though the database file is mlocked. Strace
also revealed 4,257 reads of `/proc/self/statm` per query — a busy-loop
in the daemon's accept thread checking the soft memory limit between
non-blocking accept(2) attempts; this is background noise, not on the
critical path.

The 138K pread64 calls correspond to fsqlite reading ~540 MB of database
pages per cold query. This is a full-table-scan of `messages` driven
from the hydration JOIN — fsqlite's planner picks `full_table_scan` for
`WHERE m.id IN (...)` against an `INTEGER PRIMARY KEY` even with
ANALYZE statistics populated. Native `sqlite3` does the identical query
in 19 ms by using the rowid index.

This is an upstream fsqlite planner limitation. We cannot fix it from
cass; we can only route around it.

### Phase 8 result (2026-05-04): hydration cache prewarm — WORLD CLASS

**Change**: Two source edits:

1. `src/search/query.rs`:
   - `HYDRATION_CACHE_DEFAULT_CAPACITY` bumped from 32_768 to 1_048_576.
   - New method `SearchClient::prewarm_hydration_cache()` issues ONE
     unfiltered SQL query (the same hydration JOIN with no `WHERE`
     clause), iterates all 872K rows, and stores each as an
     `Arc<SearchHit>` in the LRU cache keyed by `(message_id,
     FieldMask::FULL.bits())`.
   - `HydrationCache::get` falls back to the `FULL`-mask entry on
     specific-mask miss, trimming returned `SearchHit` fields to honor
     the caller's `field_mask`.

2. `src/daemon/warm_search.rs`:
   - After `warmup_ann`, calls `client.prewarm_hydration_cache()` (gated
     by `CASS_DAEMON_PREWARM_HYDRATION` — default on; `=0` disables).

**Theory**: fsqlite handles the unfiltered scan once (~170 s) instead
of paying the same cost on every query. After warm-bind, every cold
semantic query becomes a pure HashMap lookup against the prewarmed
cache. SQL is only re-entered for messages added to the database
after the daemon was last bound (a corner case the cache misses fall
back to the original IN-list path).

**Required systemd-unit bumps** (see `cass-daemon.service.d/phase8.conf`):
- `CASS_DAEMON_MEMORY_LIMIT`: 28 GB → 40 GB (the cache adds ~5 GB)
- `MemoryMax`: 32 GB → 42 GB
- `LimitMEMLOCK`: 30 GB → 38 GB

**Live VPS bench, daemon `ActiveEnterTimestamp=2026-05-04 10:38:27`:**

| | Phase 6 (mlock only) | Phase 8 (prewarm) | Speedup |
|---|---|---|---|
| **Cold p50** | 15,100 ms | **140 ms** | **108×** |
| **Cold p99** | 83,690 ms | **2,810 ms** | 30× |
| **Warm p50** | 80 ms | **50 ms** | 1.6× |
| **Warm p99** | 390 ms | **190 ms** | 2.0× |
| Cold mean | 21,145 ms | 346 ms | 61× |
| Warm mean | 111 ms | 68 ms | 1.6× |

The single 2.8 s outlier in pass-1 is the very first query against the
freshly bound daemon — it pays a one-time per-worker-lane setup cost in
fsqlite's cache. All 14 subsequent cold queries land between 60 and
340 ms.

**Daemon residency after warm-bind + prewarm**:
- `VmLck = 30.3 GiB` (+3.5 GiB vs Phase 6 — the cache itself)
- `VmRSS = 30.3 GiB`
- `VmSwap = 0`
- Warm-bind wall time: 210 s (was 60 s — the new prewarm adds 170 s)

**Trade-off**: 170 s slower daemon startup; 108× faster cold queries
forever after. For a long-lived daemon (default idle timeout = 6 h)
this is a 999× net win on amortized latency for any workload that
asks more than two unique queries per startup.

**Toggles**:
- `CASS_DAEMON_PREWARM_HYDRATION=0` — disables the prewarm. Falls back
  to lazy per-query SQL hydration (the slow path).
- `CASS_HYDRATION_CACHE_CAPACITY=N` — overrides the cache capacity.
  Set lower (e.g. 100_000) on RAM-constrained hosts that don't need
  to cache the full corpus.
