use std::sync::Mutex;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::types::{MediaMapping, SyncError, SyncState};

pub struct MappingDb {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = include_str!("schema.sql");

/// Bumped whenever the derived-table layout changes in a way that
/// `CREATE TABLE IF NOT EXISTS` cannot migrate. v2 = M4's media_mapping
/// UNIQUE-key change (drop stump_media_id from the key).
const SCHEMA_VERSION: i64 = 2;

impl MappingDb {
    pub fn open(path: &str) -> Result<Self, SyncError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;

        // M4 migration: `CREATE TABLE IF NOT EXISTS` cannot change an existing
        // table's UNIQUE key, so an existing mapping.db keeps the obsolete
        // (library, yac_comic, stump_media) key under which a re-matched comic
        // inserts a DUPLICATE row. The mapping DB is a derived cache (rebuilt
        // from match_comics on the next sync), so when we detect the old key we
        // drop the derived tables and let the new schema recreate them. The
        // referencing table (sync_state) is dropped first to satisfy the FK;
        // its baselines are simply re-observed on the next sync (worst case:
        // one monotonic, regression-free cycle — never data loss).
        let legacy_media_key = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='media_mapping'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            // The v1 key was `UNIQUE(library_mapping_id, yac_comic_info_id,
            // stump_media_id)`; v2 drops the trailing column. Match the full
            // 3-column clause (sqlite_master stores the CREATE verbatim,
            // comments included, so a looser substring could false-match prose).
            .map(|sql| sql.contains("yac_comic_info_id, stump_media_id)"))
            .unwrap_or(false);
        if legacy_media_key {
            conn.execute_batch(
                "DROP TABLE IF EXISTS sync_state; DROP TABLE IF EXISTS media_mapping;",
            )?;
        }

        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, SyncError> {
        self.conn
            .lock()
            .map_err(|e| SyncError::Database(format!("lock poisoned: {e}")))
    }

    pub fn ensure_library_mapping(
        &self,
        yac_path: &str,
        stump_id: &str,
        stump_path: &str,
    ) -> Result<i64, SyncError> {
        let conn = self.lock_conn()?;
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM library_mapping WHERE yac_library_path = ?1 AND stump_library_id = ?2",
                params![yac_path, stump_id],
                |row| row.get(0),
            )
            .ok();

        if let Some(id) = existing {
            return Ok(id);
        }

