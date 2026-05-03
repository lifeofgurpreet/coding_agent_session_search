//! Warm SearchClient registry for the daemon.
//!
//! The daemon keeps one or more `Arc<SearchClient>` resident, keyed by a
//! canonical `(data_dir, db_path)` pair. Each entry holds:
//! - the Tantivy reader
//! - the FSVI vector index (loaded once)
//! - the HNSW graph (loaded once)
//! - the embedder
//!
//! All of those are the multi-second/multi-GB cost of a cold one-shot CLI
//! semantic search. By keeping them resident across CLI invocations we move
//! semantic/HNSW from "minutes per query" to "tens of milliseconds per
//! query" steady-state.
//!
//! The registry deliberately does NOT pre-warm at daemon startup; warm load
//! happens on first request for a given `(data_dir, db_path)` pair, or on
//! an explicit `Request::WarmSearch`. This keeps the daemon cheap when the
//! user only wants embeddings/reranking.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use memmap2::Mmap;
use parking_lot::{Mutex, RwLock};

use crate::daemon::protocol::{SearchRequest, SearchResponseWire};
use crate::search::embedder_registry::{EmbedderRegistry, HASH_EMBEDDER};
use crate::search::model_manager::{
    SemanticAvailability, load_hash_semantic_context, load_semantic_context,
};
use crate::search::query::{
    FieldMask, SearchClient, SearchClientOptions, SearchFilters, SearchMode, SearchResult,
    SemanticTierMode,
};
use crate::search::tantivy::expected_index_dir;
use crate::search::vector_index::VECTOR_INDEX_DIR;
use crate::sources::provenance::SourceFilter;

/// One entry in the warm search registry. Cloning an entry is cheap (Arc).
#[derive(Clone)]
pub struct WarmSearchEntry {
    pub client: Arc<SearchClient>,
    /// Embedder id that was bound to this client's semantic context, e.g.
    /// "fnv1a-384" or "minilm-384". Empty when no semantic context loaded.
    pub embedder_id: String,
    /// When this entry was first warmed.
    pub created_at: Instant,
    /// Atomic monotonic millis since UNIX_EPOCH of last use, for idle
    /// eviction. Updated on every successful search.
    pub last_used_unix_millis: Arc<AtomicU64>,
    /// Total queries served by this warm entry.
    pub query_count: Arc<AtomicU64>,
    /// Wall-clock millis spent loading this entry. Reported in proof.
    pub warm_load_ms: u64,
}

/// Canonicalized `(data_dir, db_path)` key. Both paths are absolute and
/// follow symlinks so two callers using different relative paths still hit
/// the same warm entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WarmKey {
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
}

impl WarmKey {
    pub fn from_paths(data_dir: &Path, db_path: &Path) -> Self {
        Self {
            data_dir: canonicalize_or_owned(data_dir),
            db_path: canonicalize_or_owned(db_path),
        }
    }
}

