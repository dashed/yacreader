use std::sync::Mutex;

use rusqlite::{params, Connection, OpenFlags};

use crate::types::{MediaMapping, SyncError, SyncState};

pub struct MappingDb {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = include_str!("schema.sql");

impl MappingDb {
    pub fn open(path: &str) -> Result<Self, SyncError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
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
        let result = conn
            .query_row(
                "SELECT stump_media_id FROM media_mapping WHERE yac_comic_info_id = ?1",
                params![yac_comic_info_id],
                |row| row.get(0),
            )
            .ok();
        Ok(result)
    }

    pub fn get_yac_id(&self, stump_media_id: &str) -> Result<Option<i64>, SyncError> {
        let conn = self.lock_conn()?;
        let result = conn
            .query_row(
                "SELECT yac_comic_info_id FROM media_mapping WHERE stump_media_id = ?1",
                params![stump_media_id],
                |row| row.get(0),
            )
            .ok();
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
        conn.execute(
            "INSERT OR IGNORE INTO media_mapping (library_mapping_id, yac_comic_info_id, yac_comic_id, stump_media_id, relative_path, filename, matched_via) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![library_mapping_id, yac_comic_info_id, yac_comic_id, stump_media_id, relative_path, filename, matched_via],
        )?;
        Ok(conn.last_insert_rowid())
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
            .ok();
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
