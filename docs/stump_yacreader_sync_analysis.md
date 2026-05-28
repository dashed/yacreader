# Stump ↔ YACReaderLibraryServer Sync Analysis

## 1. Executive Summary

This document analyzes the feasibility and architecture for synchronizing reading progress between **Stump** (a self-hosted comic/book server) and **YACReaderLibraryServer** (the server component of YACReader). The primary use case is reading comics on an iPhone via YACReader iOS, which syncs progress to YACReaderLibraryServer, then propagating that progress to Stump — and eventually in both directions.

Both servers scan the same comic library on a shared filesystem. Stump serves as the source of truth for library organization and metadata, while YACReaderLibraryServer provides the backend for YACReader iOS. The core challenge is that **the two systems use incompatible content-hashing algorithms** (YACReader: SHA1 of first 512 KB + filesize; Stump: SHA256 of four 10 KB samples), meaning content-based identity matching is not directly possible. The recommended approach uses **relative-path matching** as the primary identity strategy, since both servers index the same directory tree.

The recommended architecture is a **YACReaderLibraryServer fork** with a Rust sync module integrated via cxx FFI and corrosion CMake module. The Rust module handles GraphQL communication with Stump, ID mapping, and sync logic, while C++ handles Qt signal integration and server lifecycle. A Python sidecar alternative is documented in §6 but the fork approach is preferred. Implementation proceeds in four phases: shared filesystem (no code), one-way sync (YACReader → Stump), two-way sync, and polish/upstream contributions.

### Implementation Status

| Phase | Status | Branch | PR |
|-------|--------|--------|-----|
| Phase 1: Shared Filesystem | Design only | — | — |
| **Phase 2: One-Way Sync** | **Implemented** | `feat/stump-sync-phase2` | [dashed/yacreader#1](https://github.com/dashed/yacreader/pull/1) |
| Phase 3: Two-Way Sync | Not started | — | — |
| Phase 4: Polish + Upstream | Not started | — | — |

**Phase 2 metrics:** 18 files, 4,891 lines added. Rust crate: 10 source files, 2,205 lines. Tests: 38 total (29 unit + 4 integration + 5 E2E). All passing.

---

## 2. System Architecture Overview

```
┌─────────────┐
│  iPhone      │
│  YACReader   │
│  iOS App     │
└──────┬───────┘
       │ HTTP (POST /v2/sync, GET pages)
       │ via Tailscale
       ▼
┌──────────────────────┐         ┌─────────────────────┐
│ YACReaderLibrary-    │         │                     │
│ Server               │         │  Stump              │
│                      │         │                     │
│ ┌──────────────────┐ │         │ ┌─────────────────┐ │
│ │ library.ydb      │ │         │ │ stump.db        │ │
│ │ (SQLite)         │ │         │ │ (SQLite/WAL)    │ │
│ └──────────────────┘ │         │ └─────────────────┘ │
│                      │         │                     │
│ HTTP API :8080       │         │ GraphQL API :10801  │
│ (v2 endpoints)       │         │ REST API            │
└──────────┬───────────┘         └──────────┬──────────┘
           │                                │
           │  ┌──────────────────────────┐  │
           │  │   Sync Sidecar Service   │  │
           │  │   (Python)               │  │
           │  │                          │  │
           └──│  Read: SQLite .ydb       │──┘
              │  Write: HTTP /v2/sync    │
              │                          │
              │  Read: GraphQL queries   │
              │  Write: GraphQL mutations│
              │                          │
              │  ┌────────────────────┐  │
              │  │ mapping.db         │  │
              │  │ (SQLite)           │  │
              │  └────────────────────┘  │
              └──────────────────────────┘
                          │
              ┌───────────┴───────────┐
              │  /srv/comics          │
              │  (shared filesystem)  │
              │  mounted by both      │
              │  servers              │
              └───────────────────────┘
```

All three services (YACReaderLibraryServer, Stump, Sync Sidecar) run on the same host, connected to the user's iPhone via Tailscale. The shared `/srv/comics` volume is scanned independently by both Stump and YACReaderLibraryServer.

---

## 3. YACReaderLibraryServer Technical Profile

### 3.1 Database

SQLite database at `<library_path>/.yacreaderlibrary/library.ydb`, schema version 9.16.0. Each library has its own independent database file. Library identity is a UUID stored in `<library_path>/.yacreaderlibrary/id`.

**Key tables:**

| Table | Purpose | Key Columns |
|-------|---------|-------------|
| `comic_info` | Metadata + progress | `id`, `hash` (UNIQUE NOT NULL), `currentPage`, `hasBeenOpened`, `read`, `lastTimeOpened`, `rating`, `numPages`, `bookmark1/2/3` |
| `comic` | File ↔ metadata link | `id`, `comicInfoId` (FK→comic_info), `parentId` (FK→folder), `fileName`, `path` (relative) |
| `folder` | Directory hierarchy | `id`, `parentId` (self-ref), `name`, `path` |

### 3.2 Progress Model

- **Single-user**: No user concept. Progress is global per comic.
- **currentPage** (INT, default 1): The page the reader is on.
- **hasBeenOpened** (BOOL): Ever opened.
- **read** (BOOL): Fully read.
- **lastTimeOpened** (epoch seconds): Last access timestamp.
- **rating** (REAL): User rating (no Stump equivalent).
- **bookmark1/2/3** (INT): Bookmarked page numbers (no Stump equivalent).

### 3.3 Comic Identity

```
hash = SHA1(first 512 KB of file content) + str(file_size_in_bytes)
```

Content-based — survives file renames and moves. Stored as `TEXT UNIQUE NOT NULL` in `comic_info`.

### 3.4 HTTP API (v2)

| Method | Endpoint | Purpose |
|--------|----------|---------|
| `GET` | `/v2/libraries` | List libraries `[{name, id, uuid}]` |
| `POST` | `/v2/sync` | Batch progress sync (tab-separated body) |
| `POST` | `/v2/library/{id}/comic/{comicId}/update` | Single comic progress update |
| `GET` | `/v2/library/{id}/comic/{comicId}/fullinfo` | Full comic metadata as JSON |
| `GET` | `/v2/library/{id}/folder/{folderId}/content` | Folder contents |
| `GET` | `/v2/library/{id}/comic/{comicId}/page/{pageNum}/remote` | Page image |

**Batch sync format** (`POST /v2/sync`):
```
{libraryId}\t{comicId}\t{hash}\t{currentPage}\t{rating}\t{lastTimeOpened}[\t{read}]
```

**Built-in conflict resolution** (sync endpoint):
- `currentPage`: `max(client, server)`
- `read`/`hasBeenOpened`: `OR` (once read, always read)
- `lastTimeOpened`: latest timestamp
- `rating`: client wins if > 0

**Limitations**: No API for creating libraries, folders, labels, or reading lists. The API is designed for iOS client sync, not general-purpose access. No authentication mechanism.

---

## 4. Stump Technical Profile

### 4.1 Database

SQLite in WAL mode, managed via SeaORM. Single database for all libraries.

**Key tables:**

| Table | Purpose | Key Columns |
|-------|---------|-------------|
| `media` | Comic files | `id` (UUID), `name`, `size`, `pages`, `path` (absolute), `hash` (optional SHA256), `series_id` FK, `deleted_at` |
| `reading_sessions` | Active progress | `id`, `page`, `percentage_completed`, `updated_at`, `media_id` FK, `user_id` FK, UNIQUE(media_id, user_id) |
| `finished_reading_sessions` | Completion history | `id`, `completed_at`, `media_id` FK, `user_id` FK (multiple per user/media) |
| `series` | Series grouping | `id` (UUID), `name`, `path`, `library_id` FK |
| `libraries` | Library roots | `id` (UUID), `name`, `path` (unique) |
| `library_configs` | Per-library settings | `generate_file_hashes` (bool) |
| `users` | User accounts | `id` (UUID), `username`, `is_server_owner`, `permissions` |

### 4.2 Progress Model

- **Multi-user**: All progress is scoped to a `user_id`.
- **Active reading**: One `reading_sessions` row per (media, user). Tracks `page`, `percentage_completed`, `updated_at`, `elapsed_seconds`.
- **Completion**: When `page >= total_pages`, the active session is **deleted** and a `finished_reading_sessions` row is created. Multiple completion records are allowed (re-reads).
- **No rating or bookmarks**.

### 4.3 Media Identity

- **UUID** primary key, assigned at scan time.
- **Path-based** matching during library scans — moved files are treated as new media.
- **Optional SHA256 hash**: 4 samples of 10 KB at evenly spaced offsets. Controlled by `library_configs.generate_file_hashes`. **Not used for deduplication.**

### 4.4 API Surface

**GraphQL** (`/api/graphql`):

| Operation | Purpose |
|-----------|---------|
| `updateMediaProgress(id, input)` | Update reading progress. Input is `@oneOf`: `paged: {page, elapsedSeconds?}` or `epub: {...}` |
| `markMediaAsComplete(id, isComplete, page?)` | Explicitly mark complete/incomplete |
| `deleteMediaProgress(id)` | Delete active reading session |
| Query `media.readProgress` | Get `ActiveReadingSession` |
| Query `media.readHistory` | Get `[FinishedReadingSession!]!` |

**REST** (`/api/v2/`):

| Method | Endpoint | Purpose |
|--------|----------|---------|
| `POST` | `/api/v2/auth/login` | Login, returns session cookie (JWT with `?generate_token=true`) |
| `GET` | `/api/v2/media/{id}/file` | Download file |
| `GET` | `/api/v2/media/{id}/page/{page}` | Page image |

**OPDS v2.0** (`/opds/v2.0/`):

| Method | Endpoint | Purpose |
|--------|----------|---------|
| `GET` | `/opds/v2.0/books/{id}/progression` | Get progress |
| `PUT` | `/opds/v2.0/books/{id}/progression` | Update progress (Readium Locator, 409 on conflict) |

**Authentication options**: Session cookies (primary), JWT tokens (optional), **API Keys** (best for sync service — designed for external integrations), OIDC (optional).

---

## 5. Sync Design

### 5.1 ID Mapping Strategy

The fundamental challenge: given a comic in YACReader, find the corresponding media in Stump (and vice versa). Three approaches were evaluated:

#### Approach A: Hash-Based Matching

| | YACReader | Stump |
|---|-----------|-------|
| Algorithm | SHA1 | SHA256 |
| Input | First 512 KB of file | 4 samples of 10 KB at evenly spaced offsets |
| Storage | `comic_info.hash` (always computed) | `media.hash` (optional per library) |

**Verdict: Not viable as primary strategy.** The algorithms differ in both hash function and sampling method. The same file produces completely different hashes in each system. Using hashes for matching would require the sync service to compute both algorithms against the actual files on disk, adding filesystem access as a dependency and significant I/O cost.

#### Approach B: Path-Based Matching

Both servers scan the same directory tree. YACReader stores **relative** paths (from library root); Stump stores **absolute** paths.

```
Shared filesystem:    /srv/comics/Marvel/Spider-Man/001.cbz
YACReader path:       Marvel/Spider-Man/001.cbz
Stump path:           /srv/comics/Marvel/Spider-Man/001.cbz
```

Conversion: `stump_path = library_root + "/" + yacreader_path`

| Pros | Cons |
|------|------|
| No filesystem access needed | Breaks if library roots differ between systems |
| Works via APIs only | Breaks on file renames (until both rescan) |
| Fast — string comparison | Case sensitivity depends on OS |
| Covers 99%+ of cases | Requires knowing both library root paths |

**Verdict: Best primary strategy.** Since both servers scan the same directory, paths will match after normalization.

#### Approach C: Filename + Size Fallback

When paths don't match (e.g., after a reorganization where one server has rescanned but the other hasn't), fall back to matching by filename and file size.