        conn.execute(
            "INSERT INTO library_mapping (yac_library_path, stump_library_id, stump_library_path) VALUES (?1, ?2, ?3)",
            params![yac_path, stump_id, stump_path],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn get_stump_id(&self, yac_comic_info_id: i64) -> Result<Option<String>, SyncError> {
        let conn = self.lock_conn()?;
        // M4: with UNIQUE(library_mapping_id, yac_comic_info_id) there is at most
        // one row per (library, comic); the ORDER BY makes the result fully
        // deterministic even if the same comic_info_id appears across libraries
        // (most-recently-matched wins).
        let result = conn
            .query_row(
                "SELECT stump_media_id FROM media_mapping WHERE yac_comic_info_id = ?1 \
                 ORDER BY matched_at DESC, id DESC LIMIT 1",
                params![yac_comic_info_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(result)
    }

    pub fn get_yac_id(&self, stump_media_id: &str) -> Result<Option<i64>, SyncError> {
        let conn = self.lock_conn()?;
        let result = conn
            .query_row(
                "SELECT yac_comic_info_id FROM media_mapping WHERE stump_media_id = ?1 \
                 ORDER BY matched_at DESC, id DESC LIMIT 1",
                params![stump_media_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(result)
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
    ) -> Result<i64, SyncError> {
        let conn = self.lock_conn()?;
        // M4: UPSERT on the (library, comic) key so a re-matched comic whose
        // Stump media id changed UPDATEs its existing row instead of inserting a
        // duplicate (the old `INSERT OR IGNORE` left a stale row AND a new one,
        // making get_stump_id nondeterministic).
        conn.execute(
            "INSERT INTO media_mapping (library_mapping_id, yac_comic_info_id, yac_comic_id, stump_media_id, relative_path, filename, matched_via) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(library_mapping_id, yac_comic_info_id) DO UPDATE SET \
                stump_media_id=excluded.stump_media_id, \
                relative_path=excluded.relative_path, \
                filename=excluded.filename, \
                matched_via=excluded.matched_via, \
                matched_at=datetime('now')",
            params![library_mapping_id, yac_comic_info_id, yac_comic_id, stump_media_id, relative_path, filename, matched_via],
        )?;
        // last_insert_rowid() is unreliable on the DO UPDATE path, so read the
        // (now-unique) row's id back explicitly.
        let id: i64 = conn.query_row(
            "SELECT id FROM media_mapping WHERE library_mapping_id = ?1 AND yac_comic_info_id = ?2",
            params![library_mapping_id, yac_comic_info_id],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    pub fn get_sync_state(&self, media_mapping_id: i64) -> Result<Option<SyncState>, SyncError> {
        let conn = self.lock_conn()?;
        let result = conn
            .query_row(
                "SELECT id, media_mapping_id, yac_current_page, stump_current_page, yac_read, stump_complete, yac_last_modified, stump_last_modified, last_synced_at FROM sync_state WHERE media_mapping_id = ?1",
                params![media_mapping_id],
                |row| {
                    Ok(SyncState {
                        id: row.get(0)?,
                        media_mapping_id: row.get(1)?,
                        yac_current_page: row.get(2)?,
                        stump_current_page: row.get(3)?,
                        yac_read: row.get::<_, i32>(4)? != 0,
                        stump_complete: row.get::<_, i32>(5)? != 0,
                        yac_last_modified: row.get(6)?,
                        stump_last_modified: row.get(7)?,
                        last_synced_at: row.get(8)?,
                    })
                },
            )
            .optional()?;
        Ok(result)
    }

    pub fn update_sync_state(
        &self,
        media_mapping_id: i64,
        yac_page: i32,
        stump_page: i32,
        yac_read: bool,
        stump_complete: bool,
        yac_modified: Option<&str>,
        stump_modified: Option<&str>,
    ) -> Result<(), SyncError> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO sync_state (media_mapping_id, yac_current_page, stump_current_page, yac_read, stump_complete, yac_last_modified, stump_last_modified, last_synced_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'))
             ON CONFLICT(media_mapping_id) DO UPDATE SET yac_current_page=?2, stump_current_page=?3, yac_read=?4, stump_complete=?5, yac_last_modified=?6, stump_last_modified=?7, last_synced_at=datetime('now')",
            params![media_mapping_id, yac_page, stump_page, yac_read as i32, stump_complete as i32, yac_modified, stump_modified],
        )?;
        Ok(())
    }

    pub fn get_all_mappings(
        &self,
        library_mapping_id: i64,
    ) -> Result<Vec<MediaMapping>, SyncError> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, library_mapping_id, yac_comic_info_id, yac_comic_id, stump_media_id, relative_path, filename, matched_via FROM media_mapping WHERE library_mapping_id = ?1",
        )?;
        let rows = stmt.query_map(params![library_mapping_id], |row| {
            Ok(MediaMapping {
                id: row.get(0)?,
                library_mapping_id: row.get(1)?,
                yac_comic_info_id: row.get(2)?,
                yac_comic_id: row.get(3)?,
                stump_media_id: row.get(4)?,
                relative_path: row.get(5)?,
                filename: row.get(6)?,
                matched_via: row.get(7)?,
            })
        })?;
        let mut mappings = Vec::new();
        for row in rows {
            mappings.push(row?);
        }
        Ok(mappings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn temp_db() -> (MappingDb, NamedTempFile) {
        let tmp = NamedTempFile::new().unwrap();
        let db = MappingDb::open(tmp.path().to_str().unwrap()).unwrap();
        (db, tmp)
    }

    #[test]
    fn test_open_creates_tables() {
        let (db, _tmp) = temp_db();
        let conn = db.lock_conn().unwrap();
        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('library_mapping', 'media_mapping', 'sync_state')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn test_insert_and_lookup() {
        let (db, _tmp) = temp_db();
        let lib_id = db
            .ensure_library_mapping("/comics", "stump-lib-1", "/srv/comics")
            .unwrap();

        let mapping_id = db
            .insert_mapping(lib_id, 100, 200, "stump-media-1", "Marvel/001.cbz", "001.cbz", "path")
            .unwrap();
        assert!(mapping_id > 0);

        let stump_id = db.get_stump_id(100).unwrap();
        assert_eq!(stump_id, Some("stump-media-1".to_string()));

        let yac_id = db.get_yac_id("stump-media-1").unwrap();
        assert_eq!(yac_id, Some(100));

        assert_eq!(db.get_stump_id(999).unwrap(), None);
        assert_eq!(db.get_yac_id("nonexistent").unwrap(), None);
    }

    /// Regression for the migration heuristic: re-opening a CURRENT-schema
    /// mapping.db must NOT be mistaken for the legacy schema and must preserve
    /// all derived rows.
    #[test]
    fn test_reopen_preserves_mappings() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap();
        {
            let db = MappingDb::open(path).unwrap();
            let lib = db.ensure_library_mapping("/c", "s", "/srv/c").unwrap();
            db.insert_mapping(lib, 1, 2, "sm-1", "a.cbz", "a.cbz", "path")
                .unwrap();
        }
        let db = MappingDb::open(path).unwrap();
        assert_eq!(
            db.get_stump_id(1).unwrap(),
            Some("sm-1".to_string()),
            "re-opening a current-schema DB must preserve mappings"
        );
    }

    /// M4 migration: a mapping.db built with the OLD 3-column UNIQUE key (which
    /// allowed duplicate rows for one comic) is detected on open, its derived
    /// tables dropped, and recreated with the 2-column key that upserts in place.
    #[test]
    fn test_legacy_schema_is_rebuilt() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap();
        {
            let conn = Connection::open(path).unwrap();
            conn.execute_batch(
                "CREATE TABLE library_mapping (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    yac_library_path TEXT NOT NULL,
                    stump_library_id TEXT NOT NULL,
                    stump_library_path TEXT NOT NULL,
                    UNIQUE(yac_library_path, stump_library_id)
                );
                CREATE TABLE media_mapping (
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
                INSERT INTO library_mapping (id, yac_library_path, stump_library_id, stump_library_path)
                    VALUES (1, '/c', 's', '/srv/c');
                INSERT INTO media_mapping (library_mapping_id, yac_comic_info_id, yac_comic_id, stump_media_id, relative_path, filename, matched_via)
                    VALUES (1, 1, 2, 'old-1', 'a.cbz', 'a.cbz', 'path'),
                           (1, 1, 2, 'old-2', 'a.cbz', 'a.cbz', 'path');",
            )
            .unwrap();
        }

        let db = MappingDb::open(path).unwrap();
        let lib = db.ensure_library_mapping("/c", "s", "/srv/c").unwrap();
        assert!(
            db.get_all_mappings(lib).unwrap().is_empty(),
            "legacy derived rows are dropped; they rebuild on the next sync"
        );

        // The rebuilt table enforces the new key: a remap upserts in place.
        db.insert_mapping(lib, 1, 2, "new-1", "a.cbz", "a.cbz", "path")
            .unwrap();
        db.insert_mapping(lib, 1, 2, "new-2", "a.cbz", "a.cbz", "path")
            .unwrap();
        assert_eq!(db.get_all_mappings(lib).unwrap().len(), 1);
        assert_eq!(db.get_stump_id(1).unwrap(), Some("new-2".to_string()));
    }

    #[test]
    fn test_ensure_library_mapping_idempotent() {
        let (db, _tmp) = temp_db();
        let id1 = db
            .ensure_library_mapping("/comics", "stump-lib-1", "/srv/comics")
            .unwrap();
        let id2 = db
            .ensure_library_mapping("/comics", "stump-lib-1", "/srv/comics")
            .unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_sync_state_update_and_retrieval() {
        let (db, _tmp) = temp_db();
        let lib_id = db
            .ensure_library_mapping("/comics", "stump-lib-1", "/srv/comics")
            .unwrap();
        let mapping_id = db
            .insert_mapping(lib_id, 100, 200, "stump-media-1", "Marvel/001.cbz", "001.cbz", "path")
            .unwrap();

        assert!(db.get_sync_state(mapping_id).unwrap().is_none());

        db.update_sync_state(mapping_id, 5, 3, false, false, None, None)
            .unwrap();

        let state = db.get_sync_state(mapping_id).unwrap().unwrap();
        assert_eq!(state.yac_current_page, 5);
        assert_eq!(state.stump_current_page, 3);
        assert!(!state.yac_read);
        assert!(!state.stump_complete);

        db.update_sync_state(mapping_id, 10, 10, true, true, Some("2024-01-01"), Some("2024-01-02"))
            .unwrap();

        let state = db.get_sync_state(mapping_id).unwrap().unwrap();
        assert_eq!(state.yac_current_page, 10);
        assert_eq!(state.stump_current_page, 10);
        assert!(state.yac_read);
        assert!(state.stump_complete);
        assert_eq!(state.yac_last_modified, Some("2024-01-01".to_string()));
        assert_eq!(state.stump_last_modified, Some("2024-01-02".to_string()));
    }

    #[test]
    fn test_get_all_mappings() {
        let (db, _tmp) = temp_db();
        let lib_id = db
            .ensure_library_mapping("/comics", "stump-lib-1", "/srv/comics")
            .unwrap();

        db.insert_mapping(lib_id, 100, 200, "sm-1", "Marvel/001.cbz", "001.cbz", "path")
            .unwrap();
        db.insert_mapping(lib_id, 101, 201, "sm-2", "Marvel/002.cbz", "002.cbz", "path")
            .unwrap();

        let mappings = db.get_all_mappings(lib_id).unwrap();
        assert_eq!(mappings.len(), 2);
        assert_eq!(mappings[0].stump_media_id, "sm-1");
        assert_eq!(mappings[1].stump_media_id, "sm-2");
    }
}