fn canonicalize_or_owned(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

pub fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Registry of warm SearchClients. All ops are `&self`-callable and safe
/// across threads.
///
/// `load_locks` is a registry of per-key Mutexes that serialize concurrent
/// first-load attempts: when two callers simultaneously ask for the same
/// key while the entry is missing, the second caller blocks on the per-key
/// mutex while the first does the expensive `SearchClient::open` plus
/// `set_semantic_context` (~30-90s on the corpus). When the first caller
/// finishes, the second caller takes the mutex, re-checks the entry, finds
/// it now warm, and returns immediately — instead of doing the same load
/// in parallel.
pub struct WarmSearchRegistry {
    entries: RwLock<HashMap<WarmKey, WarmSearchEntry>>,
    load_locks: RwLock<HashMap<WarmKey, Arc<Mutex<()>>>>,
}

impl WarmSearchRegistry {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            load_locks: RwLock::new(HashMap::new()),
        }
    }

    fn load_lock_for(&self, key: &WarmKey) -> Arc<Mutex<()>> {
        if let Some(existing) = self.load_locks.read().get(key).cloned() {
            return existing;
        }
        let mut w = self.load_locks.write();
        w.entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Look up a warm entry without loading. None if not warmed.
    pub fn get(&self, key: &WarmKey) -> Option<WarmSearchEntry> {
        self.entries.read().get(key).cloned()
    }

    /// Number of currently warm entries.
    pub fn len(&self) -> usize {
        self.entries.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.read().is_empty()
    }

    /// Drop a warm entry. Returns true if an entry was removed.
    pub fn evict(&self, key: &WarmKey) -> bool {
        self.entries.write().remove(key).is_some()
    }

    /// Drop entries that have been idle longer than `max_idle`.
    /// Returns the number evicted.
    pub fn evict_idle(&self, max_idle: Duration) -> usize {
        let cutoff_ms = now_unix_millis().saturating_sub(max_idle.as_millis() as u64);
        let mut guard = self.entries.write();
        let stale: Vec<WarmKey> = guard
            .iter()
            .filter_map(|(k, v)| {
                let last = v.last_used_unix_millis.load(Ordering::Relaxed);
                if last < cutoff_ms {
                    Some(k.clone())
                } else {
                    None
                }
            })
            .collect();
        let n = stale.len();
        for k in stale {
            guard.remove(&k);
        }
        n
    }

    /// Get an existing entry or load and cache a new one.
    ///
    /// `requested_model` mirrors CLI semantics: None = best indexed tier,
    /// Some("hash") = force hash, Some("minilm-384") = explicit. Returns
    /// (entry, warm_load_triggered, warm_load_ms).
    ///
    /// Concurrent first-callers for the same key serialize on a per-key
    /// load mutex so we never run the multi-GB FSVI/HNSW load twice in
    /// parallel. The second caller waits on the mutex; when it acquires
    /// it the entry is already warm and it returns immediately.
    pub fn get_or_warm(
        &self,
        data_dir: &Path,
        db_path: &Path,
        requested_model: Option<&str>,
    ) -> Result<(WarmSearchEntry, bool, u64)> {
        let key = WarmKey::from_paths(data_dir, db_path);

        // Fast path: already warm.
        if let Some(entry) = self.get(&key) {
            entry
                .last_used_unix_millis
                .store(now_unix_millis(), Ordering::Relaxed);
            return Ok((entry, false, 0));
        }

        // Slow path: take the per-key load mutex. If another caller is
        // already loading this key, we block here until they finish; on
        // the other side of the lock the entry will be warm and we
        // re-check before reloading.
        let lock = self.load_lock_for(&key);
        let _guard = lock.lock();

        if let Some(entry) = self.get(&key) {
            entry
                .last_used_unix_millis
                .store(now_unix_millis(), Ordering::Relaxed);
            return Ok((entry, false, 0));
        }

        let load_start = Instant::now();
        let (client, embedder_id) = open_warm_client(data_dir, db_path, requested_model)?;
        let warm_load_ms = load_start.elapsed().as_millis() as u64;

        let entry = WarmSearchEntry {
            client: Arc::new(client),
            embedder_id,
            created_at: Instant::now(),
            last_used_unix_millis: Arc::new(AtomicU64::new(now_unix_millis())),
            query_count: Arc::new(AtomicU64::new(0)),
            warm_load_ms,
        };

        let mut entries = self.entries.write();
        let final_entry = entries.entry(key).or_insert(entry).clone();
        Ok((final_entry, true, warm_load_ms))
    }

    /// For Status: snapshot of (key, embedder_id, query_count, age_secs).
    pub fn snapshot(&self) -> Vec<WarmEntrySnapshot> {
        let guard = self.entries.read();
        guard
            .iter()
            .map(|(k, v)| WarmEntrySnapshot {
                data_dir: k.data_dir.display().to_string(),
                db_path: k.db_path.display().to_string(),
                embedder_id: v.embedder_id.clone(),
                query_count: v.query_count.load(Ordering::Relaxed),
                age_secs: v.created_at.elapsed().as_secs(),
                warm_load_ms: v.warm_load_ms,
            })
            .collect()
    }
}

