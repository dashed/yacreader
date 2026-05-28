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

fn build_media_response_json(media_list: &[MockMedia]) -> serde_json::Value {
    let media_json: Vec<serde_json::Value> = media_list
        .iter()
        .map(|m| {
            let read_progresses = if let Some(page) = m.read_page {
                let pct = if m.is_complete {
                    100.0
                } else {
                    (page as f64 / m.pages as f64) * 100.0
                };
                let completed_at = if m.is_complete {
                    json!("2024-01-01")
                } else {
                    json!(null)
                };
                json!([{
                    "page": page,
                    "percentage_completed": pct,
                    "is_completed": m.is_complete,
                    "epubCfi": null,
                    "completedAt": completed_at,
                    "updatedAt": null
                }])
            } else {
                json!([])
            };

            json!({
                "id": m.id,
                "name": m.name,
                "pages": m.pages,
                "path": m.path,
                "readProgresses": read_progresses
            })
        })
        .collect();

    json!({
        "data": {
            "libraries": {
                "nodes": [{
                    "series": {
                        "nodes": [{
                            "media": media_json
                        }]
                    }
                }]
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
                    "putMediaCompletion": {
                        "isCompleted": true
                    }
                }
            })),
        )
        .mount(server)
        .await;
}
