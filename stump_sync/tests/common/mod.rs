use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path as req_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub struct MockMedia {
    pub id: String,
    pub name: String,
    pub pages: i32,
    pub path: String,
    pub read_page: Option<i32>,
    pub is_complete: bool,
}

pub fn create_test_ydb(dir: &Path) -> PathBuf {
    let ydb_dir = dir.join(".yacreaderlibrary");
    std::fs::create_dir_all(&ydb_dir).unwrap();
    let ydb_path = ydb_dir.join("library.ydb");

    let conn = Connection::open(&ydb_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE folder (
            id INTEGER PRIMARY KEY,
            parentId INTEGER NOT NULL,
            name TEXT NOT NULL,
            path TEXT NOT NULL
        );
        INSERT INTO folder VALUES (1, 1, 'root', '/');

        CREATE TABLE comic_info (
            id INTEGER PRIMARY KEY,
            hash TEXT UNIQUE NOT NULL,
            currentPage INTEGER DEFAULT 1,
            numPages INTEGER,
            hasBeenOpened INTEGER DEFAULT 0,
            read INTEGER DEFAULT 0,
            lastTimeOpened INTEGER,
            rating REAL DEFAULT 0
        );

        CREATE TABLE comic (
            id INTEGER PRIMARY KEY,
            parentId INTEGER NOT NULL,
            comicInfoId INTEGER NOT NULL,
            fileName TEXT NOT NULL,
            path TEXT
        );",
    )
    .unwrap();

    ydb_path
}

pub fn open_ydb(ydb_path: &Path) -> Connection {
    Connection::open(ydb_path).unwrap()
}

pub fn insert_comic(
    conn: &Connection,
    comic_info_id: i64,
    hash: &str,
    filename: &str,
    dir_path: &str,
    current_page: i32,
    num_pages: i32,
    read: bool,
    last_time_opened: i64,
) {
    let has_been_opened = i32::from(current_page > 1 || read);
    conn.execute(
        "INSERT INTO comic_info (id, hash, currentPage, numPages, hasBeenOpened, read, lastTimeOpened)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            comic_info_id,
            hash,
            current_page,
            num_pages,
            has_been_opened,
            read as i32,
            last_time_opened
        ],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO comic (parentId, comicInfoId, fileName, path) VALUES (1, ?1, ?2, ?3)",
        rusqlite::params![comic_info_id, filename, dir_path],
    )
    .unwrap();
}

/// Build the real `libraryById { media { ... readProgress readHistory } }`
/// response shape. `read_page` → the nullable singular `readProgress` object;
/// `is_complete` → a one-entry `readHistory` (presence == completed). The two
/// are independent: a completed comic may have `readProgress = null`.
pub fn build_media_response_json(media_list: &[MockMedia]) -> serde_json::Value {
    let media_json: Vec<serde_json::Value> = media_list
        .iter()
        .map(|m| {
            let read_progress = match m.read_page {
                Some(page) => json!({
                    "page": page,
                    "updatedAt": "2026-05-20T14:32:10Z"
                }),
                None => json!(null),
            };
            let read_history = if m.is_complete {
                json!([{ "completedAt": "2026-05-19T09:01:00Z" }])
            } else {
                json!([])
            };

            json!({
                "id": m.id,
                "name": m.name,
                "pages": m.pages,
                "path": m.path,
                "readProgress": read_progress,
                "readHistory": read_history
            })
        })
        .collect();

    json!({
        "data": {
            "libraryById": {
                "id": "lib-1",
                "media": media_json
            }
        }
    })
}

pub async fn mock_library_media_response(server: &MockServer, media_list: &[MockMedia]) {
    let response_json = build_media_response_json(media_list);

    Mock::given(method("POST"))
        .and(req_path("/graphql"))
        .and(body_string_contains("GetLibraryMedia"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response_json))
        .mount(server)
        .await;
}

pub async fn mock_update_progress_response(server: &MockServer) {
    Mock::given(method("POST"))
        .and(req_path("/graphql"))
        .and(body_string_contains("UpdateProgress"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "updateMediaProgress": {
                        "__typename": "ActiveReadingSession",
                        "id": "media",
                        "page": 1
                    }
                }
            })),
        )
        .mount(server)
        .await;
}

pub async fn mock_mark_complete_response(server: &MockServer) {
    Mock::given(method("POST"))
        .and(req_path("/graphql"))
        .and(body_string_contains("MarkComplete"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "markMediaAsComplete": {
                        "id": "media",
                        "completedAt": "2024-01-01T00:00:00Z"
                    }
                }
            })),
        )
        .mount(server)
        .await;
}

pub struct ComicRow {
    pub current_page: i32,
    pub read: bool,
    pub has_been_opened: bool,
    pub last_time_opened: Option<i64>,
}

pub fn read_comic_from_ydb(ydb_path: &Path, comic_info_id: i64) -> ComicRow {
    let conn = open_ydb(ydb_path);
    conn.query_row(
        "SELECT currentPage, read, hasBeenOpened, lastTimeOpened FROM comic_info WHERE id = ?1",
        rusqlite::params![comic_info_id],
        |row| {
            Ok(ComicRow {
                current_page: row.get::<_, Option<i32>>(0)?.unwrap_or(0),
                read: row.get::<_, Option<i32>>(1)?.unwrap_or(0) != 0,
                has_been_opened: row.get::<_, Option<i32>>(2)?.unwrap_or(0) != 0,
                last_time_opened: row.get(3)?,
            })
        },
    )
    .unwrap()
}

pub fn mock_media_stump_ahead(id: &str, path: &str, stump_page: i32, total_pages: i32) -> MockMedia {
    MockMedia {
        id: id.to_string(),
        name: path.rsplit('/').next().unwrap_or(path).to_string(),
        pages: total_pages,
        path: path.to_string(),
        read_page: Some(stump_page),
        is_complete: false,
    }
}

pub fn mock_media_stump_complete(id: &str, path: &str, page: i32, total_pages: i32) -> MockMedia {
    MockMedia {
        id: id.to_string(),
        name: path.rsplit('/').next().unwrap_or(path).to_string(),
        pages: total_pages,
        path: path.to_string(),
        read_page: Some(page),
        is_complete: true,
    }
}

/// Completed on Stump with NO active reading session: `readProgress = null`,
/// `readHistory = [entry]`. `current_page()` falls back to `total_pages`.
pub fn mock_media_completed_no_active(id: &str, path: &str, total_pages: i32) -> MockMedia {
    MockMedia {
        id: id.to_string(),
        name: path.rsplit('/').next().unwrap_or(path).to_string(),
        pages: total_pages,
        path: path.to_string(),
        read_page: None,
        is_complete: true,
    }
}