```sql
-- YACReader: comic.fileName + comic_info via join
-- Stump: media.name + media.size
```

| Pros | Cons |
|------|------|
| Survives directory reorganization | Ambiguous if multiple files have same name+size |
| Simple to implement | Requires iterating candidates |

**Verdict: Good fallback, not sufficient alone.**

#### Recommended: Hybrid Strategy

```
1. PRIMARY:   Relative path match (normalize and compare)
2. FALLBACK:  Filename + filesize match (for moved files)
3. MANUAL:    Config file overrides for edge cases
```

The sync service stores confirmed mappings in its own database, so matching only needs to happen once per comic. Subsequent syncs use the cached mapping.

### 5.2 Field Mapping Table

| YACReader Field | Stump Field | Direction | Notes |
|-----------------|-------------|-----------|-------|
| `comic_info.currentPage` | `reading_sessions.page` | ↔ | Core sync field |
| `comic_info.read` | Presence in `finished_reading_sessions` | ↔ | YAC bool ↔ Stump completion event |
| `comic_info.hasBeenOpened` | Presence of any session (active or finished) | ↔ | Derived, not directly synced |
| `comic_info.lastTimeOpened` | `reading_sessions.updated_at` | ↔ | Epoch seconds ↔ datetime |
| `comic_info.numPages` | `media.pages` | read-only | Should match (same file); used for completion check |
| `comic_info.rating` | *(no equivalent)* | — | YACReader only |
| `comic_info.bookmark1/2/3` | *(no equivalent)* | — | YACReader only |
| *(no equivalent)* | `reading_sessions.elapsed_seconds` | — | Stump only |
| *(no equivalent)* | `reading_sessions.percentage_completed` | — | Stump only; derivable as `page/numPages` |

### 5.3 Conflict Resolution

When both systems have changed progress for the same comic since the last sync:

| Field | Resolution Rule | Rationale |
|-------|----------------|-----------|
| `currentPage` / `page` | `max(yacreader, stump)` | Reading progresses forward; the higher page is always more recent progress |
| `read` / completion | `OR` — once read, stays read | Marking unread is a deliberate action, not a sync artifact |
| `lastTimeOpened` / `updated_at` | Latest timestamp wins | Standard LWW (Last-Writer-Wins) |

**Edge cases:**

1. **Comic completed in Stump but not YACReader**: Stump has a `finished_reading_session` but no active session. Sync service sets YACReader `read = true` and `currentPage = numPages`.

2. **Comic completed in YACReader but not Stump**: YACReader has `read = true`. Sync service calls `markMediaAsComplete(id, isComplete: true)` on Stump.

