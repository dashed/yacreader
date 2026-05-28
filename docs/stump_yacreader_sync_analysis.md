# Stump ↔ YACReaderLibraryServer Sync Analysis

## 1. Executive Summary

This document analyzes the feasibility and architecture for synchronizing reading progress between **Stump** (a self-hosted comic/book server) and **YACReaderLibraryServer** (the server component of YACReader). The primary use case is reading comics on an iPhone via YACReader iOS, which syncs progress to YACReaderLibraryServer, then propagating that progress to Stump — and eventually in both directions.

Both servers scan the same comic library on a shared filesystem. Stump serves as the source of truth for library organization and metadata, while YACReaderLibraryServer provides the backend for YACReader iOS. The core challenge is that **the two systems use incompatible content-hashing algorithms** (YACReader: SHA1 of first 512 KB + filesize; Stump: SHA256 of four 10 KB samples), meaning content-based identity matching is not directly possible. The recommended approach uses **relative-path matching** as the primary identity strategy, since both servers index the same directory tree.

The recommended architecture is a **Python sidecar service** that polls both systems, maintains an ID-mapping database, and writes progress updates through each system's existing API. Implementation proceeds in four phases: shared filesystem (no code), one-way sync (YACReader → Stump), two-way sync, and finally an optional embedded integration in a YACReaderLibraryServer fork.

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

### Phase 1: Shared Filesystem (No Code Changes)

**Goal**: Both servers index the same comic library.

**Steps**:
1. Set up the shared filesystem (`/srv/comics` or equivalent).
2. Deploy Stump and YACReaderLibraryServer, both scanning the shared directory.
3. Configure YACReader iOS to connect to YACReaderLibraryServer.
4. Verify both servers see the same comics.

**Outcome**: Reading works on both systems independently, but progress is not synced. This is the foundation for all subsequent phases.

**Effort**: ~1 hour of setup.

### Phase 2: One-Way Progress Sync (YACReader → Stump)

**Goal**: Reading progress from YACReader iOS appears in Stump.

**Steps**:
1. Build the sync sidecar (Python) with:
   - YACReader `.ydb` reader (SQLite, read-only)
   - Stump GraphQL client (write progress)
   - Path-based media matching
   - Mapping DB (`mapping.db`)
2. Implement the sync cycle (read YAC → match → write Stump).
3. Deploy as a Docker container or systemd service.
4. Test: read a comic on iPhone, verify progress appears in Stump.

**Outcome**: Stump reflects YACReader reading progress. Stump-originated progress is not synced back.

**Effort**: ~2–3 days of development.

### Phase 3: Two-Way Progress Sync

**Goal**: Progress flows in both directions. Reading on any client updates both systems.

**Steps**:
1. Add Stump → YACReader sync to the sidecar:
   - Stump GraphQL reader (query progress)
   - YACReader HTTP API writer (`POST /v2/sync`)
2. Implement conflict resolution (§5.3).
3. Handle Stump completion events (finished sessions → YACReader `read` flag).
4. Test: read in Stump web reader, verify progress appears in YACReader iOS on next sync.

**Outcome**: Full bidirectional sync. Either system can be used for reading.

**Effort**: ~2–3 additional days.

### Phase 4: Embedded Sync in YACReaderLibraryServer Fork (Optional)

**Goal**: Eliminate the sidecar by embedding Stump sync directly into YACReaderLibraryServer.

**Steps**:
1. Fork YACReaderLibraryServer.
2. Add a C++/Qt module that:
   - Communicates with Stump's GraphQL API via `QNetworkAccessManager`
   - Runs sync on a configurable timer
   - Stores mapping state in the library `.ydb` (new table) or a separate DB
3. Add configuration UI or config file support for Stump connection details.
4. Merge sync logic from Phase 3 Python into C++.

**Outcome**: Single-server deployment. No sidecar needed.

**Effort**: ~1–2 weeks. Requires C++/Qt familiarity. Consider whether the maintenance overhead justifies eliminating a simple Python sidecar.

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