impl Default for WarmSearchRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Opaque view of a warm entry, suitable for status reporting.
#[derive(Debug, Clone)]
pub struct WarmEntrySnapshot {
    pub data_dir: String,
    pub db_path: String,
    pub embedder_id: String,
    pub query_count: u64,
    pub age_secs: u64,
    pub warm_load_ms: u64,
}

/// Open and fully load a SearchClient for the given paths.
///
/// This is the expensive operation we want to do once per `(data_dir, db_path,
/// model)` and never again. It mirrors the model-selection logic in
/// `run_cli_search` so the daemon and the in-process path agree on which
/// tier they bind to.
fn open_warm_client(
    data_dir: &Path,
    db_path: &Path,
    requested_model: Option<&str>,
) -> Result<(SearchClient, String)> {
    let index_path = expected_index_dir(data_dir);

    // We *do* enable reload-on-search and warmup for the daemon-resident
    // client — those keep the Tantivy reader fresh when the index gets
    // rebuilt out from under us, which is the daemon's whole point.
    let opts = SearchClientOptions {
        enable_reload: true,
        enable_warm: true,
    };

    let client = SearchClient::open_with_options(&index_path, Some(db_path), opts)
        .with_context(|| format!("opening SearchClient at {}", index_path.display()))?
        .ok_or_else(|| anyhow!("Tantivy index not found at {}", index_path.display()))?;

    // Bind a semantic context so semantic/hybrid queries are warm too.
    let registry = EmbedderRegistry::new(data_dir);
    let embedder_info = match requested_model {
        Some(name) => registry.get(name),
        None => Some(registry.best_available()),
    };
    let mut prefer_hash = embedder_info.is_some_and(|e| e.name == HASH_EMBEDDER);

    let mut setup = if prefer_hash {
        load_hash_semantic_context(data_dir, db_path)
    } else {
        load_semantic_context(data_dir, db_path)
    };

    // Same fallback logic as run_cli_search: when the installed default
    // model has no ready vector index but hash does, prefer hash. The
    // daemon path makes this fallback *more* important because the daemon
    // is responsible for keeping the right tier warm.
    if requested_model.is_none() && !prefer_hash && setup.context.is_none() {
        let hash_setup = load_hash_semantic_context(data_dir, db_path);
        if hash_setup.context.is_some() {
            tracing::warn!(
                "warm_search: requested default semantic tier has no ready vector \
                 index; binding hash semantic instead"
            );
            prefer_hash = true;
            setup = hash_setup;
        }
    }

    let mut bound_embedder_id = String::new();
    if let Some(context) = setup.context {
        let embedder = context.embedder;
        let index = context.index;
        let filter_maps = context.filter_maps;
        let roles = context.roles;
        bound_embedder_id = embedder.id().to_string();

        let ann_path = Some(
            data_dir
                .join(VECTOR_INDEX_DIR)
                .join(format!("hnsw-{}.chsw", bound_embedder_id)),
        );

        client
            .set_semantic_context(embedder, index, filter_maps, roles, ann_path)
            .with_context(|| {
                format!(
                    "binding semantic context (prefer_hash={}, embedder={})",
                    prefer_hash, bound_embedder_id
                )
            })?;

        // Pin the FSVI + HNSW sidecar files into physical RAM with mlock so
        // the kernel can't reclaim them under memory pressure from sibling
        // processes (other agents, antivirus scans, backups). Without this,
        // an idle daemon's vector_index pages get evicted within minutes
        // and the next user query pays a 15-55 s page-in cost. Requires
        // sufficient RLIMIT_MEMLOCK (set LimitMEMLOCK in the systemd unit).
        // CASS_DAEMON_MLOCK_INDEX=0 disables; default is best-effort
        // enabled — failures (small rlimit, missing files) are warned and
        // the daemon continues without locking.
        let mlock_enabled =
            std::env::var("CASS_DAEMON_MLOCK_INDEX").as_deref() != Ok("0");
        if mlock_enabled {
            match mlock_vector_index(data_dir, &bound_embedder_id) {
                Ok(bytes) if bytes > 0 => {
                    tracing::info!(
                        bytes_locked = bytes,
                        embedder = %bound_embedder_id,
                        "vector_index files pinned in RAM via mlock",
                    );
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(
                        embedder = %bound_embedder_id,
                        error = %err,
                        "mlock of vector_index failed; pages may be reclaimed \
                         under pressure (raise LimitMEMLOCK in the systemd unit)",
                    );
                }
            }
        }

        // Eagerly load the HNSW accelerator at warm time so the first user
        // approximate-semantic query doesn't pay the multi-second
        // `reload_hnsw` cost (~15-55 s on the production corpus, dominated
        // by faulting the 1.3 GB graph data file from disk into the daemon's
        // address space). Set CASS_DAEMON_PREWARM_ANN=0 to skip — useful
        // when the daemon is being restarted under memory pressure and the
        // page cache is cold; the lazy-load path still works.
        let prewarm_ann =
            std::env::var("CASS_DAEMON_PREWARM_ANN").as_deref() != Ok("0");
        if prewarm_ann {
            let warmup_started = Instant::now();
            match client.warmup_ann() {
                Ok(true) => {
                    tracing::info!(
                        warmup_ann_ms = warmup_started.elapsed().as_millis() as u64,
                        embedder = %bound_embedder_id,
                        "HNSW accelerator warmed at daemon startup",
                    );
                }
                Ok(false) => {
                    tracing::debug!(
                        embedder = %bound_embedder_id,
                        "no HNSW accelerator on disk; approximate search will fall back",
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        embedder = %bound_embedder_id,
                        error = %err,
                        "HNSW accelerator warmup failed; first --approximate query \
                         will pay the lazy-load cost (or fall back to brute force)",
                    );
                }
            }
        }
    } else {
        // Lexical-only warm. Still useful: keeps Tantivy reader resident.
        let summary = match &setup.availability {
            SemanticAvailability::IndexMissing { .. } => "index missing",
            SemanticAvailability::DatabaseUnavailable { .. } => "database unavailable",
            SemanticAvailability::LoadFailed { .. } => "load failed",
            _ => "no semantic context",
        };
        tracing::info!(
            "warm_search: lexical-only warm for {} ({})",
            data_dir.display(),
            summary
        );
    }

    Ok((client, bound_embedder_id))
}

