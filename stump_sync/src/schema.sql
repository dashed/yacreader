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
    -- M4: the UNIQUE key is (library_mapping_id, yac_comic_info_id) and does
    -- NOT include the media id. A YAC comic maps to exactly one Stump media per
    -- library; if Stump re-scans and the media id changes, insert_mapping
    -- UPDATEs this row in place (ON CONFLICT DO UPDATE) rather than inserting a
    -- duplicate. The previous key additionally keyed on the media id, which made
    -- remapping impossible and left get_stump_id nondeterministic. The mapping
    -- DB is a derived cache, so a mapping.db built with the old key is rebuilt
    -- automatically by MappingDb::open on the user_version bump.
    UNIQUE(library_mapping_id, yac_comic_info_id)
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