3. **Page regressed**: User re-reads from an earlier page. The `max()` rule would ignore this. Mitigation: if the timestamp on the "lower" page is significantly newer (configurable threshold, e.g., > 5 minutes), treat it as an intentional re-read and accept the lower page. Default behavior: always advance forward (matches YACReader's own sync behavior).

### 5.4 Sync Protocol

#### Reading from YACReader

**Preferred: Direct SQLite read** (read-only, safe with concurrent server access).

```sql
SELECT ci.id, ci.hash, ci.currentPage, ci.hasBeenOpened, ci.read,
       ci.lastTimeOpened, ci.rating, ci.numPages,
       c.fileName, c.path
FROM comic_info ci
JOIN comic c ON c.comicInfoId = ci.id
WHERE ci.lastTimeOpened > :last_sync_timestamp
   OR ci.currentPage > 1
   OR ci.read = 1;
```

This efficiently fetches only comics with reading progress. The `.ydb` file path is `<library_path>/.yacreaderlibrary/library.ydb`.

#### Writing to YACReader

**Preferred: HTTP API** (`POST /v2/sync` for batch, `POST /v2/library/{id}/comic/{comicId}/update` for single).

The batch sync endpoint is designed for exactly this use case. The sidecar formats progress updates as tab-separated rows and posts them. The server's built-in conflict resolution (max page, OR read) aligns with our strategy.

#### Reading from Stump

**Preferred: GraphQL query.**

```graphql
query {
  libraries {
    id
    name
    path
    series {
      media {
        id
        name
        size
        pages
        path
        readProgress {
          page
          percentageCompleted
          updatedAt
        }
        readHistory {
          completedAt
        }
      }
    }
  }
}
```

#### Writing to Stump

**Preferred: GraphQL mutations.**

```graphql
# Update page progress
mutation {
  updateMediaProgress(id: "uuid", input: { paged: { page: 42 } })
}

# Mark as complete
mutation {
  markMediaAsComplete(id: "uuid", isComplete: true)
}
```

**Authentication**: API key, passed as a header. Created in Stump's admin UI.

### 5.5 Real-Time Sync Mechanisms

The polling-based sync cycle described in §5.4 introduces up to 60 seconds of latency between a reading progress change and its propagation. This section describes event-driven approaches that reduce sync latency from tens of seconds to sub-second, while retaining polling as a reliable fallback.

#### 5.5.1 Polling vs Event-Driven Overview

| Approach | Latency | Complexity | Reliability |
|----------|---------|------------|-------------|
| Polling only (§5.4) | 0–60s (configurable) | Low | Very high — no state to manage |
| PRAGMA data_version + filesystem watch | 0–5s | Medium | High — OS-level primitives |
| Fork: webhook from YACReaderServer | <100ms | Low (after fork) | High — HTTP POST on each event |
| Stump WebSocket subscription | Instant | Medium | Moderate — requires reconnection logic |
| Combined (recommended) | <5s typical, instant for Stump events | Medium–High | Very high with polling fallback |

The recommended production architecture combines event-driven triggers with polling as a heartbeat. Events trigger immediate sync cycles; polling catches anything missed.

#### 5.5.2 Stump: GraphQL WebSocket Subscriptions

**What works today (no Stump changes):**

Stump exposes a GraphQL subscription endpoint at `GET /api/graphql/ws` using the `graphql-ws` WebSocket protocol. The schema defines:

```graphql
type Subscription {
  readEvents: CoreEvent!
}
```

The subscription is backed by a `tokio::sync::broadcast` channel (capacity 1024) held in the `Ctx` struct. The sidecar can subscribe and receive `CoreEvent` variants in real time:

| CoreEvent Variant | Trigger | Useful for Sync? |
|-------------------|---------|------------------|
| `JobStarted` | Scan/task begins | No — informational |
| `JobUpdate` | Scan progress | No |
| `JobOutput` | Task output | No |
| `CreatedMedia` | New media indexed | Yes — trigger ID mapping rebuild |
| `CreatedManySeries` | Bulk series creation | Yes — trigger ID mapping rebuild |
| `CreatedOrUpdatedManyMedia` | Bulk media changes | Yes — trigger ID mapping rebuild |
| `DiscoveredMissingLibrary` | Library path gone | No — error condition |

This means the sidecar can **already** subscribe to library structure changes (new comics added, scan completions) and rebuild its ID mappings immediately rather than waiting for the next poll cycle.

**The critical gap: no reading progress events.**

The `updateMediaProgress` GraphQL mutation (`crates/graphql/src/mutation/media.rs:300`) writes directly to the database without emitting a `CoreEvent`. The same is true for the KoReader sync and OPDS v2.0 progression endpoints. This means reading progress changes in Stump are **invisible** to WebSocket subscribers.

**Minimal Stump contribution (~20 lines of Rust):**

Adding a new variant to the `CoreEvent` enum and emitting it from the three progress-update code paths would make progress changes flow through the existing broadcast → subscription pipeline automatically:

```rust
// In core/src/event.rs — add variant to CoreEvent enum
ReadingProgressUpdated {
    user_id: String,
    media_id: String,
    page: i32,
    percentage: Option<f64>,
    is_complete: bool,
}
```

Emit sites (each ~3 lines):
1. `update_media_progress` — GraphQL mutation handler
2. KoReader sync endpoint
3. OPDS v2.0 `PUT /books/{id}/progression`

This is a small, self-contained contribution that benefits any Stump integration.

**Sidecar WebSocket client:**

```python
import asyncio
import websockets
import json

SUBSCRIPTION_QUERY = """
subscription {
  readEvents {
    __typename
    ... on CreatedMedia { id }
    ... on CreatedOrUpdatedManyMedia { count }
    # ReadingProgressUpdated fields (after Stump contribution)
  }
}
"""

async def subscribe_stump_events(url: str, on_event):
    async for ws in websockets.connect(url, subprotocols=["graphql-transport-ws"]):
        try:
            await ws.send(json.dumps({"type": "connection_init"}))
            await ws.recv()  # connection_ack
            await ws.send(json.dumps({
                "id": "1",
                "type": "subscribe",
                "payload": {"query": SUBSCRIPTION_QUERY},
            }))
            async for msg in ws:
                data = json.loads(msg)
                if data.get("type") == "next":
                    await on_event(data["payload"]["data"]["readEvents"])
        except websockets.ConnectionClosed:
            continue  # Auto-reconnect via `async for`
```

#### 5.5.3 YACReader: Change Detection

**Existing Qt signals (connected in GUI, unconnected in headless server):**

YACReaderLibraryServer already emits Qt signals on progress updates:
- `requestmapper.cpp:131` — `emit clientSync()` after `POST /v2/sync` batch sync
- `requestmapper.cpp:153` — `emit comicUpdated(updatedLibraryId, updatedComicId)` after single comic update

These signals are relayed from `RequestMapper` → `YACReaderHttpServer` (`yacreader_http_server.cpp:110–111`), but in the headless `YACReaderLibraryServer/main.cpp`, **they are never connected to anything**. The GUI version uses them for UI refresh.

**Approach A: PRAGMA data_version (best no-fork option)**

SQLite's `PRAGMA data_version` returns a counter that increments on any write to the database, even from other connections or processes. This enables efficient cross-process change detection:

```python
import sqlite3

class YACReaderChangeDetector:
    def __init__(self, ydb_path: str):
        self.conn = sqlite3.connect(f"file:{ydb_path}?mode=ro", uri=True)
        self.last_data_version = self._get_data_version()

    def _get_data_version(self) -> int:
        return self.conn.execute("PRAGMA data_version").fetchone()[0]

    def has_changes(self) -> bool:
        current = self._get_data_version()
        if current != self.last_data_version:
            self.last_data_version = current
            return True
        return False
```

| Property | Value |
|----------|-------|
| Latency | 0–5s (depends on poll interval for this PRAGMA) |
| Overhead | Negligible — no data read, just a version counter |
| Cross-process | Yes — detects writes from YACReaderServer's process |
| Platform | All (SQLite built-in) |
| Limitation | Only signals "something changed" — follow-up query needed |

The follow-up query uses `lastTimeOpened` to find what changed:

```sql
SELECT ci.id, ci.currentPage, ci.read, ci.lastTimeOpened
FROM comic_info ci
WHERE ci.lastTimeOpened > :last_known_timestamp;
```

**Approach B: Filesystem watching**

Watch the `.ydb` file for modifications using OS-level file notification APIs:

| Platform | Mechanism | Latency | Notes |
|----------|-----------|---------|-------|
| Linux | inotify (`IN_MODIFY`) | <10ms | Kernel-level, very reliable |
| macOS | kqueue / FSEvents | <10ms / ~500ms | kqueue for file-level; FSEvents for directory-level |
| Cross-platform | Python `watchdog` library | Varies by backend | Abstracts platform differences |

Caveats:
- SQLite in default journal mode (`delete`) generates writes to both the main DB file and a `-journal` file. Filter events to the main `.ydb` file only.
- Debounce rapid successive writes (SQLite may trigger multiple filesystem events per transaction).

**Recommended no-fork hybrid: PRAGMA data_version + filesystem watchdog**

```
1. Open library.ydb read-only
2. Start watchdog observer on .ydb file
3. On filesystem event OR every 5 seconds:
   → Check PRAGMA data_version
   → If changed: query comic_info WHERE lastTimeOpened > last_known
   → Trigger sync cycle for changed comics
4. Latency: 0–5 seconds. Overhead: negligible. Reliability: very high.
```

The filesystem watcher provides near-instant notification; the PRAGMA check confirms an actual data change (filtering out journal-file noise); the 5-second polling interval catches any events the watcher might miss.

**Fork options (minimal YACReaderLibraryServer changes):**

| Option | Lines Changed | Latency | Implementation |
|--------|---------------|---------|----------------|
| **Webhook** | ~10 | <100ms | After `emit clientSync()` / `emit comicUpdated(...)`, add `QNetworkAccessManager::post()` to configurable URL |
| **Connect signals to Unix socket** | ~3 | <50ms | In `main.cpp`, connect existing signals to `QLocalServer` |
| **Touch sentinel file** | ~5 | <100ms | Touch `.sync_event` file after sync; sidecar watches with `watchdog` |
| **Unix socket server** | ~15 | <50ms | Create `QLocalServer` that emits JSON events with libraryId, comicId |

The webhook option is recommended for forks — it provides rich event data (library ID, comic ID) with minimal code change and works over the network.

#### 5.5.4 Combined Real-Time Architecture

```
┌─────────────┐
│  iPhone      │
│  YACReader   │──── POST /v2/sync ────┐
│  iOS App     │                        │
└──────────────┘                        ▼
                              ┌──────────────────────┐
                              │ YACReaderLibrary-     │
                              │ Server                │
                              │                       │
                              │  library.ydb ◄────────┼──── writes on sync
                              └──────────┬────────────┘
                                         │
              ┌──────────────────────────┐│
              │                          ││ filesystem events
              │   Sync Sidecar Service   ││ + PRAGMA data_version
              │                          │◄┘
              │  ┌────────────────────┐  │
              │  │ Event Listeners    │  │
              │  │                    │  │
              │  │ • watchdog on .ydb │  │        ┌─────────────────┐
              │  │ • PRAGMA polling   │──┼───────►│                 │
              │  │ • Stump WebSocket  │◄─┼────────│  Stump          │
              │  │ • Polling fallback │  │  WS    │                 │
              │  └────────────────────┘  │        │  GraphQL WS     │
              │                          │        │  /api/graphql/ws│
              │  ┌────────────────────┐  │        └─────────────────┘
              │  │ Sync Engine        │  │
              │  │ • match IDs        │  │
              │  │ • resolve conflicts│  │
              │  │ • write updates    │  │
              │  └────────────────────┘  │
              └──────────────────────────┘
```

**Event routing logic:**

| Event Source | Triggers | Sync Direction |
|--------------|----------|----------------|
| Filesystem watch / PRAGMA data_version change | Immediate sync cycle | YACReader → Stump |
| Stump WebSocket `CreatedMedia` / `CreatedOrUpdatedManyMedia` | ID mapping rebuild | — (structure only) |
| Stump WebSocket `ReadingProgressUpdated` (after contribution) | Immediate sync cycle | Stump → YACReader |
| Polling timer (every 60–300s) | Full sync cycle | Bidirectional |

The polling fallback runs on a longer interval (60–300s) than the original design since events handle the fast path. It serves as a consistency check and catches any events lost to network interruptions or race conditions.

#### 5.5.5 Latency Comparison

| Approach | YAC→Stump Latency | Stump→YAC Latency | Requires Code Changes? |
|----------|-------------------|--------------------|------------------------|
| Polling only (§5.4) | 0–60s | 0–60s | No |
| + PRAGMA data_version + watchdog | **0–5s** | 0–60s | No |
| + Stump WebSocket (current events) | 0–5s | 0–60s (progress not evented) | No |
| + Stump WebSocket (with contribution) | 0–5s | **Instant** | ~20 lines Rust in Stump |
| + YACReader fork webhook | **<100ms** | 0–60s | ~10 lines C++/Qt in YACReader |
| Full real-time (both contributions) | **<100ms** | **Instant** | Both forks |

---

## 6. Sidecar Service Architecture

### 6.1 Technology Choice

**Recommended: Python 3.11+**

| Factor | Rationale |
|--------|-----------|
| SQLite access | `sqlite3` in stdlib — no dependencies for YACReader DB reads |
| HTTP client | `httpx` — async-capable, handles both REST and GraphQL |
| GraphQL | Raw `httpx` POST to `/api/graphql` (no heavy GraphQL client needed) |
| Deployment | Single script or small package, Docker-friendly |
| Iteration speed | Fastest to prototype and debug |
| Ecosystem | `pydantic` for config validation, `structlog` for logging |

**Alternative for Phase 4**: If embedding into YACReaderLibraryServer (C++/Qt), rewrite the sync logic in C++ using Qt's `QNetworkAccessManager` for HTTP and `QSqlDatabase` for SQLite.

### 6.2 Configuration

```yaml
# sync_config.yaml
yacreader:
  server_url: "http://localhost:8080"    # YACReaderLibraryServer HTTP API
  libraries:
    - name: "Comics"
      path: "/srv/comics"               # Filesystem path to library root
      ydb_path: "/srv/comics/.yacreaderlibrary/library.ydb"

stump:
  server_url: "http://localhost:10801"
  api_key: "stump_api_key_here"
  user_id: "uuid-of-target-user"         # Stump user whose progress to sync

sync:
  interval_seconds: 60                   # Polling interval
  direction: "bidirectional"             # "yac_to_stump" | "stump_to_yac" | "bidirectional"
  conflict_page_strategy: "max"          # "max" | "latest_timestamp"
  mapping_db_path: "/data/sync/mapping.db"

logging:
  level: "INFO"
```

### 6.3 Data Flow

Each sync cycle follows this sequence:

```
┌─────────────────────────────────────────────────────┐
│                    SYNC CYCLE                        │
├─────────────────────────────────────────────────────┤
│                                                      │
│  1. READ YACReader progress                          │
│     ├─ Open .ydb (read-only)                        │
│     ├─ Query comics with progress since last sync   │
│     └─ Close .ydb                                   │
│                                                      │
│  2. READ Stump progress                              │
│     ├─ GraphQL query: libraries → media → progress  │
│     └─ Collect active sessions + completion history  │
│                                                      │
│  3. MATCH comics across systems                      │
│     ├─ Check mapping DB for cached matches          │
│     ├─ For unmatched: try relative path match       │
│     ├─ For still unmatched: try filename+size       │
│     └─ Store new matches in mapping DB              │
│                                                      │
│  4. COMPARE & RESOLVE per matched pair               │
│     ├─ Compare currentPage / page                   │
│     ├─ Compare read / completion status             │
│     ├─ Compare timestamps                           │
│     └─ Apply conflict resolution rules              │
│                                                      │
│  5. WRITE updates                                    │
│     ├─ Push to Stump: GraphQL mutations             │
│     ├─ Push to YACReader: HTTP POST /v2/sync        │
│     └─ Update mapping DB sync timestamps            │
│                                                      │
└─────────────────────────────────────────────────────┘
```

**Step-by-step detail:**

1. **Read YACReader**: Open the `.ydb` file in read-only mode (`sqlite3.connect("file:path?mode=ro", uri=True)`). Query `comic_info` joined with `comic` for all entries where `lastTimeOpened > last_sync_time OR currentPage > 1 OR read = 1`. This captures all comics with any reading activity.

2. **Read Stump**: Send a GraphQL query to fetch all media with their `readProgress` (active session) and `readHistory` (finished sessions). Filter to the configured user's progress.

3. **Match**: For each YACReader comic, compute `relative_path = comic.path + "/" + comic.fileName`. Look up in the mapping DB. If not found, search Stump media where `media.path == library_root + "/" + relative_path`. If still not found, try `media.name == comic.fileName AND media.size ~= filesize`. Cache successful matches.

4. **Compare**: For each matched pair, compare the sync-relevant fields. Determine which system is "ahead" using the conflict resolution rules from §5.3.

5. **Write**: Batch updates to YACReader via `POST /v2/sync`. Individual updates to Stump via GraphQL mutations. Record the new sync state in the mapping DB.

#### Event-Driven Flow (Real-Time Enhancement)

When real-time sync is enabled (§5.5), the sidecar runs persistent event listeners alongside the polling cycle. Events trigger targeted sync operations rather than full cycles:

```
┌─────────────────────────────────────────────────────────────────┐
│                  EVENT-DRIVEN SYNC FLOW                         │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  EVENT SOURCE A: Filesystem watch + PRAGMA data_version          │
│  (YACReader .ydb changed)                                        │
│                                                                  │
│    watchdog detects .ydb modification                            │
│      → debounce 500ms                                            │
│      → PRAGMA data_version — changed?                            │
│        ├─ NO  → ignore (journal noise)                          │
│        └─ YES → query changed comics (lastTimeOpened > last)    │
│                 → match IDs (use cached mapping)                │
│                 → write to Stump (GraphQL mutations)            │
│                 → update sync_state                              │
│                                                                  │
│  EVENT SOURCE B: Stump WebSocket subscription                    │
│  (GraphQL readEvents)                                            │
│                                                                  │
│    CreatedMedia / CreatedOrUpdatedManyMedia                       │
│      → rebuild ID mappings for affected library                  │
│                                                                  │
│    ReadingProgressUpdated (after Stump contribution)             │
│      → look up media_id in mapping DB                           │
│      → compare with cached YACReader state                      │
│      → if Stump is ahead: POST /v2/sync to YACReader           │
│      → update sync_state                                         │
│                                                                  │
│  EVENT SOURCE C: Polling heartbeat (every 60–300s)               │
│                                                                  │
│    Full sync cycle (steps 1–5 above)                             │
│      → catches anything missed by event sources                 │
│      → verifies consistency of event-driven syncs               │
│                                                                  │
│  THROTTLE: Only one sync operation runs at a time.               │
│  Queued events coalesce into a single follow-up cycle.           │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### 6.4 State Management

The sidecar maintains a SQLite database (`mapping.db`) with three tables:

```sql
CREATE TABLE library_mapping (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    yac_library_id  TEXT NOT NULL,       -- YACReader library UUID
    yac_library_name TEXT,
    stump_library_id TEXT NOT NULL,      -- Stump library UUID
    stump_library_name TEXT,
    library_root    TEXT NOT NULL,       -- Shared filesystem path
    UNIQUE(yac_library_id, stump_library_id)
);

CREATE TABLE media_mapping (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    library_mapping_id INTEGER NOT NULL REFERENCES library_mapping(id),
    yac_comic_info_id INTEGER NOT NULL,  -- comic_info.id in YACReader
    yac_comic_id    INTEGER NOT NULL,    -- comic.id in YACReader
    stump_media_id  TEXT NOT NULL,       -- media.id (UUID) in Stump
    relative_path   TEXT NOT NULL,       -- Normalized relative path
    filename        TEXT NOT NULL,
    matched_via     TEXT NOT NULL,       -- "path" | "filename_size" | "manual"
    matched_at      TEXT NOT NULL,       -- ISO 8601 timestamp
    UNIQUE(yac_comic_info_id, stump_media_id)
);

CREATE TABLE sync_state (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    media_mapping_id INTEGER NOT NULL REFERENCES media_mapping(id),
    yac_current_page INTEGER,
    stump_current_page INTEGER,
    yac_read        INTEGER,             -- 0 or 1
    stump_complete  INTEGER,             -- 0 or 1
    yac_last_modified TEXT,              -- epoch seconds from YACReader
    stump_last_modified TEXT,            -- ISO 8601 from Stump
    last_synced_at  TEXT NOT NULL,       -- ISO 8601
    UNIQUE(media_mapping_id)
);
```

The `sync_state` table stores the last-known state from each system. On the next sync cycle, the sidecar compares current values against stored values to detect changes. Only changed comics trigger writes.

### 6.5 Deployment

#### Docker Compose (recommended)

```yaml
version: "3.8"

services:
  stump:
    image: aaronleopold/stump:latest
    ports:
      - "10801:10801"
    volumes:
      - /srv/comics:/comics
      - stump_data:/data

  yacreaderlibraryserver:
    image: xthursdayx/yacreaderlibrary-server-docker:latest
    ports:
      - "8080:8080"
    volumes:
      - /srv/comics:/comics

  sync-sidecar:
    build: ./sync-sidecar
    volumes:
      - /srv/comics:/comics:ro          # Read-only access to .ydb files
      - sync_data:/data                 # Mapping DB storage
    environment:
      - SYNC_CONFIG_PATH=/data/sync_config.yaml
    depends_on:
      - stump
      - yacreaderlibraryserver
    restart: unless-stopped

volumes:
  stump_data:
  sync_data:
```

#### Tailscale Considerations

- All three services run on the same host behind Tailscale.
- YACReader iOS connects to YACReaderLibraryServer via Tailscale IP/hostname.
- The sync sidecar accesses both servers via `localhost` — no Tailscale needed for sidecar traffic.
- The user's iPhone only needs to reach YACReaderLibraryServer (for reading) and optionally Stump (for its web UI).

---

## 7. Implementation Phases

The recommended implementation path is a **fork of YACReaderLibraryServer** with a Rust sync module linked via FFI. This approach embeds sync directly into the server process, eliminates the sidecar deployment, and leverages YACReaderLibraryServer's existing Qt signals for near-instant event-driven sync. See §9 for the full Rust FFI architecture.

### Phase 1: Shared Filesystem (No Code Changes)

**Goal**: Both servers index the same comic library.

**Steps**:
1. Set up the shared filesystem (`/srv/comics` or equivalent).
2. Deploy Stump and YACReaderLibraryServer, both scanning the shared directory.
3. Configure YACReader iOS to connect to YACReaderLibraryServer.
4. Verify both servers see the same comics.

**Outcome**: Reading works on both systems independently, but progress is not synced. This is the foundation for all subsequent phases.

**Effort**: ~1 hour of setup.

### Phase 2: Fork with One-Way Sync (YACReader → Stump) ✅ IMPLEMENTED

> **Status:** Implemented on branch [`feat/stump-sync-phase2`](https://github.com/dashed/yacreader/pull/1).
> **PR:** [dashed/yacreader#1](https://github.com/dashed/yacreader/pull/1)

**Goal**: When YACReader iOS syncs progress, it automatically pushes to Stump.

**Steps**:
1. Fork YACReaderLibraryServer.
2. Add the Rust `stump_sync` crate (§9.9) with:
   - GraphQL client for Stump progress mutations via `reqwest` (§9.7)
   - Path-based ID mapping with `rusqlite` (§9.8)
   - `cxx` FFI bridge exposing `rust_sync_init`, `rust_sync_push_progress`, `rust_sync_shutdown` (§9.3)
   - Tokio runtime with command channel (§9.4)
3. Integrate into the build via `corrosion` CMake module (§9.6).
4. In `main.cpp`, connect existing Qt signals to Rust FFI calls (§9.5):
   - `comicUpdated(libraryId, comicId)` → `rust_sync_push_progress()`
   - `clientSync()` → `rust_sync_push_all()`
5. Test: read a comic on iPhone → YACReader iOS syncs to server → signal fires → Rust pushes progress to Stump.

**Outcome**: Stump reflects YACReader reading progress in near-real-time (<100ms after iOS sync completes). Only 3 files modified in the YACReader codebase (§9.1).

**Effort**: ~1–2 weeks of development.

**Implementation notes (deviations from original plan):**
- Added `ffi` Cargo feature flag — `cxx` dependency is optional, gated behind `ffi` feature. This allows `cargo test` to run without a C++ toolchain. The CMake build uses `corrosion_import_crate(... FEATURES ffi)`.
- Crate type includes both `staticlib` (for C++ linking) and `lib` (for `cargo test`).
- `extern "C++"` block deferred to Phase 3 — Phase 2 is one-way (C++ calls Rust), so no Rust→C++ callbacks needed. This eliminates the need for C++ stubs during testing.
- `MappingDb` wraps `rusqlite::Connection` in `std::sync::Mutex` for thread safety with tokio.
- Configuration reads from `QSettings` under `[StumpSync]` group (serverUrl, apiKey, userId, mappingDbPath, syncIntervalSecs).
- 38 tests: 29 inline unit tests + 4 integration tests (mapping DB lifecycle) + 5 E2E tests (full sync flow with wiremock mock Stump server).

### Phase 3: Two-Way Sync + Real-Time

**Goal**: Progress flows in both directions. Reading on any client updates both systems.

**Steps**:
1. Add Stump → YACReader progress pulling to the Rust module:
   - Polling: periodic GraphQL query for progress changes (timer-based or `PRAGMA data_version` on Stump's DB if co-located)
   - Optional: WebSocket subscription to Stump's `readEvents` for library structure changes (§5.5.2)
2. Implement conflict resolution in the Rust sync engine (§5.3):
   - `currentPage`: `max(yacreader, stump)`
   - `read` / completion: `OR` (once read, stays read)
   - `lastTimeOpened` / `updated_at`: latest timestamp wins (LWW)
3. Add Rust → C++ callback for updating YACReader's `.ydb`:
   - Rust calls a C++ function via the `cxx` bridge
   - C++ side uses `QMetaObject::invokeMethod(Qt::QueuedConnection)` to marshal to the Qt thread (§9.5)
   - Writes to YACReader DB via existing `DBHelper` functions
4. Test: read in Stump web reader → Rust module detects change → writes to YACReader → progress visible on next iOS sync.

**Outcome**: Full bidirectional sync. Reading on iPhone (via YACReader) or desktop/web (via Stump) updates both systems.

**Effort**: ~1–2 weeks of additional development.

### Phase 4: Polish and Optional Upstream

**Goal**: Production-quality deployment and community contributions.

**Steps**:
1. Contribute `ReadingProgressUpdated` CoreEvent to Stump (~20 lines of Rust, §5.5.2). This enables instant Stump → YACReader sync via WebSocket instead of polling.
2. Add configuration support:
   - Config file (`stump_sync.toml` or YAML) for Stump URL, API key, user ID, sync interval
   - Optional: CLI flags for YACReaderLibraryServer to specify config path
3. Docker image with Rust build stage:
   - Multi-stage Dockerfile: Rust builder → corrosion + cargo build → copy `.a` into CMake build stage
   - CI build matrix additions for Rust toolchain
4. Consider upstreaming the sync module to YACReader if applicable — the `stump_sync` crate is cleanly separated and could be made optional via a CMake flag.

**Outcome**: Polished, deployable sync solution with sub-second latency in both directions.

**Effort**: ~1 week.

### Sidecar Alternative

The fork approach above is recommended for its lower deployment complexity (single process) and direct access to Qt signals for event-driven sync. However, a **Python sidecar** is a viable alternative that avoids forking YACReaderLibraryServer entirely. This was the original architecture proposed in §6.

**Sidecar Phase 2** (One-Way, YAC → Stump): Build a Python sidecar with `.ydb` SQLite reader, Stump GraphQL client, path-based matching, and a mapping DB. Deploy as a Docker container alongside both servers. Effort: ~2–3 days. Latency: 0–60s (polling) or 0–5s (with `PRAGMA data_version` + filesystem `watchdog`).

**Sidecar Phase 3** (Two-Way): Add Stump → YACReader sync via GraphQL reads and `POST /v2/sync` writes. Implement conflict resolution. Add Stump WebSocket subscription for library structure events. Effort: ~2–3 additional days.

**Sidecar Phase 4** (Real-Time Enhancement): Add a minimal YACReaderLibraryServer fork with webhook notifications (~10 lines of C++/Qt) — after `emit clientSync()` and `emit comicUpdated(...)`, POST to a configurable URL. The sidecar receives HTTP events for sub-100ms YACReader → Stump sync. This is a much smaller fork surface than the full Rust FFI approach.

**When to prefer the sidecar**: If you want to avoid maintaining a fork, need rapid prototyping, or prefer Python's iteration speed. The sidecar architecture is fully described in §6.

---

## 8. Critical Challenges and Mitigations

### 8.1 Hash Algorithm Mismatch

| | YACReader | Stump |
|---|-----------|-------|
| Algorithm | SHA1 | SHA256 |
| Input | First 512 KB (contiguous) | 4 × 10 KB samples (evenly spaced) |
| Required | Always computed | Optional (`generate_file_hashes` config) |

**Impact**: Cannot use hashes to match comics across systems.

**Mitigation**: Use path-based matching as primary strategy (§5.1). Hashes are irrelevant for matching but remain useful within each system for their own dedup/identity purposes.

**Future option**: The sync sidecar could compute YACReader-style hashes for Stump media (by reading files from the shared filesystem) to enable hash-based fallback matching. This is expensive and rarely necessary.

### 8.2 Single-User vs Multi-User Model

**YACReader**: No user concept. All progress is global — one set of progress per comic.

**Stump**: Multi-user. All progress is scoped to a `(user_id, media_id)` pair.

**Mitigation**: The sidecar configuration specifies a single `stump.user_id`. All YACReader progress maps to/from this one Stump user. If the Stump instance has multiple users, only the configured user's progress is synced.

### 8.3 Path Differences (Relative vs Absolute)

**YACReader**: Stores relative paths from library root. `comic.path` is the directory path (e.g., `"Marvel/Spider-Man"`), `comic.fileName` is the filename (e.g., `"001.cbz"`).

**Stump**: Stores absolute paths in `media.path` (e.g., `"/srv/comics/Marvel/Spider-Man/001.cbz"`).

**Mitigation**: The sidecar knows both library roots (from config). Conversion:
```
yac_full_relative = comic.path + "/" + comic.fileName
stump_relative = stump_media.path.removeprefix(stump_library.path + "/")
match = (yac_full_relative == stump_relative)
```

Path normalization: strip leading/trailing slashes, normalize separators to `/`, handle empty `comic.path` (root-level files).

### 8.4 Stump Hashes Are Optional

Stump only computes file hashes if `library_configs.generate_file_hashes` is enabled for that library. Many libraries will have `NULL` hashes.

**Impact**: Cannot rely on Stump hashes for any matching strategy.

**Mitigation**: Path-based matching does not depend on hashes. This is a non-issue for the recommended approach.

### 8.5 Library DB Isolation in YACReader

Each YACReader library has its own `.ydb` file. Comic IDs are only unique within a single library database.

**Impact**: The sidecar must track which library each comic belongs to. A comic ID like `42` means nothing without its library context.

**Mitigation**: The `media_mapping` table includes `library_mapping_id`, which ties each comic mapping to a specific (YACReader library, Stump library) pair. The sidecar iterates over all configured libraries.

### 8.6 Stump Completion Model

When a comic is completed in Stump (`page >= total_pages`), the active `reading_sessions` row is **deleted** and a `finished_reading_sessions` row is created.

**Impact**: After completion, there is no `reading_sessions.page` to read. The sync service must check both `readProgress` (active) and `readHistory` (finished).

**Mitigation**: Sync logic:
```python
if stump_media.readProgress:
    stump_page = stump_media.readProgress.page
    stump_complete = False
elif stump_media.readHistory:
    stump_page = stump_media.pages  # Assume last page
    stump_complete = True
else:
    stump_page = 0
    stump_complete = False
```

### 8.7 YACReader API Efficiency

The YACReader HTTP API has no endpoint to list all comics with their progress in a single call. `GET /v2/library/{id}/folder/{folderId}/content` returns folder contents but requires recursive traversal.

**Impact**: Using the HTTP API alone for reading progress is inefficient — requires many requests.

**Mitigation**: Read progress directly from the `.ydb` SQLite file (read-only mode). This is a single query, fast, and safe for concurrent access (SQLite supports multiple readers). Use the HTTP API only for writes.

### 8.8 WebSocket Reconnection

The Stump GraphQL WebSocket subscription is a long-lived connection that will inevitably drop — server restarts, network blips, Tailscale reconnects, container restarts.

**Impact**: Missed events during disconnection. The sidecar believes it's listening but receives nothing.

**Mitigation**:
- **Reconnect with exponential backoff**: Start at 1s, cap at 60s. The `websockets` library's `async for` pattern handles this automatically.
- **Full sync on reconnect**: After re-establishing the subscription, run one complete polling cycle to catch events missed during the disconnection window.
- **Heartbeat monitoring**: The `graphql-ws` protocol supports `ping`/`pong` frames. If no pong is received within 30s, treat the connection as dead and reconnect.
- **Connection state logging**: Log connect/disconnect events with timestamps so missed-event windows are auditable.

### 8.9 Filesystem Watch Reliability

Filesystem watchers (inotify, kqueue, FSEvents) have platform-specific quirks that affect reliability.

**Impact**: False positives (journal file writes), missed events (buffer overflow), or platform-dependent behavior.

**Mitigation**:
- **Journal file filtering**: SQLite in default journal mode creates and deletes `library.ydb-journal` files during writes. Watch only the main `.ydb` file, not the journal.
- **Debouncing**: A single YACReader sync operation may trigger multiple rapid filesystem events (journal create → DB write → journal delete). Debounce with a 500ms–1s window before checking `PRAGMA data_version`.
- **inotify queue overflow** (Linux): The default `max_queued_events` (16384) is sufficient, but under extreme write load, events can be dropped. The PRAGMA polling fallback (every 5s) catches these.
- **macOS FSEvents latency**: Directory-level FSEvents can batch with ~500ms latency. For file-level precision, use kqueue — Python's `watchdog` library selects the appropriate backend automatically.
- **Docker/NFS considerations**: Filesystem watchers do not work reliably across Docker volume mounts backed by network filesystems (NFS, CIFS). If the `.ydb` is on a network mount, fall back to PRAGMA-only polling.

### 8.10 Event Ordering and Deduplication

When multiple event sources (filesystem watch, WebSocket, polling) trigger sync cycles concurrently, duplicate or out-of-order operations can occur.

**Impact**: Redundant API calls, potential for stale data overwriting fresh data.

**Mitigation**:
- **Idempotent sync operations**: All write operations must be idempotent. The conflict resolution rules (§5.3) ensure that writing a "stale" value is harmless — `max(page)` and `OR(read)` converge regardless of application order.
- **Sync cycle serialization**: Use an `asyncio.Lock` (or equivalent) to ensure only one sync cycle runs at a time. If an event arrives during an active cycle, queue it and run one follow-up cycle after the current one completes. Coalesce multiple queued events into a single cycle.
- **Timestamp-based deduplication**: Track the timestamp of the last completed sync cycle per library. Skip a triggered cycle if less than 1 second has elapsed since the last one (rapid-fire events from a single batch sync).
- **Event source attribution**: Log which source triggered each sync cycle (filesystem, WebSocket, polling) for debugging and tuning.

```python
class SyncThrottle:
    def __init__(self, min_interval: float = 1.0):
        self.lock = asyncio.Lock()
        self.last_sync: float = 0
        self.pending = False

    async def trigger(self, source: str):
        if self.lock.locked():
            self.pending = True
            return
        async with self.lock:
            elapsed = time.monotonic() - self.last_sync
            if elapsed < self.min_interval:
                return
            await self._run_sync_cycle(source)
            self.last_sync = time.monotonic()
            if self.pending:
                self.pending = False
                await self._run_sync_cycle("coalesced")
                self.last_sync = time.monotonic()
```

### 8.11 Rust–C++ FFI Complexity

The `cxx` bridge provides type-safe FFI but introduces constraints:

| Constraint | Impact | Mitigation |
|------------|--------|------------|
| No `Option<T>` in bridge types | Cannot express nullable fields directly | Use sentinel values (`-1` for missing page, empty string for missing error) or wrapper structs with `has_value: bool` |
| No `async fn` in bridge | Cannot call async Rust from C++ directly | Post work to tokio via `mpsc` channel from bridge functions; return immediately |
| String conversion overhead | `rust::String` ↔ `QString` requires UTF-8 encode/decode | Negligible for progress data (small strings); batch conversions for bulk operations |
| Debugging across FFI boundary | Stack traces don't cross the Rust/C++ boundary cleanly | Use `tracing` on Rust side, `qDebug()` on C++ side; log at the FFI entry/exit points |
| Build error messages | `cxx` codegen errors can be cryptic | Keep the bridge definition minimal; test bridge compilation independently |

The bridge definition should be kept as small as possible — only the types and functions that must cross the boundary. Internal Rust logic stays in pure Rust modules.

### 8.12 Tokio + Qt Event Loop Coexistence

Two event loops run concurrently: Qt's `QCoreApplication::exec()` on the main thread and Tokio's multi-threaded runtime on its own thread pool.

**Thread safety**: The `cxx` bridge functions are called from the Qt main thread. They must not block (would freeze the server's HTTP handling). All bridge functions post commands to the Tokio runtime via an `mpsc` channel and return immediately.

**Callback marshaling**: When the Rust side needs to update YACReader's database (Stump → YAC direction), it must marshal the call back to the Qt main thread. Raw function pointer callbacks from Rust execute on a Tokio worker thread — direct `DBHelper` calls from there are safe (DBHelper opens per-thread SQLite connections), but Qt signal emission or UI updates are not. Use `QMetaObject::invokeMethod(obj, Qt::QueuedConnection)` to queue the callback on the Qt event loop.

**Shutdown ordering**: The Rust runtime must shut down before `QCoreApplication` exits. Sequence:
1. `rust_sync_shutdown()` — sends `Shutdown` command via channel
2. Tokio runtime completes pending tasks (with timeout)
3. Runtime drops, all Rust resources cleaned up
4. `QCoreApplication::quit()` proceeds

**Deadlock prevention**: Never hold a Rust mutex while calling back into C++, and never hold a Qt mutex while calling into Rust. The channel-based design avoids this by decoupling the two sides.

### 8.13 Cross-Compilation

Adding Rust to the build introduces platform-specific considerations:

| Platform | Rust Target Triple | Extra Link Libraries | Notes |
|----------|-------------------|---------------------|-------|
| Linux x86_64 | `x86_64-unknown-linux-gnu` | `pthread`, `dl`, `m` | Most common deployment target (Docker) |
| Linux aarch64 | `aarch64-unknown-linux-gnu` | `pthread`, `dl`, `m` | ARM servers, Raspberry Pi |
| macOS x86_64 | `x86_64-apple-darwin` | `Security.framework`, `SystemConfiguration.framework` | Required by `rustls-tls` |
| macOS aarch64 | `aarch64-apple-darwin` | Same as above | Apple Silicon |

**CI implications**: All CI runners must have the Rust toolchain installed. `corrosion` handles debug/release mapping and platform-specific system library linking, but the Rust target must match the C++ target when cross-compiling.

**Docker builds**: Use a multi-stage Dockerfile with `rust:1.XX` as the builder stage for the `stump_sync` crate, then copy the compiled static library (`.a`) into the CMake build stage.

### 8.14 Dependency Management

The project now has two package managers: **Cargo** (Rust) and **CMake/system packages** (C++). This creates maintenance considerations:

- **Cargo.lock**: Must be committed to version control for reproducible builds. The `stump_sync` crate is an application-like artifact (not a library published to crates.io), so locking dependencies is correct.
- **Rust dependency updates**: `cargo update` is independent of CMake. Security advisories can be monitored via `cargo audit`. The `reqwest` + `tokio` stack has frequent releases but is stable.
- **Vendoring**: For fully reproducible builds without network access, `cargo vendor` can download all crate sources into a local directory. This adds ~50MB to the repo but eliminates build-time network dependencies.
- **Version pinning**: Pin `corrosion` to a specific Git tag in `FetchContent_Declare` to avoid build breakage from upstream changes.
- **Binary size**: The Rust static library adds ~3–5MB to the final executable (with `reqwest` + `tokio` + `rusqlite`). Use `opt-level = "z"` and `lto = true` in the release profile to minimize this.

---

## 9. Rust FFI Fork Architecture

### 9.1 Overview

The recommended approach embeds a Rust sync module directly into **YACReaderLibraryServer** via a `cxx` FFI bridge. This eliminates the external sidecar process and provides direct access to Qt signals for event-driven sync with sub-100ms latency.

**Why Rust FFI fork instead of a sidecar?**

| Factor | Sidecar (Python) | Fork (Rust FFI) |
|--------|-------------------|-----------------|
| Deployment | 3 processes (Stump + YACReader + sidecar) | 2 processes (Stump + YACReader-fork) |
| YAC → Stump latency | 0–5s (PRAGMA polling) or <100ms (webhook fork) | <100ms (direct signal connection) |
| Change detection | External polling / filesystem watch | Internal Qt signals — zero overhead |
| Docker complexity | 3-container compose | 2-container compose |
| Build complexity | pip install | Rust toolchain + corrosion CMake module |
| YACReader code changes | 0 (or ~10 lines for webhook) | 3 files modified |

The fork approach is preferred because YACReaderLibraryServer already emits Qt signals (`comicUpdated`, `clientSync`) on every progress update — they just have no receivers in the headless server. Connecting them to Rust FFI calls is the most natural integration point.

**Architecture diagram:**

```
┌─────────────┐
│  iPhone      │
│  YACReader   │──── POST /v2/sync ────┐
│  iOS App     │                        │
└──────────────┘                        ▼
┌───────────────────────────────────────────────────────┐
│  YACReaderLibraryServer (forked)                       │
│                                                        │
│  ┌─────────────────────┐    ┌────────────────────────┐│
│  │ Qt HTTP Server       │    │ Rust stump_sync module ││
│  │                      │    │                        ││
│  │ RequestMapper        │    │ ┌────────────────────┐ ││
│  │   emit comicUpdated ─┼────┼→│ cxx FFI bridge     │ ││
│  │   emit clientSync   ─┼────┼→│                    │ ││
│  │                      │    │ │ rust_sync_push_*() │ ││
│  │ library.ydb ◄────────┼────┼─│ on_stump_progress  │ ││
│  │ (SQLite)             │    │ └────────┬───────────┘ ││
│  └─────────────────────┘    │          │              ││
│                              │ ┌────────▼───────────┐ ││
│                              │ │ Tokio runtime       │ ││
│                              │ │ • GraphQL client    │ ││
│                              │ │ • WebSocket client  │ ││
│                              │ │ • Sync engine       │ ││
│                              │ │ • ID mapping DB     │ ││
│                              │ └────────┬───────────┘ ││
│                              └──────────┼─────────────┘│
└─────────────────────────────────────────┼──────────────┘
                                          │ GraphQL / WS
                                          ▼
                               ┌─────────────────────┐
                               │  Stump               │
                               │  GraphQL :10801      │
                               └─────────────────────┘
```

**Only 3 files in YACReaderLibraryServer need modification:**

1. `YACReaderLibraryServer/CMakeLists.txt` — add corrosion + stump_sync import + link
2. `YACReaderLibraryServer/main.cpp` — add init + signal connections + shutdown (~15 lines)
3. New: `stump_sync/` directory — the entire Rust crate (new code, no existing file changes)

### 9.2 Rust Sync Module Design

The `stump_sync` crate is a self-contained Rust module that handles all communication with Stump. It runs its own async runtime (Tokio) and communicates with the C++ host via a narrow FFI bridge.

**Components:**

| Component | Responsibility | Key Dependencies |
|-----------|---------------|-----------------|
| **GraphQL client** | Query and mutate Stump reading progress | `reqwest`, `serde_json` |
| **WebSocket client** | Subscribe to Stump `readEvents` for library structure changes | `tokio-tungstenite`, `serde_json` |
| **ID mapping database** | Map YACReader comic IDs ↔ Stump media UUIDs via relative paths | `rusqlite` |
| **Sync engine** | Compare progress, resolve conflicts, decide write direction | Pure Rust logic |
| **Configuration** | Stump URL, API key, user ID, sync interval, mapping DB path | `serde`, TOML/YAML |

**Data flow for YACReader → Stump (Phase 2):**

```
iPhone syncs → RequestMapper::comicUpdated(libraryId, comicId)
  → Qt signal → C++ slot calls rust_sync_push_progress(libraryId, comicId)
    → posts PushProgress to Tokio channel
      → Tokio task: read comic progress from .ydb (rusqlite, read-only)
        → look up Stump media ID in mapping DB
          → GraphQL mutation: updateMediaProgress(id, {paged: {page}})
```

**Data flow for Stump → YACReader (Phase 3):**

```
Stump progress changes (polling or WebSocket event)
  → Tokio task: query Stump GraphQL for updated progress
    → compare with last-known YACReader state in mapping DB
      → if Stump is ahead: call C++ callback on_stump_progress_update()
        → QMetaObject::invokeMethod(Qt::QueuedConnection)
          → Qt thread: DBHelper::updateComicProgress() writes to .ydb
```

### 9.3 FFI Boundary Design

The FFI boundary uses the `cxx` crate for type-safe bidirectional communication. The bridge definition is intentionally minimal — only types and functions that must cross the Rust/C++ boundary are declared here. All internal logic stays in pure Rust.

```rust
// stump_sync/src/lib.rs

#[cxx::bridge(namespace = "stump_sync")]
mod ffi {
    // Shared types — visible to both Rust and C++
    struct SyncConfig {
        stump_url: String,
        api_key: String,
        user_id: String,
        sync_interval_secs: u32,
        mapping_db_path: String,
        ydb_paths: Vec<String>,       // paths to YACReader .ydb files
        library_roots: Vec<String>,   // corresponding filesystem roots
    }

    struct ProgressUpdate {
        comic_info_id: i64,
        stump_media_id: String,
        current_page: i32,
        is_read: bool,
        last_opened: i64,            // epoch seconds
    }

    struct SyncResult {
        success: bool,
        error_message: String,        // empty if success (no Option<T> in cxx)
    }

    struct SyncStatus {
        is_running: bool,
        last_sync_epoch: i64,         // 0 if never synced
        pending_updates: u32,
        error_message: String,        // empty if no error
    }

    // Rust functions exposed to C++
    extern "Rust" {
        fn rust_sync_init(config: &SyncConfig) -> SyncResult;
        fn rust_sync_push_progress(library_id: i64, comic_id: i64);
        fn rust_sync_push_all();
        fn rust_sync_pull_all();
        fn rust_sync_shutdown() -> SyncResult;
        fn rust_sync_status() -> SyncStatus;
    }

    // C++ functions that Rust can call (callbacks)
    unsafe extern "C++" {
        include!("stump_sync_callbacks.h");

        fn on_stump_progress_update(
            comic_info_id: i64,
            page: i32,
            is_read: bool,
            last_opened: i64,
        );
    }
}
```

**Type mapping notes:**

| Rust type | C++ type (via cxx) | Notes |
|-----------|-------------------|-------|
| `String` | `rust::String` | Convert to `QString` via `QString::fromUtf8(s.data(), s.size())` |
| `Vec<String>` | `rust::Vec<rust::String>` | Iterable from C++ |
| `i64`, `i32`, `u32` | `int64_t`, `int32_t`, `uint32_t` | Direct mapping |
| `bool` | `bool` | Direct mapping |
| `&SyncConfig` | `const SyncConfig&` | Shared struct, passed by reference |

### 9.4 Tokio Runtime Integration

The Rust module runs a Tokio multi-threaded runtime alongside Qt's event loop. The runtime is initialized once via `OnceLock` and communicates with the FFI bridge through an `mpsc` channel.

```rust
// stump_sync/src/lib.rs (runtime management)

use std::sync::OnceLock;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static CMD_TX: OnceLock<mpsc::UnboundedSender<SyncCommand>> = OnceLock::new();

enum SyncCommand {
    PushProgress { library_id: i64, comic_id: i64 },
    PushAll,
    PullAll,
    FullSync,
    Shutdown(tokio::sync::oneshot::Sender<()>),
}

fn rust_sync_init(config: &ffi::SyncConfig) -> ffi::SyncResult {
    let rt = RUNTIME.get_or_init(|| {
        Runtime::new().expect("failed to create tokio runtime")
    });

    let (tx, rx) = mpsc::unbounded_channel::<SyncCommand>();
    CMD_TX.set(tx).expect("rust_sync_init called twice");

    let config = config.clone().into(); // convert to internal Config type

    rt.spawn(async move {
        sync_engine::run(rx, config).await;
    });

    ffi::SyncResult { success: true, error_message: String::new() }
}

fn rust_sync_push_progress(library_id: i64, comic_id: i64) {
    if let Some(tx) = CMD_TX.get() {
        let _ = tx.send(SyncCommand::PushProgress { library_id, comic_id });
    }
}

fn rust_sync_push_all() {
    if let Some(tx) = CMD_TX.get() {
        let _ = tx.send(SyncCommand::PushAll);
    }
}

fn rust_sync_pull_all() {
    if let Some(tx) = CMD_TX.get() {
        let _ = tx.send(SyncCommand::PullAll);
    }
}

fn rust_sync_shutdown() -> ffi::SyncResult {
    if let Some(tx) = CMD_TX.get() {
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let _ = tx.send(SyncCommand::Shutdown(done_tx));

        if let Some(rt) = RUNTIME.get() {
            // Block briefly to allow graceful shutdown (max 5 seconds)
            let _ = rt.block_on(async {
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    done_rx,
                ).await
            });
        }
    }
    ffi::SyncResult { success: true, error_message: String::new() }
}
```

**Thread safety guarantees:**

- `OnceLock` ensures the runtime and channel sender are initialized exactly once.
- `mpsc::UnboundedSender` is `Send + Sync` — safe to call from the Qt main thread.
- `send()` is non-blocking — returns immediately, never stalls the Qt event loop.
- The Tokio runtime's thread pool is independent of Qt threads.

### 9.5 Qt Signal Integration

YACReaderLibraryServer already emits two Qt signals on progress updates that have **no receivers** in the headless server. The fork connects them to Rust FFI calls.

**Existing signals (already emitted, no code changes needed):**

| Signal | Emitted From | When |
|--------|-------------|------|
| `YACReaderHttpServer::comicUpdated(qulonglong libraryId, qulonglong comicId)` | `requestmapper.cpp:153` | Single comic progress update from iOS |
| `YACReaderHttpServer::clientSync()` | `requestmapper.cpp:131` | Batch sync from iOS (`POST /v2/sync`) |

**Modifications to `main.cpp`:**

```cpp
// YACReaderLibraryServer/main.cpp — additions for Stump sync
// (lines reference the existing start() function)