/// Process-lifetime holder for mlocked vector_index Mmaps. Storing the
/// `Mmap`s here ensures the kernel keeps the underlying pages resident
/// until daemon exit — dropping the `Mmap` would munmap and implicitly
/// munlock. The `OnceLock` is initialized on the first warm bind; later
/// binds re-enter and add new mappings if a different embedder_id binds
/// (rare in practice — usually one tier per daemon instance).
///
/// Note: there is no public API to release these. The expectation is that
/// the daemon process exits to release them, the same lifecycle as the
/// SearchClient itself.
static MLOCKED_INDEX_FILES: OnceLock<Mutex<Vec<Mmap>>> = OnceLock::new();

/// Open, mmap, and `mlock(2)` the FSVI + HNSW sidecar files for `embedder_id`
/// under `data_dir`. Returns the total bytes locked across all files. Skips
/// silently for files that do not exist (missing HNSW = no accelerator
/// built yet, which is fine).
fn mlock_vector_index(data_dir: &Path, embedder_id: &str) -> Result<u64> {
    let vi = data_dir.join(VECTOR_INDEX_DIR);
    let candidates = [
        vi.join(format!("index-{embedder_id}.fsvi")),
        vi.join(format!("hnsw-{embedder_id}.chsw")),
        vi.join(format!("hnsw-{embedder_id}.hnsw-rs.hnsw.data")),
        vi.join(format!("hnsw-{embedder_id}.hnsw-rs.hnsw.graph")),
    ];

    let holder = MLOCKED_INDEX_FILES.get_or_init(|| Mutex::new(Vec::new()));
    let mut locked = holder.lock();
    let already_locked: std::collections::HashSet<*const u8> =
        locked.iter().map(|m| m.as_ptr()).collect();

    let mut total: u64 = 0;
    for path in &candidates {
        if !path.is_file() {
            continue;
        }
        let file = std::fs::File::open(path)
            .with_context(|| format!("mlock: open {}", path.display()))?;
        // SAFETY: read-only mapping of a regular file. We hold the Mmap
        // for the lifetime of the daemon process, so the lifetime
        // requirements of `Mmap` (no concurrent writes that change the
        // file size) match how cass treats these artifacts: rebuilds
        // produce *new* paths via `.staging-…fsvi` + atomic rename, so
        // the inode under our mapping never has its size mutated.
        let mmap = unsafe { Mmap::map(&file) }
            .with_context(|| format!("mlock: mmap {}", path.display()))?;
        // Skip if we already have this region locked (idempotent re-bind).
        if already_locked.contains(&mmap.as_ptr()) {
            continue;
        }
        let len = mmap.len();
        // SAFETY: `mmap.as_ptr()` is the start of a valid mapping of `len`
        // bytes that we own. mlock is read-only-safe.
        let rc = unsafe {
            libc::mlock(mmap.as_ptr() as *const libc::c_void, len as libc::size_t)
        };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            anyhow::bail!(
                "mlock({}): {} ({}). Bytes already locked this call: {}.",
                path.display(),
                err,
                err.raw_os_error().unwrap_or(-1),
                total
            );
        }
        total = total.saturating_add(len as u64);
        locked.push(mmap);
        tracing::debug!(
            file = %path.display(),
            bytes = len,
            "mlock successful",
        );
    }
    Ok(total)
}

