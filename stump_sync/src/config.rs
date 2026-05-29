#[derive(Debug, Clone)]
pub struct Config {
    pub stump_url: String,
    pub api_key: String,
    pub user_id: String,
    pub mapping_db_path: String,
    pub sync_interval_secs: u64,
    pub libraries: Vec<LibraryConfig>,
}

#[derive(Debug, Clone)]
pub struct LibraryConfig {
    /// YACReader's numeric library id (the legacy id used in `/v2/library/<id>/`
    /// URLs and carried by the `comicUpdated` signal). Used to resolve which
    /// library a per-comic push belongs to.
    pub yac_library_id: i64,
    pub ydb_path: String,
    pub library_root: String,
    /// Stump library id. Empty means "auto-discover at init".
    pub stump_library_id: String,
    /// Stump library filesystem path. Empty means "auto-discover at init".
    pub stump_library_path: String,
}
