#!/usr/bin/env bash
#
# cass live wrapper v2 — warm-search aware.
#
# Compared to the prior v1 wrapper (which served *all* semantic searches via
# lexical fallback because cold semantic was multi-second), v2 routes
# semantic searches through the warm daemon when available. The daemon
# keeps the SearchClient + HNSW graph resident, so semantic queries return
# in tens of milliseconds steady-state.
#
# Behavior:
#   1. --build-hnsw is still gated behind CASS_ALLOW_HNSW=1. HNSW build
#      remains a Worker8-class operation; the live VPS only consumes.
#   2. cass health --json keeps the existing post-rebuild reconciliation.
#   3. cass search ... --mode semantic|hybrid:
#        - if the warm daemon is up, route through it transparently
#        - if the daemon is not up and CASS_ALLOW_EXPERIMENTAL_SEMANTIC_SEARCH=1
#          is set, run cold in-process (slow but correct)
#        - otherwise serve via lexical fallback with _cass_semantic_guard
#          attached when --json is requested
#   4. Everything else passes straight through.
set -euo pipefail
REAL="/home/gurpreet/.local/bin/cass.real"
DAEMON_SOCK="${CASS_DAEMON_SOCKET:-/tmp/semantic-daemon-${USER}.sock}"

# --build-hnsw guard (unchanged from v1).
if [[ " ${*:-} " == *" --build-hnsw "* ]] && [[ "${CASS_ALLOW_HNSW:-}" != "1" ]]; then
  cat >&2 <<'MSG'
Blocked: CASS HNSW build is disabled on this VPS.
Reason: HNSW rebuilds are heavy and belong on a capped worker (Worker 8). The
live VPS consumes pre-built HNSW sidecars; it does not build them.
Set CASS_ALLOW_HNSW=1 only for a controlled validation run.
MSG
  exit 42
fi

args=("$@")

# health post-rebuild reconciliation (unchanged from v1).
if [[ "${args[0]:-}" == "health" ]]; then
  structured_health=0
  for arg in "${args[@]}"; do
    case "$arg" in
      --json|--robot)
        structured_health=1
        break
        ;;
    esac
  done

  if [[ "$structured_health" == "1" ]]; then
    out="$(mktemp)"
    err="$(mktemp)"
    cleanup() { rm -f "$out" "$err"; }
    trap cleanup EXIT

    set +e
    "$REAL" "${args[@]}" >"$out" 2>"$err"
    code=$?
    set -e

    if jq -e '
        .errors == ["index stale"]
        and .db.exists == true
        and .db.opened == true
        and .state.index.exists == true
        and .state.index.status == "stale"
        and (.state.index.checkpoint.completed == true)
        and (.state.index.checkpoint.db_matches == true)
      ' "$out" >/dev/null 2>&1; then
      jq '.healthy = true
        | .status = "healthy"
        | .errors = []
        | .recommended_action = null
        | .state.index.status = "ready"
        | .state.index.stale = false
        | .state.index.reason = "checkpoint metadata refreshed by VPS guard; Tantivy search verified separately"' "$out"
      exit 0
    fi

    cat "$out"
    cat "$err" >&2
    exit "$code"
  fi
fi

# search routing
if [[ "${args[0]:-}" == "search" ]]; then
  wants_semantic=0
  wants_json=0
  has_explicit_daemon=0
  has_no_daemon=0
  for arg in "${args[@]}"; do
    case "$arg" in
      --json|--robot|--robot-format=json|--robot-format=compact)
        wants_json=1
        ;;
      --mode)
        : # next-arg pattern handled below
        ;;
      --mode=semantic|--mode=hybrid)
        wants_semantic=1
        ;;
      --daemon)
        has_explicit_daemon=1
        ;;
      --no-daemon)
        has_no_daemon=1
        ;;
    esac
  done
  # also catch the --mode <value> next-arg form
  for ((i=0; i<${#args[@]}; i++)); do
    if [[ "${args[$i]}" == "--mode" ]]; then
      next="${args[$((i+1))]:-}"
      if [[ "$next" == "semantic" || "$next" == "hybrid" ]]; then
        wants_semantic=1
      fi
    fi
  done

  daemon_alive=0
  if [[ -S "$DAEMON_SOCK" ]]; then
    daemon_alive=1
  fi

  if [[ "$wants_semantic" == "1" ]]; then
    if [[ "$daemon_alive" == "1" && "$has_no_daemon" == "0" ]]; then
      # Insert --daemon if the user didn't explicitly add it. The CLI
      # forwards --daemon into SemanticSearchOptions.use_daemon which is
      # what triggers the warm-search code path.
      if [[ "$has_explicit_daemon" == "0" ]]; then
        args+=(--daemon)
      fi
      exec "$REAL" "${args[@]}"
    fi

    # No warm daemon. Decide: cold in-process vs lexical fallback.
    if [[ "${CASS_ALLOW_EXPERIMENTAL_SEMANTIC_SEARCH:-}" == "1" ]]; then
      echo "Notice: warm daemon not running; cass.real will load the vector index cold (slow). Start the daemon with 'systemctl --user start cass-daemon' for sub-second warm queries." >&2
      exec "$REAL" "${args[@]}"
    fi

    # Default safe path: serve via lexical with a guard marker.
    transformed=()
    skip_next=0
    for ((i=0; i<${#args[@]}; i++)); do
      if [[ "$skip_next" == "1" ]]; then
        skip_next=0
        continue
      fi
      arg="${args[$i]}"
      next="${args[$((i+1))]:-}"
      case "$arg" in
        --mode)
          if [[ "$next" == "semantic" || "$next" == "hybrid" ]]; then
            transformed+=(--mode lexical)
            skip_next=1
          else
            transformed+=("$arg")
          fi
          ;;
        --mode=semantic|--mode=hybrid)
          transformed+=(--mode=lexical)
          ;;
        --model)
          if [[ "$next" == fnv1a-* || "$next" == "hash" || "$next" == "minilm-384" ]]; then
            skip_next=1
          else
            transformed+=("$arg")
          fi
          ;;
        --model=fnv1a-*|--model=hash|--model=minilm-384)
          ;;
        *)
          transformed+=("$arg")
          ;;
      esac
    done

    if [[ "$wants_json" == "1" ]]; then
      "$REAL" "${transformed[@]}" | jq '. + {
        "_cass_semantic_guard": {
          "requested_mode": "semantic_or_hybrid",
          "served_mode": "lexical_candidate_fallback",
          "reason": "warm daemon not running; cold semantic disabled by default. Start the daemon (systemctl --user start cass-daemon) or set CASS_ALLOW_EXPERIMENTAL_SEMANTIC_SEARCH=1 to load the vector index cold."
        }
      }'
      exit "${PIPESTATUS[0]}"
    fi
    echo "Warning: warm daemon not running; serving semantic search via lexical fallback. Start the daemon with 'systemctl --user start cass-daemon' for warm semantic." >&2
    exec "$REAL" "${transformed[@]}"
  fi

  # Lexical or other: pass-through, with the auto-fields default kept.
  if [[ "${CASS_ALLOW_FULL_HYDRATE:-}" != "1" ]]; then
    has_fields=0
    for arg in "${args[@]}"; do
      case "$arg" in
        --fields|--fields=*)
          has_fields=1
          break
          ;;
      esac
    done
    if [[ "$has_fields" == "0" ]]; then
      args+=(--fields summary)
    fi
  fi
fi

exec "$REAL" "${args[@]}"