#include "stump_sync/src/lib.rs.h"  // cxx-generated header

// --- In start(), after LibrariesUpdateCoordinator init (line 251) ---

// Initialize Rust sync module
stump_sync::SyncConfig syncConfig;
syncConfig.stump_url = "http://localhost:10801";  // TODO: read from config file
syncConfig.api_key = "your-api-key";
syncConfig.user_id = "stump-user-uuid";
syncConfig.sync_interval_secs = 300;
syncConfig.mapping_db_path = "/data/sync/mapping.db";
// ... populate ydb_paths and library_roots from library list ...

auto initResult = stump_sync::rust_sync_init(syncConfig);
if (!initResult.success) {
    qWarning() << "Stump sync init failed:"
               << QString::fromUtf8(initResult.error_message.data(),
                                     initResult.error_message.size());
}

// Connect existing signals to Rust sync
QObject::connect(
    httpServer, &YACReaderHttpServer::comicUpdated,
    [](qulonglong libraryId, qulonglong comicId) {
        stump_sync::rust_sync_push_progress(
            static_cast<int64_t>(libraryId),
            static_cast<int64_t>(comicId)
        );
    }
);

QObject::connect(
    httpServer, &YACReaderHttpServer::clientSync,
    []() {
        stump_sync::rust_sync_push_all();
    }
);

