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
