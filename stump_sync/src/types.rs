use serde::Deserialize;
use std::fmt;

#[derive(Debug, Clone)]
pub struct ComicProgress {
    pub comic_info_id: i64,
    pub comic_id: i64,
    pub current_page: i32,
    pub num_pages: i32,
    pub read: bool,
    pub has_been_opened: bool,
    pub last_time_opened: Option<i64>,
    pub hash: Option<String>,
    pub relative_path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StumpMedia {
    pub id: String,
    pub name: String,
    pub pages: i32,
    pub path: String,
    #[serde(rename = "readProgresses")]
    pub read_progresses: Vec<ReadProgress>,
}

impl StumpMedia {
    pub fn current_page(&self) -> i32 {
        self.read_progresses
            .first()
            .map(|rp| rp.page)
            .unwrap_or(0)
    }

    pub fn is_complete(&self) -> bool {
        self.read_progresses
            .first()
            .map(|rp| rp.is_completed)
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReadProgress {
    pub page: i32,
    pub percentage_completed: Option<f64>,
    pub is_completed: bool,
    #[serde(rename = "epubCfi")]
    pub epub_cfi: Option<String>,
    #[serde(rename = "completedAt")]
    pub completed_at: Option<String>,
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SyncDelta {
    pub stump_media_id: String,
    pub new_page: Option<i32>,
    pub should_mark_complete: bool,
}

#[derive(Debug, Clone)]
pub struct MediaMapping {
    pub id: i64,
    pub library_mapping_id: i64,
    pub yac_comic_info_id: i64,
    pub yac_comic_id: i64,
    pub stump_media_id: String,
    pub relative_path: String,
    pub filename: String,
    pub matched_via: String,
}

#[derive(Debug, Clone)]
pub struct SyncState {
    pub id: i64,
    pub media_mapping_id: i64,
    pub yac_current_page: i32,
    pub stump_current_page: i32,
    pub yac_read: bool,
    pub stump_complete: bool,
    pub yac_last_modified: Option<String>,
    pub stump_last_modified: Option<String>,
    pub last_synced_at: String,
}

#[derive(Debug, Clone)]
pub struct SyncReport {
    pub libraries_processed: u32,
    pub comics_matched: u32,
    pub pages_pushed: u32,
    pub completions_pushed: u32,
    pub errors: Vec<String>,
}

impl SyncReport {
    pub fn new() -> Self {
        Self {
            libraries_processed: 0,
            comics_matched: 0,
            pages_pushed: 0,
            completions_pushed: 0,
            errors: Vec::new(),
        }
    }
}

impl Default for SyncReport {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum SyncError {
    Database(String),
    Http(String),
    GraphQL(String),
    Config(String),
    NotInitialized,
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::Database(msg) => write!(f, "database error: {msg}"),
            SyncError::Http(msg) => write!(f, "HTTP error: {msg}"),
            SyncError::GraphQL(msg) => write!(f, "GraphQL error: {msg}"),
            SyncError::Config(msg) => write!(f, "config error: {msg}"),
            SyncError::NotInitialized => write!(f, "sync engine not initialized"),
        }
    }
}

impl std::error::Error for SyncError {}

impl From<rusqlite::Error> for SyncError {
    fn from(e: rusqlite::Error) -> Self {
        SyncError::Database(e.to_string())
    }
}

impl From<reqwest::Error> for SyncError {
    fn from(e: reqwest::Error) -> Self {
        SyncError::Http(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_error_display() {
        let err = SyncError::Database("table not found".into());
        assert_eq!(err.to_string(), "database error: table not found");

        let err = SyncError::Http("timeout".into());
        assert_eq!(err.to_string(), "HTTP error: timeout");

        let err = SyncError::GraphQL("invalid query".into());
        assert_eq!(err.to_string(), "GraphQL error: invalid query");

        let err = SyncError::Config("missing url".into());
        assert_eq!(err.to_string(), "config error: missing url");

        let err = SyncError::NotInitialized;
        assert_eq!(err.to_string(), "sync engine not initialized");
    }

    #[test]
    fn test_sync_report_default() {
        let report = SyncReport::new();
        assert_eq!(report.libraries_processed, 0);
        assert_eq!(report.comics_matched, 0);
        assert_eq!(report.pages_pushed, 0);
        assert_eq!(report.completions_pushed, 0);
        assert!(report.errors.is_empty());
    }

    #[test]
    fn test_stump_media_current_page_empty() {
        let media = StumpMedia {
            id: "id1".into(),
            name: "test".into(),
            pages: 20,
            path: "/comics/test.cbz".into(),
            read_progresses: vec![],
        };
        assert_eq!(media.current_page(), 0);
        assert!(!media.is_complete());
    }

    #[test]
    fn test_stump_media_current_page_with_progress() {
        let media = StumpMedia {
            id: "id1".into(),
            name: "test".into(),
            pages: 20,
            path: "/comics/test.cbz".into(),
            read_progresses: vec![ReadProgress {
                page: 10,
                percentage_completed: Some(50.0),
                is_completed: false,
                epub_cfi: None,
                completed_at: None,
                updated_at: None,
            }],
        };
        assert_eq!(media.current_page(), 10);
        assert!(!media.is_complete());
    }

    #[test]
    fn test_stump_media_is_complete() {
        let media = StumpMedia {
            id: "id1".into(),
            name: "test".into(),
            pages: 20,
            path: "/comics/test.cbz".into(),
            read_progresses: vec![ReadProgress {
                page: 20,
                percentage_completed: Some(100.0),
                is_completed: true,
                epub_cfi: None,
                completed_at: Some("2024-01-01".into()),
                updated_at: None,
            }],
        };
        assert_eq!(media.current_page(), 20);
        assert!(media.is_complete());
    }
}