// --- Before app.exec() (line 253) --- no changes needed

// --- In shutdown section (lines 258–265), before return ---
stump_sync::rust_sync_shutdown();
```

**Callback from Rust → C++ (Stump → YACReader direction):**

```cpp
// stump_sync_callbacks.h — C++ function callable from Rust

#include <QMetaObject>
#include <QCoreApplication>

// Called from a Tokio worker thread — must marshal to Qt thread
void on_stump_progress_update(
    int64_t comic_info_id, int32_t page,
    bool is_read, int64_t last_opened
) {
    QMetaObject::invokeMethod(
        QCoreApplication::instance(),
        [=]() {
            // Now on the Qt main thread — safe to use DBHelper
            DBHelper::updateComicProgress(comic_info_id, page, is_read, last_opened);
        },
        Qt::QueuedConnection
    );
}
```

### 9.6 CMake Integration

The Rust crate is integrated into YACReaderLibraryServer's CMake build using the `corrosion` module, which bridges Cargo and CMake.

**Full CMake additions for `YACReaderLibraryServer/CMakeLists.txt`:**

```cmake
# --- Rust stump_sync integration ---

# Fetch corrosion (Rust-CMake bridge)
include(FetchContent)
FetchContent_Declare(
    Corrosion
    GIT_REPOSITORY https://github.com/corrosion-rs/corrosion.git
    GIT_TAG v0.5.1  # pin to specific release
)
FetchContent_MakeAvailable(Corrosion)