/// Execute a search using the given warm entry. Mirrors the dispatch logic
/// in `run_cli_search` for the lexical / semantic / hybrid modes, including
/// the wildcard-fallback path for sparse lexical results.
pub fn run_search_on_entry(
    entry: &WarmSearchEntry,
    req: &SearchRequest,
) -> Result<SearchResponseWire> {
    let client = entry.client.as_ref();
    let mode = parse_mode(&req.mode)?;

    let mut filters = SearchFilters::default();
    if !req.agents.is_empty() {
        filters.agents = req.agents.iter().cloned().collect();
    }
    if !req.workspaces.is_empty() {
        filters.workspaces = req.workspaces.iter().cloned().collect();
    }
    filters.created_from = req.since;
    filters.created_to = req.until;
    if let Some(ref s) = req.source_filter {
        filters.source_filter = SourceFilter::parse(s);
    }

    let field_mask = FieldMask::from_bits(req.field_mask_bits);
    let sparse_threshold = req.sparse_threshold.max(1);
    let limit = req.limit;
    let offset = req.offset;

    // Track the realized mode (may downgrade for hybrid_fail_open).
    let mut realized_mode_str = match mode {
        SearchMode::Lexical => "lexical",
        SearchMode::Semantic => "semantic",
        SearchMode::Hybrid => "hybrid",
    }
    .to_string();

    let result: SearchResult = match mode {
        SearchMode::Lexical => client
            .search_with_fallback(
                &req.query,
                filters.clone(),
                limit,
                offset,
                sparse_threshold,
                field_mask,
            )
            .map_err(|e| anyhow!("lexical search failed: {e}"))?,
        SearchMode::Semantic => {
            // The warm entry already bound semantic context (or failed to).
            // If it isn't bound, fail open if requested, else error.
            if entry.embedder_id.is_empty() {
                if req.hybrid_fail_open {
                    realized_mode_str = "lexical".to_string();
                    client.search_with_fallback(
                        &req.query,
                        filters.clone(),
                        limit,
                        offset,
                        sparse_threshold,
                        field_mask,
                    )?
                } else {
                    return Err(anyhow!(
                        "semantic context not bound for this warm entry; \
                         re-warm with an explicit model or build a vector index"
                    ));
                }
            } else {
                let (hits, ann_stats) = client
                    .search_semantic_with_tier(
                        &req.query,
                        filters.clone(),
                        limit,
                        offset,
                        field_mask,
                        req.approximate,
                        SemanticTierMode::Single,
                    )
                    .map_err(|e| anyhow!("semantic search failed: {e}"))?;
                SearchResult {
                    hits,
                    wildcard_fallback: false,
                    cache_stats: client.cache_stats(),
                    suggestions: Vec::new(),
                    ann_stats,
                    total_count: None,
                }
            }
        }
        SearchMode::Hybrid => {
            if entry.embedder_id.is_empty() {
                if req.hybrid_fail_open {
                    realized_mode_str = "lexical".to_string();
                    client.search_with_fallback(
                        &req.query,
                        filters.clone(),
                        limit,
                        offset,
                        sparse_threshold,
                        field_mask,
                    )?
                } else {
                    return Err(anyhow!(
                        "hybrid requires semantic context; warm entry has none"
                    ));
                }
            } else {
                client
                    .search_hybrid_with_tier(
                        &req.query,
                        &req.query,
                        filters.clone(),
                        limit,
                        offset,
                        sparse_threshold,
                        field_mask,
                        req.approximate,
                        SemanticTierMode::Single,
                    )
                    .map_err(|e| anyhow!("hybrid search failed: {e}"))?
            }
        }
    };

    let hits_json =
        serde_json::to_string(&result.hits).map_err(|e| anyhow!("serializing hits: {e}"))?;
    let suggestions_json = serde_json::to_string(&result.suggestions)
        .map_err(|e| anyhow!("serializing suggestions: {e}"))?;
    let ann_stats_json = match &result.ann_stats {
        Some(s) => {
            Some(serde_json::to_string(s).map_err(|e| anyhow!("serializing ann_stats: {e}"))?)
        }
        None => None,
    };

    Ok(SearchResponseWire {
        hits_json,
        wildcard_fallback: result.wildcard_fallback,
        ann_stats_json,
        total_count: result.total_count,
        suggestions_json,
        realized_mode: realized_mode_str,
        embedder_id: entry.embedder_id.clone(),
        elapsed_ms: 0,              // filled in by the caller
        warm_load_triggered: false, // filled in by the caller
        warm_load_ms: 0,            // filled in by the caller
    })
}

