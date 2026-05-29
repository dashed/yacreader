# Stump Sync Implementation — Audit Report

**Date:** 2026-05-29
**Scope:** Phases 2 & 3 of the `stump_sync` crate + C++ integration + tests + docs, on branch `feat/stump-sync-phase2` ([dashed/yacreader#1](https://github.com/dashed/yacreader/pull/1)).
**Method:** 4 parallel read-only auditors (Rust core correctness; FFI/concurrency; DB/SQL/security; tests/build/docs), each using fuzzy search + sequential reasoning. All Critical/High findings independently re-verified against the source by the lead.

---

## TL;DR

The Rust crate is well-structured and its 56 unit/integration tests pass — **but the test suite runs with default features (no `ffi`), so the entire FFI bridge, `rust_sync_init`, and the C++ wiring are never compiled or exercised.** That green checkmark hid two Critical defects:

1. **The feature cannot initialize in any real deployment** (C++ never supplies the per-library Stump id/path the Rust init requires, and there is no config source for it).
2. **Bidirectional sync can irreversibly wipe a "read" flag in the live YACReader database** when page and completion disagree on direction.

Plus a build that won't compile with `ENABLE_STUMP_SYNC=ON` (missing cxx header include path), and a concurrency hazard writing to the live `.ydb` with no busy timeout.

**Severity tally:** 2 Critical · 6 High · 8 Medium · ~12 Low/Info, plus 11 test-coverage gaps.

> Note: this is design/implementation feedback on an in-progress feature branch that is **off by default** (`ENABLE_STUMP_SYNC=OFF`) and not yet merged. With the feature disabled, the upstream build is provably unaffected (verified). None of this is shipping to users yet.

---

## Critical

### C1 — Integration is non-functional: `rust_sync_init` always fails when ≥1 library exists
**Confidence: High (verified).** `main.cpp:285-292` + `lib.rs:77-81`. (Reported as TB-1 / FC-2.)

The cxx `SyncConfig` has four parallel per-library vectors: `ydb_paths`, `library_roots`, `stump_library_ids`, `stump_library_paths`. `main.cpp`'s loop pushes only the first two. `rust_sync_init` validates that **all four are equal length** and returns `Err("library config vectors must have equal length")` otherwise. With N≥1 libraries: `ydb=N, roots=N, ids=0, paths=0` → mismatch → init fails every run (logged as "Stump sync init failed"). Only the zero-library case "succeeds," and then syncs nothing.

Deeper problem: `QSettings[StumpSync]` (`main.cpp:263-267`) reads only `serverUrl/apiKey/userId/mappingDbPath/syncIntervalSecs`. There is **no configuration source at all** for *which Stump library id + path-prefix* maps to each YACReader library — yet `match_comics()` needs `stump_library_path` to translate absolute↔relative paths. The integration is incomplete, not just mis-wired.

**Fix:** Add per-library config (e.g. a `[StumpSync/libraries]` array of `{yacPath, stumpLibraryId, stumpLibraryPath}`), populate all four vectors in lock-step, skip libraries with no mapping. Add an `ffi`-feature test asserting init succeeds with realistic vectors and fails cleanly on mismatch.

### C2 — Bidirectional conflict resolution wipes the YACReader `read` flag (irreversible local data loss)
**Confidence: High (verified).** `sync_engine.rs:614-672` (decision) + `:582,:594-597` (write). (Reported as RC-1.)

`compute_bidirectional_delta` chooses direction **by page alone**; completion is only carried when it agrees with the page-ahead side. When the page-behind side holds the completion, it is neither propagated nor preserved — violating the documented `OR(completion)` ("once read, stays read") rule. Because `write_yac_progress` does a **blind UPDATE** of `read`, the local flag is overwritten downward.

**Verified scenario:** User taps "Mark as read" in YACReader (`read=1`, `currentPage=5`). Stump shows page 10, not completed, `num_pages=20`.
- `stump_page_ahead = true` → direction `PullToYac`, `pull_page=10`, and since `stump_has_completion=false`, `pull_read=false`.
- `apply_pull_delta` → `write_yac_progress(page=10, read=false)`. `is_read = false || (20>0 && 10>=20)` = **false** → `UPDATE … SET read=0`.
- **The user's "read" flag is silently destroyed in the real `.ydb`.** Symmetric loss exists on the push side (`yp>sp, yr=false, sc=true` pushes page only, never marks complete).

**Fix:** Decouple completion from page direction — compute `final_page = max(...)` and `final_complete = yac.read || stump.complete` independently; propagate completion to whichever side lacks it. `write_yac_progress` must **never lower** an existing `read=1` (read-modify-write, or OR the flag).

---

## High

### H1 — `ENABLE_STUMP_SYNC=ON` build likely fails to compile (missing cxx header include path)
**Confidence: High (verified, (a)) / Medium ((b)).** `YACReaderLibraryServer/CMakeLists.txt:62-72`, `main.cpp:18`. (Reported as TB-2.)

(a) `main.cpp` does `#include "stump-sync/src/lib.rs.h"` (the cxx-generated bridge header), but neither CMakeLists adds a `target_include_directories` for the generated cxxbridge dir. `corrosion_import_crate` links the Rust staticlib but does **not** expose cxx headers → header-not-found. (b) The cxx C++ glue compiled by `build.rs` into `stump_sync_cxx` (a Cargo `OUT_DIR` lib) may not be linked by `corrosion_import_crate`, risking undefined references to `stump_sync::*`.

**Fix:** Use `corrosion_add_cxxbridge(...)` (generates the bridge, a C++ target, include dirs, and links the glue) instead of hand-rolling. Verify with an actual `ON` build in CI.

### H2 — `library_id` namespace mismatch: per-comic push silently fails
**Confidence: High (verified).** `main.cpp:298-303` → `sync_engine.rs:37-51`; `mapping_db.rs` `ensure_library_mapping`. (Reported as FC-3 / RC-6.)

`comicUpdated` carries YACReader's URL library id (parsed from `/v2/library/<id>/…`). `push_single` resolves the library by calling `ensure_library_mapping(...)` inside a `find()` predicate and comparing `.ok() == Some(library_id)` — but `ensure_library_mapping` returns the **mapping DB's own autoincrement rowid**, unrelated to YACReader's id. They coincide only by luck (single library, both `1`). Otherwise every per-comic push returns `Config("library mapping {id} not found")` and silently fails. Two further smells: the predicate **mutates** (inserts a row as a side effect of a lookup), and **swallows errors** (`.ok()` turns a DB/lock failure into "no match").

**Fix:** Key library resolution on a stable identity both sides know (e.g. `yac_library_path`), not a synthetic rowid. No mutation/error-swallowing in a `find()` predicate; add `get_library_config_by_id`.

### H3 — No `busy_timeout` on `.ydb` connections → `SQLITE_BUSY` under concurrent access with the live server
**Confidence: High.** `sync_engine.rs:441,536,576-579`. (Reported as DS-1, confirms hypothesis H2.)

The module reads and (Phase 3) writes the YACReader `.ydb` while the YACReaderLibraryServer process is also using it. YACReader uses the **default rollback journal (not WAL)**, so writers take a whole-file exclusive lock. No connection sets `busy_timeout` (default 0 → fail immediately). Result: periodic `sync_all` colliding with the server → `SQLITE_BUSY`. A write collision loses that comic's pulled progress for the cycle; a **read** collision propagates via `?` and **aborts the whole library's sync**; and while Rust holds a lock, the server's *own* writes can fail. No corruption (SQLite rejects safely; page values are absolute so retries converge), but real reliability/lost-update risk.

**Fix:** `conn.busy_timeout(Duration::from_millis(5000))` on **every** `.ydb` connection immediately after open. Do **not** switch the `.ydb` to WAL. Optionally a small bounded retry around the write.

### H4 — `write_yac_progress` blind-overwrites and derives completion from the wrong page count
**Confidence: High (verified).** `sync_engine.rs:568-612`. (Reported as RC-3.)

(a) It never reads the existing row, so `read`, `hasBeenOpened`, `lastTimeOpened` are all clobbered from the delta — the mechanism behind C2. (b) `is_read` auto-complete compares Stump's `page` against **YACReader's** `num_pages` (`:582`); if the two systems count pages differently (cover page, etc.), a pulled `sp >= yac.num_pages` **falsely sets read=1** even when Stump isn't complete; and `num_pages=0/unknown` disables auto-complete so a genuinely complete pull with `pull_read=false` loses completion.

**Fix:** Read-modify-write; OR the read flag; trust Stump's `is_completed`, not page arithmetic, for completion.

### H5 — `update_progress` hardcodes `isCompleted: false`, un-completing comics on Stump
**Confidence: Medium (depends on Stump semantics).** `stump_client.rs:158-164`. (Reported as RC-2.)

Every page push sends `"isCompleted": false`. Pushing a page to a comic Stump considers complete will reset its completion (if Stump honors the flag). Any page-push not immediately followed by `mark_complete` can clear Stump-side completion.

**Fix:** Omit `isCompleted` on a pure page update, or compute it from the desired final state.

### H6 — Documented LWW conflict policy is not implemented
**Confidence: High (verified).** `sync_engine.rs:614-672,:346-354`; doc §5.3 / Phase 3 step 2. (Reported as RC-5 / TB-3.)

The doc claims "latest timestamp wins (LWW)" and a timestamp-threshold re-read detection. The code does pure `max(page)` + `OR(complete)` with **no timestamp comparison anywhere**; `ReadProgress.updated_at` is parsed but never read; `update_sync_state` is always called with `*_modified = None` so the `*_last_modified` columns are **always NULL** (LWW data isn't even recorded). This is partly a missing feature and partly **doc drift** (see L-docs).

**Fix:** Either implement LWW (parse `updated_at`/`completed_at` → epoch, compare, write the true source timestamp) or downgrade the doc to the real policy.

---

## Medium

| ID | Title | Location | Note |
|----|-------|----------|------|
| M1 | `api_key` lives in `Debug`-derived `Config` **and** `SyncConfig` — latent secret leak (not currently logged) | `config.rs:1`, `lib.rs:22` | DS-2. Hand-write redacting `Debug`. |
| M2 | `sync_state` records **pre-sync** values and `get_sync_state` is **never called** in production — dead data, no 3-way merge | `sync_engine.rs:345-355`; `mapping_db.rs` | RC-4. Feed it into conflict resolution, or drop it. |
| M3 | `build_mappings` re-reads `.ydb` and **re-fetches Stump GraphQL** that `push_library` immediately fetches again — 2× I/O + 2× network per push | `sync_engine.rs:130-143,:180-189` | RC-7. Fetch once, pass in. |
| M4 | Remapping impossible: `INSERT OR IGNORE` + UNIQUE on `(lib, yac_id, stump_id)` → a changed `stump_media_id` makes a **second** row; `get_stump_id` has no `ORDER BY/LIMIT` → nondeterministic | `schema.sql:19`; `mapping_db.rs` | RC-8. UNIQUE `(lib, yac_id)` + `ON CONFLICT DO UPDATE`. |
| M5 | `match_comics` uses a raw **string** prefix, not a path-boundary prefix: `/srv/comics` false-matches `/srv/comics-extra/…`; also case- and unicode-NFC-sensitive; `normalize_path` doesn't collapse `//` | `sync_engine.rs:482-511,:474` | RC-9. Require a `/` boundary; normalize. |
| M6 | `readProgresses.first()` with **no user filter**; configured `user_id` is `#[allow(dead_code)]` and unused → on multi-user Stump, reads the wrong user's progress | `types.rs:48-62`; `stump_client.rs:10-11,103-128` | RC-10 / TB-7. Filter query by `user_id`. |
| M7 | No `catch_unwind` on FFI bodies — a panic in `rust_sync_init`'s deep call path aborts (crashes) the **whole server**. Not UB (defined abort); reachability low today but future-facing | `lib.rs:48-56`; `runtime.rs:42-67` | FC-1. Wrap each `extern "Rust"` body in `catch_unwind`. |
| M8 | Doc §9.6 CMake snippet is stale/self-contradictory (missing `FEATURES ffi`, wrong file, omits `CoreFoundation`, wrong include path) | doc §9.6 | TB-4. |

---

## Low / Info

- **RC-11** — `SyncReport` miscounts: page counted as 0 if `update_progress` succeeds but `mark_complete` fails; auto-complete pulls not counted in `completions_pulled`. (`sync_engine.rs:159-174,:404-411`)
- **RC-12** — `hasBeenOpened = page > 0` but doc/`WHERE currentPage > 1` imply 1-based pages → pulling page 1 wrongly marks opened. Confirm YACReader page base. (`sync_engine.rs:581`)
- **FC-4** — Shutdown runs `rust_sync_shutdown()` before `httpServer->stop()`; reverse is safer (stop signal sources first). (`main.cpp:325-336`)
- **FC-5** — No idempotency guard on `rust_sync_init`; a second call drops a live runtime under the lock. (`lib.rs:107-113`)
- **FC-6** — Two blocking calls on the Qt main thread: `shutdown_timeout(10s)` worst-case exit hang, and synchronous sqlite open/migrate at startup. (`runtime.rs:87,49`)
- **FC-7** — If `event_loop` panics, `status.is_running` stays `true` while pushes degrade to "not initialized" (misleading status). (`runtime.rs:220-222,77-81`)
- **FC-8** — `QObject::connect` uses the context-less functor form (safe here — lambdas capture nothing — but non-idiomatic). (`main.cpp:298,305`)
- **DS-3** — New short-lived `.ydb` connection per `write_yac_progress`. (`sync_engine.rs:576`)
- **DS-4** — Mutex poisoning permanently disables `MappingDb` for the process (handled gracefully, no recovery).
- **DS-5** — Mapping DB WAL assumes a local filesystem.
- **TB-5** — Doc §9.5/§9.3 snippet drift: `page>1` vs code `>0`; missing `num_pages>0` guard; `chrono::Utc::now()` vs `SystemTime`; `sync_interval_secs: u32` vs `u64`; stray code fence at L1316; pre-impl hardcoded main.cpp snippet.
- **TB-6** — `mappingDbPath` has no default; empty → `MappingDb::open("")` opens a temp DB, losing mappings each run, silently. (`main.cpp:266`)

---

## Test coverage gaps (the meta-finding)

`cargo test` runs with **default features → the `ffi` bridge, `rust_sync_init`, runtime, and C++ wiring are never compiled**, so all 56 passing tests give **zero** coverage of where C1, H1, H2, M7 live. Priority gaps:

1. **FFI/init path** — vector-length validation, `Config` construction, init success/failure. (Would have caught C1.)
2. **A test mirroring `main.cpp`'s config→`LibraryConfig` population.** (Would have caught C1.)
3. **`runtime.rs`/`event_loop`** — periodic timer firing (use `tokio` paused time — dev-dep present but unused), command dispatch, shutdown, init-twice.
4. **`SQLITE_BUSY` / concurrent `.ydb` writes** — currently untested and the doc calls it "safe."
5. **Conflict-resolution data-loss cases** — the exact C2 scenario (page-behind side holds completion) is untested; `test_conflict_resolution_max_page` is mislabeled (it's just stump-ahead).
6. Multi-library E2E (all tests use 1 library); per-library error isolation.
7. Engine-level Stump fetch failure; malformed/empty Stump responses; zero matches.
8. GraphQL **pagination** — `get_library_media` assumes a single response; large libraries may truncate.
9. `push_single` error paths (no-mapping-after-build, library-not-found, media-not-found).
10. Pull-side field semantics — `hasBeenOpened`/`lastTimeOpened` never asserted after a pull.
11. `test_retry_on_http_500` uses real ~3s backoff sleeps (injectable backoff would speed the suite).

**Positives (verified):** bidirectional pull tests genuinely read the `.ydb` back and assert page/read (not false-positives); client tests use strict `.expect(N)` + body matchers; negative assertions present; `ENABLE_STUMP_SYNC=OFF` compiles no stump code (upstream unaffected); real CMake correctly includes `FEATURES ffi`; no SQL injection anywhere (all `params!`); no GraphQL injection (typed variables); TLS via rustls with validation intact; API key sent as a header, not in URL; retries only network/5xx and check the GraphQL `errors` array; idempotent mutations make retry safe; `Mutex<Connection>` never held across `.await`; FFI string conversions are UTF-8/lifetime-correct; the `tokio::select!` loop is cancellation-safe with the first immediate tick consumed.

---

## Recommended fix order

1. **C1 + H1 + H2** — make the integration actually initialize, build, and resolve libraries (these three block any real use).
2. **C2 + H4** — stop the data loss: read-modify-write + decouple completion from page direction.
3. **H3** — `busy_timeout` on `.ydb` connections.
4. **H5, H6, M-series** — correctness/robustness and doc reconciliation.
5. Backfill the **FFI/init + concurrency + data-loss** tests so the suite actually covers the integration.