# Import the Rust crate as a CMake target
corrosion_import_crate(
    MANIFEST_PATH ${CMAKE_SOURCE_DIR}/stump_sync/Cargo.toml
)

# Link the Rust static library into the server executable
target_link_libraries(YACReaderLibraryServer
    PRIVATE
    stump-sync  # target name from Cargo.toml [package].name
)

# Platform-specific system libraries required by Rust dependencies
if(UNIX AND NOT APPLE)
    target_link_libraries(YACReaderLibraryServer PRIVATE pthread dl m)
elseif(APPLE)
    target_link_libraries(YACReaderLibraryServer PRIVATE
        "-framework Security"
        "-framework SystemConfiguration"
    )
endif()

# Include path for cxx-generated headers
target_include_directories(YACReaderLibraryServer
    PRIVATE
    ${CMAKE_BINARY_DIR}/corrosion_generated/cxxbridge/stump-sync/src/
)
```

**Updated dependency chain (standalone mode):**

```
YACReaderLibraryServer (executable)
├── library_common (STATIC)
├── db_helper (STATIC)
├── server (STATIC) — REST API handlers
├── common_all (STATIC)
├── comic_backend (STATIC)
├── cbx_backend (STATIC)
├── naturalsort, yr_global (STATIC)
├── QsLog, QrCode, QtWebApp_httpserver (STATIC)
└── stump-sync (STATIC IMPORTED — Rust via corrosion)
    ├── reqwest (HTTP client)
    ├── tokio (async runtime)
    ├── rusqlite (SQLite for ID mapping)
    ├── serde + serde_json (serialization)
    ├── chrono (timestamps)
    └── cxx (FFI bridge)