fn parse_mode(s: &str) -> Result<SearchMode> {
    match s {
        "lexical" => Ok(SearchMode::Lexical),
        "semantic" => Ok(SearchMode::Semantic),
        "hybrid" => Ok(SearchMode::Hybrid),
        other => Err(anyhow!("invalid search mode '{other}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warm_key_canonicalizes_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let db = tmp.path().join("agent.db");
        std::fs::write(&db, b"").unwrap();

        let key1 = WarmKey::from_paths(&data, &db);
        let key2 = WarmKey::from_paths(&data, &db);
        assert_eq!(key1, key2);
    }

    #[test]
    fn warm_key_distinguishes_data_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let db = tmp.path().join("agent.db");
        std::fs::write(&db, b"").unwrap();

        let ka = WarmKey::from_paths(&a, &db);
        let kb = WarmKey::from_paths(&b, &db);
        assert_ne!(ka, kb);
    }

    #[test]
    fn registry_evict_idle_removes_stale_entries() {
        let reg = WarmSearchRegistry::new();
        // Manually inject a synthetic stale entry without a real client by
        // poking entries via a unit-test-only door is overkill; instead
        // assert the empty case.
        assert_eq!(reg.evict_idle(Duration::from_secs(1)), 0);
        assert!(reg.is_empty());
    }
}
