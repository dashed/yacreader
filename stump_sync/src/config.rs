#[derive(Debug, Clone)]
pub struct Config {
    pub stump_url: String,
    pub api_key: String,
    pub user_id: String,
    pub mapping_db_path: String,
    pub libraries: Vec<LibraryConfig>,
}

#[derive(Debug, Clone)]
pub struct LibraryConfig {
    pub ydb_path: String,
    pub library_root: String,
    pub stump_library_id: String,
    pub stump_library_path: String,
}