```

### 9.7 Stump API Client

The Rust module implements its own lightweight Stump client using `reqwest` + hand-written GraphQL queries. This avoids depending on Stump's internal crates (which would pull in SeaORM + async-graphql + 100+ transitive dependencies).

**Authentication:**

```rust
// stump_sync/src/stump_client.rs

use reqwest::Client;

pub struct StumpClient {
    client: Client,
    base_url: String,
    api_key: String,
}

impl StumpClient {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        let client = Client::builder()
            .default_headers({
                let mut h = reqwest::header::HeaderMap::new();
                h.insert("Authorization", format!("Bearer {}", api_key).parse().unwrap());
                h
            })
            .build()
            .expect("failed to build HTTP client");

        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
        }
    }
}
```

**GraphQL queries:**

```rust
impl StumpClient {
    // Fetch all media with progress for a specific library
    pub async fn get_library_progress(&self, library_id: &str)
        -> Result<Vec<MediaProgress>, SyncError>
    {
        let query = r#"
            query GetLibraryMedia($id: String!) {
                library(id: $id) {
                    series {
                        media {
                            id
                            name
                            pages
                            path
                            size
                            readProgress {
                                page
                                percentageCompleted
                                updatedAt
                            }
                            readHistory {
                                completedAt
                            }
                        }
                    }
                }
            }
        "#;

        let resp = self.client
            .post(format!("{}/api/graphql", self.base_url))
            .json(&serde_json::json!({
                "query": query,
                "variables": { "id": library_id }
            }))
            .send()
            .await?;

        let body: GraphQLResponse<LibraryData> = resp.json().await?;
        // ... extract and flatten media list ...
        Ok(media_progress)
    }

    // Push a page progress update
    pub async fn update_progress(&self, media_id: &str, page: i32)
        -> Result<(), SyncError>
    {
        let mutation = r#"
            mutation UpdateProgress($id: String!, $page: Int!) {
                updateMediaProgress(id: $id, input: { paged: { page: $page } }) {
                    ... on ActiveReadingSession { page updatedAt }
                    ... on FinishedReadingSession { completedAt }
                }
            }
        "#;

        self.client
            .post(format!("{}/api/graphql", self.base_url))
            .json(&serde_json::json!({
                "query": mutation,
                "variables": { "id": media_id, "page": page }
            }))
            .send()
            .await?
            .error_for_status()?;

        Ok(())
    }

    // Mark a media item as complete
    pub async fn mark_complete(&self, media_id: &str, is_complete: bool)
        -> Result<(), SyncError>
    {
        let mutation = r#"
            mutation MarkComplete($id: String!, $isComplete: Boolean!) {
                markMediaAsComplete(id: $id, isComplete: $isComplete)
            }
        "#;

        self.client
            .post(format!("{}/api/graphql", self.base_url))
            .json(&serde_json::json!({
                "query": mutation,
                "variables": { "id": media_id, "isComplete": is_complete }
            }))
            .send()
            .await?
            .error_for_status()?;

        Ok(())
    }
}
```

**Error handling and retry logic:**

```rust
impl StumpClient {
    async fn graphql_request_with_retry<T: serde::de::DeserializeOwned>(
        &self, query: &str, variables: serde_json::Value,
    ) -> Result<T, SyncError> {
        let mut attempts = 0;
        let max_retries = 3;
        let mut delay = std::time::Duration::from_secs(1);

        loop {
            match self.execute_graphql(query, &variables).await {
                Ok(data) => return Ok(data),
                Err(e) if e.is_retryable() && attempts < max_retries => {
                    attempts += 1;
                    tracing::warn!(attempt = attempts, error = %e, "retrying GraphQL request");
                    tokio::time::sleep(delay).await;
                    delay *= 2; // exponential backoff
                }
                Err(e) => return Err(e),
            }
        }
    }
}
```

**Optional WebSocket subscription for library events:**

```rust
// stump_sync/src/stump_client.rs — WebSocket subscription

use tokio_tungstenite::{connect_async, tungstenite::Message};

impl StumpClient {
    pub async fn subscribe_events(&self, tx: mpsc::UnboundedSender<StumpEvent>) {
        let ws_url = format!(
            "{}/api/graphql/ws",
            self.base_url.replace("http", "ws")
        );

        loop {
            match connect_async(&ws_url).await {
                Ok((mut ws, _)) => {
                    // graphql-ws protocol init
                    let _ = ws.send(Message::Text(
                        r#"{"type":"connection_init"}"#.into()
                    )).await;

                    // Subscribe to readEvents
                    let _ = ws.send(Message::Text(serde_json::json!({
                        "id": "1",
                        "type": "subscribe",
                        "payload": {
                            "query": "subscription { readEvents { __typename } }"
                        }
                    }).to_string().into())).await;

                    while let Some(Ok(msg)) = ws.next().await {
                        if let Message::Text(text) = msg {
                            if let Ok(event) = serde_json::from_str::<WsMessage>(&text) {
                                if event.r#type == "next" {
                                    let _ = tx.send(StumpEvent::from(event));
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "WebSocket connection failed, retrying in 10s");
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                }
            }
        }
    }
}
```

### 9.8 ID Mapping Database

The Rust module maintains its own SQLite database for mapping YACReader comic IDs to Stump media UUIDs. This database is separate from both YACReader's `.ydb` and Stump's database.

**Schema:**

```sql
-- Stored at the path specified in SyncConfig.mapping_db_path
-- e.g., /data/sync/stump_mapping.db

CREATE TABLE library_mapping (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    yac_library_id     TEXT NOT NULL,      -- YACReader library UUID (from .yacreaderlibrary/id)
    yac_library_name   TEXT,
    stump_library_id   TEXT NOT NULL,      -- Stump library UUID
    stump_library_name TEXT,
    yac_library_root   TEXT NOT NULL,      -- YACReader library path
    stump_library_root TEXT NOT NULL,      -- Stump library path (for path prefix stripping)
    UNIQUE(yac_library_id, stump_library_id)
);

CREATE TABLE media_mapping (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    library_mapping_id  INTEGER NOT NULL REFERENCES library_mapping(id),
    yac_comic_info_id   INTEGER NOT NULL,  -- comic_info.id in YACReader
    yac_comic_id        INTEGER NOT NULL,  -- comic.id in YACReader
    stump_media_id      TEXT NOT NULL,     -- media.id (UUID) in Stump
    relative_path       TEXT NOT NULL,     -- Normalized relative path from library root
    filename            TEXT NOT NULL,
    matched_via         TEXT NOT NULL,     -- "path" | "filename_size" | "manual"
    matched_at          TEXT NOT NULL,     -- ISO 8601 timestamp
    UNIQUE(yac_comic_info_id, stump_media_id)
);

CREATE TABLE sync_state (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    media_mapping_id    INTEGER NOT NULL UNIQUE REFERENCES media_mapping(id),
    yac_current_page    INTEGER DEFAULT 0,
    stump_current_page  INTEGER DEFAULT 0,
    yac_read            INTEGER DEFAULT 0, -- boolean
    stump_complete      INTEGER DEFAULT 0, -- boolean
    yac_last_modified   INTEGER DEFAULT 0, -- epoch seconds
    stump_last_modified TEXT,              -- ISO 8601
    last_synced_at      TEXT NOT NULL      -- ISO 8601
);
```

**Population strategy:**

1. **First sync**: The Rust module reads all media from Stump (via GraphQL) and all comics from YACReader (via read-only SQLite on `.ydb`). It matches them using the hybrid strategy from §5.1:
   - **Primary**: Normalize both paths to relative form and compare.
   - **Fallback**: Match by filename + file size for moved files.
2. **Incremental updates**: When Stump emits `CreatedMedia` or `CreatedOrUpdatedManyMedia` events (via WebSocket), or when a new comic appears in YACReader, the module matches only the new entries.
3. **Manual overrides**: The `matched_via = "manual"` type supports config-file-driven mappings for edge cases that automated matching cannot resolve.

**Rust mapping database module:**

```rust
// stump_sync/src/mapping_db.rs

use rusqlite::Connection;

pub struct MappingDb {
    conn: Connection,
}

impl MappingDb {
    pub fn open(path: &str) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
        conn.execute_batch(include_str!("schema.sql"))?;
        Ok(Self { conn })
    }

    pub fn get_stump_id(&self, yac_comic_info_id: i64) -> Option<String> {
        self.conn.query_row(
            "SELECT stump_media_id FROM media_mapping WHERE yac_comic_info_id = ?1",
            [yac_comic_info_id],
            |row| row.get(0),
        ).ok()
    }

    pub fn get_yac_id(&self, stump_media_id: &str) -> Option<i64> {
        self.conn.query_row(
            "SELECT yac_comic_info_id FROM media_mapping WHERE stump_media_id = ?1",
            [stump_media_id],
            |row| row.get(0),
        ).ok()
    }

    pub fn insert_mapping(
        &self,
        library_mapping_id: i64,
        yac_comic_info_id: i64,
        yac_comic_id: i64,
        stump_media_id: &str,
        relative_path: &str,
        filename: &str,
        matched_via: &str,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT OR IGNORE INTO media_mapping
             (library_mapping_id, yac_comic_info_id, yac_comic_id,
              stump_media_id, relative_path, filename, matched_via, matched_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'))",
            rusqlite::params![
                library_mapping_id, yac_comic_info_id, yac_comic_id,
                stump_media_id, relative_path, filename, matched_via
            ],
        )?;
        Ok(())
    }
}
```

### 9.9 Module File Structure

```
stump_sync/
├── Cargo.toml               — Package definition + dependencies
├── Cargo.lock               — Locked dependency versions (committed)
├── build.rs                 — cxx build script (gated behind ffi feature)
├── src/
│   ├── lib.rs               — cxx bridge definition (behind ffi feature) + FFI entry points
│   ├── sync_engine.rs       — Core sync loop: path matching, delta computation, push
│   ├── stump_client.rs      — GraphQL/HTTP client with retry logic
│   ├── mapping_db.rs        — Mutex-wrapped rusqlite ID mapping database
│   ├── runtime.rs           — Tokio runtime management + mpsc command channel
│   ├── config.rs            — Configuration types (Config, LibraryConfig)
│   ├── types.rs             — Internal data types (ComicProgress, StumpMedia, SyncDelta, SyncError)
│   └── schema.sql           — Mapping DB schema (included via include_str!)
└── tests/
    ├── common/
    │   └── mod.rs            — Test fixtures (temp .ydb creation, mock Stump server)
    ├── test_mapping.rs       — Integration tests for mapping DB lifecycle
    └── test_sync_flow.rs     — E2E tests: full sync flow with wiremock mock server
```

**`Cargo.toml`** (as implemented):

```toml
[package]
name = "stump-sync"
version = "0.1.0"
edition = "2021"

[features]
default = []
ffi = ["dep:cxx"]

[dependencies]
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["rt-multi-thread", "sync", "macros", "time"] }
rusqlite = { version = "0.31", features = ["bundled"] }
chrono = { version = "0.4", features = ["serde"] }
tracing = "0.1"
cxx = { version = "1", optional = true }

