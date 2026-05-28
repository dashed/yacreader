CREATE TABLE IF NOT EXISTS library_mapping (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    yac_library_path TEXT NOT NULL,
    stump_library_id TEXT NOT NULL,
    stump_library_path TEXT NOT NULL,
    UNIQUE(yac_library_path, stump_library_id)
);

CREATE TABLE IF NOT EXISTS media_mapping (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    library_mapping_id INTEGER NOT NULL REFERENCES library_mapping(id),
    yac_comic_info_id INTEGER NOT NULL,
    yac_comic_id INTEGER NOT NULL,
    stump_media_id TEXT NOT NULL,
    relative_path TEXT NOT NULL,
    filename TEXT NOT NULL,
    matched_via TEXT NOT NULL DEFAULT 'path',
    matched_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(library_mapping_id, yac_comic_info_id, stump_media_id)
);

CREATE TABLE IF NOT EXISTS sync_state (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    media_mapping_id INTEGER NOT NULL UNIQUE REFERENCES media_mapping(id),
    yac_current_page INTEGER NOT NULL DEFAULT 0,
    stump_current_page INTEGER NOT NULL DEFAULT 0,
    yac_read INTEGER NOT NULL DEFAULT 0,
    stump_complete INTEGER NOT NULL DEFAULT 0,
    yac_last_modified TEXT,
    stump_last_modified TEXT,
    last_synced_at TEXT NOT NULL DEFAULT (datetime('now'))
);
