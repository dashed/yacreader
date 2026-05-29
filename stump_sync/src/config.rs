#[derive(Clone)]
pub struct Config {
    pub stump_url: String,
    pub api_key: String,
    pub user_id: String,
    pub mapping_db_path: String,
    pub sync_interval_secs: u64,
    pub libraries: Vec<LibraryConfig>,
}

// Hand-written so the `api_key` secret can never leak into logs, panics, or
// error chains via `{:?}` (audit M1). Every other field is printed normally;
// only `api_key` is replaced with a fixed redaction marker.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("stump_url", &self.stump_url)
            .field("api_key", &"<redacted>")
            .field("user_id", &self.user_id)
            .field("mapping_db_path", &self.mapping_db_path)
            .field("sync_interval_secs", &self.sync_interval_secs)
            .field("libraries", &self.libraries)
            .finish()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// M1: the hand-written `Debug` must never expose the API key, while still
    /// rendering the other fields so the type stays useful in logs.
    #[test]
    fn debug_redacts_api_key() {
        let config = Config {
            stump_url: "https://stump.example".into(),
            api_key: "super-secret-key-value".into(),
            user_id: "user-1".into(),
            mapping_db_path: "/tmp/mappings.db".into(),
            sync_interval_secs: 300,
            libraries: vec![LibraryConfig {
                yac_library_id: 1,
                ydb_path: "/lib/.yacreaderlibrary/library.ydb".into(),
                library_root: "/lib".into(),
                stump_library_id: "stump-1".into(),
                stump_library_path: "/srv/lib".into(),
            }],
        };

        let rendered = format!("{config:?}");

        assert!(
            !rendered.contains("super-secret-key-value"),
            "api_key leaked through Debug: {rendered}"
        );
        assert!(
            rendered.contains("api_key: \"<redacted>\""),
            "expected redaction marker, got: {rendered}"
        );
        // Non-secret fields must still be visible for diagnostics.
        assert!(rendered.contains("https://stump.example"));
        assert!(rendered.contains("user-1"));
    }
}