[dev-dependencies]
tempfile = "3"
wiremock = "0.6"
tokio = { version = "1", features = ["test-util"] }

[build-dependencies]
cxx-build = "1"

[lib]
crate-type = ["staticlib", "lib"]
```

**`build.rs`** (as implemented):

```rust
fn main() {
    if std::env::var("CARGO_FEATURE_FFI").is_ok() {
        cxx_build::bridge("src/lib.rs")
            .flag_if_supported("-std=c++17")
            .compile("stump_sync_cxx");
    }
}
```

**CMake integration** (in top-level `CMakeLists.txt`):
```cmake
corrosion_import_crate(MANIFEST_PATH ${CMAKE_SOURCE_DIR}/stump_sync/Cargo.toml FEATURES ffi)
```

---

## Appendix A: YACReader Database Schema

```sql
CREATE TABLE comic_info (
    id            INTEGER PRIMARY KEY,
    hash          TEXT UNIQUE NOT NULL,  -- SHA1(first 512KB) + filesize
    title         TEXT,
    numPages      INTEGER,
    currentPage   INTEGER DEFAULT 1,
    hasBeenOpened INTEGER DEFAULT 0,     -- BOOL
    read          INTEGER DEFAULT 0,     -- BOOL
    lastTimeOpened INTEGER,              -- Epoch seconds
    rating        REAL DEFAULT 0,
    bookmark1     INTEGER DEFAULT -1,
    bookmark2     INTEGER DEFAULT -1,
    bookmark3     INTEGER DEFAULT -1,
    -- Additional metadata fields omitted for brevity:
    -- coverPage, comicVineID, volume, number, series, publisher,
    -- writer, penciller, colorist, genre, synopsis, etc.
);

CREATE TABLE comic (
    id          INTEGER PRIMARY KEY,
    parentId    INTEGER NOT NULL REFERENCES folder(id),
    comicInfoId INTEGER NOT NULL REFERENCES comic_info(id),
    fileName    TEXT NOT NULL,
    path        TEXT               -- Relative path from library root (directory only)
);

CREATE TABLE folder (
    id          INTEGER PRIMARY KEY,
    parentId    INTEGER REFERENCES folder(id),
    name        TEXT NOT NULL,
    path        TEXT NOT NULL,      -- Relative path from library root
    -- macOSXDisplayName, firstChildHash, customImage, etc.
);

-- Libraries are NOT stored in the database.
-- Each library is a filesystem path with its own .ydb file.
-- Library UUID is in <library_path>/.yacreaderlibrary/id
-- Library list is managed by YACReaderLibraryServer config.
```

## Appendix B: Stump Database Schema

```sql
-- Core tables (SeaORM-managed, SQLite WAL mode)

CREATE TABLE libraries (
    id              TEXT PRIMARY KEY,    -- UUID
    name            TEXT NOT NULL,
    path            TEXT NOT NULL UNIQUE,
    config_id       TEXT REFERENCES library_configs(id),
    last_scanned_at TEXT                 -- ISO 8601
);

CREATE TABLE library_configs (
    id                   TEXT PRIMARY KEY,
    generate_file_hashes INTEGER DEFAULT 0  -- BOOL
    -- Additional config fields: thumbnail_config, etc.
);

CREATE TABLE series (
    id          TEXT PRIMARY KEY,         -- UUID
    name        TEXT NOT NULL,
    path        TEXT NOT NULL,            -- Absolute path
    library_id  TEXT NOT NULL REFERENCES libraries(id)
);

CREATE TABLE media (
    id          TEXT PRIMARY KEY,         -- UUID
    name        TEXT NOT NULL,
    size        INTEGER NOT NULL,         -- Bytes (BIGINT)
    extension   TEXT NOT NULL,
    pages       INTEGER NOT NULL,
    path        TEXT NOT NULL,            -- Absolute path
    hash        TEXT,                     -- Optional SHA256 (sample-based)
    koreader_hash TEXT,                   -- Optional MD5
    series_id   TEXT NOT NULL REFERENCES series(id),
    deleted_at  TEXT                      -- Soft delete, ISO 8601
);

CREATE TABLE users (
    id              TEXT PRIMARY KEY,     -- UUID
    username        TEXT NOT NULL UNIQUE,
    hashed_password TEXT NOT NULL,
    is_server_owner INTEGER DEFAULT 0,
    permissions     TEXT                  -- JSON
);

CREATE TABLE reading_sessions (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    page                  INTEGER NOT NULL,
    percentage_completed  REAL,           -- 0.0 to 1.0
    started_at            TEXT NOT NULL,  -- ISO 8601
    updated_at            TEXT NOT NULL,  -- ISO 8601
    media_id              TEXT NOT NULL REFERENCES media(id),
    user_id               TEXT NOT NULL REFERENCES users(id),
    device_id             TEXT,
    elapsed_seconds       INTEGER DEFAULT 0,
    UNIQUE(media_id, user_id)
);

CREATE TABLE finished_reading_sessions (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at      TEXT,                -- ISO 8601
    completed_at    TEXT NOT NULL,       -- ISO 8601
    media_id        TEXT NOT NULL REFERENCES media(id),
    user_id         TEXT NOT NULL REFERENCES users(id),
    device_id       TEXT,
    elapsed_seconds INTEGER DEFAULT 0
    -- Multiple rows per (media_id, user_id) allowed (re-reads)
);
```

## Appendix C: API Endpoint Reference

### Side-by-Side Comparison

| Operation | YACReader Endpoint | Stump Endpoint |
|-----------|-------------------|----------------|
| **List libraries** | `GET /v2/libraries` | GraphQL: `query { libraries { ... } }` |
| **List comics/media** | `GET /v2/library/{id}/folder/{fid}/content` (recursive) | GraphQL: `query { libraries { series { media { ... } } } }` |
| **Get comic details** | `GET /v2/library/{id}/comic/{cid}/fullinfo` | GraphQL: `query { media(id) { ... readProgress readHistory } }` |
| **Get page image** | `GET /v2/library/{id}/comic/{cid}/page/{p}/remote` | `GET /api/v2/media/{id}/page/{p}` |
| **Batch sync progress** | `POST /v2/sync` (tab-separated body) | *(no batch endpoint — individual mutations)* |
| **Update single progress** | `POST /v2/library/{id}/comic/{cid}/update` | GraphQL: `mutation { updateMediaProgress(id, input) }` |
| **Mark complete** | Set `read=1` via sync endpoint | GraphQL: `mutation { markMediaAsComplete(id, isComplete) }` |
| **Authentication** | None (no auth mechanism) | API key header (recommended), session cookie, JWT, OIDC |
| **Download file** | *(not available)* | `GET /api/v2/media/{id}/file` |

### YACReader Batch Sync Format

```
POST /v2/sync
Content-Type: text/plain

{libraryId}\t{comicId}\t{hash}\t{currentPage}\t{rating}\t{lastTimeOpened}\t{read}
{libraryId}\t{comicId}\t{hash}\t{currentPage}\t{rating}\t{lastTimeOpened}\t{read}
...
```

Response: JSON array of comics where the server has more recent state (the client should update these).

### Stump GraphQL Examples

```graphql
# Query progress for all media in a library
query {
  libraries {
    id
    name
    path
    series {
      media {
        id
        name
        pages
        path
        size
        readProgress {
          page
          percentageCompleted
          updatedAt
          elapsedSeconds
        }
        readHistory {
          completedAt
        }
      }
    }
  }
}

# Update reading progress
mutation {
  updateMediaProgress(
    id: "550e8400-e29b-41d4-a716-446655440000"
    input: { paged: { page: 42 } }
  ) {
    ... on ActiveReadingSession {
      page
      updatedAt
    }
    ... on FinishedReadingSession {
      completedAt
    }
  }
}

# Mark as complete
mutation {
  markMediaAsComplete(
    id: "550e8400-e29b-41d4-a716-446655440000"
    isComplete: true
  )
}
```
