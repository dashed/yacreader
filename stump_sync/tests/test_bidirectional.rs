mod common;

use common::*;
use stump_sync::config::LibraryConfig;
use stump_sync::mapping_db::MappingDb;
use stump_sync::stump_client::StumpClient;
use stump_sync::sync_engine::SyncEngine;
use tempfile::TempDir;
use wiremock::MockServer;

fn make_engine(
    server: &MockServer,
    mapping_db_path: &str,
    lib_configs: Vec<LibraryConfig>,
) -> SyncEngine {
    let client =
        StumpClient::new(server.uri(), "test-key".into(), "test-user".into()).unwrap();
    let mapping_db = MappingDb::open(mapping_db_path).unwrap();
    SyncEngine::new(client, mapping_db, lib_configs)
}

fn requests_containing(requests: &[wiremock::Request], needle: &str) -> Vec<String> {
    requests
        .iter()
        .filter_map(|r| {
            let body = String::from_utf8_lossy(&r.body).to_string();
            if body.contains(needle) {
                Some(body)
            } else {
                None
            }
        })
        .collect()
}

#[tokio::test]
async fn test_pull_when_stump_ahead() {
    let dir = TempDir::new().unwrap();
    let ydb_path = create_test_ydb(dir.path());
    let conn = open_ydb(&ydb_path);
    insert_comic(&conn, 1, "hash-1", "001.cbz", "Marvel", 5, 20, false, 1700000000);
    drop(conn);

    let server = MockServer::start().await;
    mock_library_media_response(
        &server,
        &[mock_media_stump_ahead(
            "media-1",
            "/srv/comics/Marvel/001.cbz",
            20,
            20,
        )],
    )
    .await;

    let mapping_db_path = dir.path().join("mappings.db");
    let lib_config = LibraryConfig {
        ydb_path: ydb_path.to_str().unwrap().to_string(),
        library_root: dir.path().to_str().unwrap().to_string(),
        stump_library_id: "lib-1".into(),
        stump_library_path: "/srv/comics".into(),
    };
    let engine = make_engine(
        &server,
        mapping_db_path.to_str().unwrap(),
        vec![lib_config],
    );

    let report = engine.sync_all().await.unwrap();

    let comic = read_comic_from_ydb(&ydb_path, 1);
    assert_eq!(comic.current_page, 20);

    let requests = server.received_requests().await.unwrap();
    let updates = requests_containing(&requests, "UpdateProgress");
    assert!(updates.is_empty(), "expected no UpdateProgress mutations");

    assert_eq!(report.pages_pulled, 1);
    assert_eq!(report.pages_pushed, 0);
    assert!(report.errors.is_empty());
}

#[tokio::test]
async fn test_bidirectional_mixed() {
    let dir = TempDir::new().unwrap();
    let ydb_path = create_test_ydb(dir.path());
    let conn = open_ydb(&ydb_path);
    // A: YAC page 15/20, Stump page 5 → push to Stump
    insert_comic(
        &conn, 1, "hash-a", "001.cbz", "Marvel", 15, 20, false, 1700000000,
    );
    // B: YAC page 3/30, Stump page 20 → pull from Stump
    insert_comic(
        &conn, 2, "hash-b", "001.cbz", "DC", 3, 30, false, 1700000000,
    );
    // C: YAC page 10/20, Stump page 10 → no change
    insert_comic(
        &conn, 3, "hash-c", "001.cbz", "Manga", 10, 20, false, 1700000000,
    );
    drop(conn);

    let server = MockServer::start().await;
    mock_library_media_response(
        &server,
        &[
            MockMedia {
                id: "media-a".into(),
                name: "001.cbz".into(),
                pages: 20,
                path: "/srv/comics/Marvel/001.cbz".into(),
                read_page: Some(5),
                is_complete: false,
            },
            mock_media_stump_ahead("media-b", "/srv/comics/DC/001.cbz", 20, 30),
            MockMedia {
                id: "media-c".into(),
                name: "001.cbz".into(),
                pages: 20,
                path: "/srv/comics/Manga/001.cbz".into(),
                read_page: Some(10),
                is_complete: false,
            },
        ],
    )
    .await;
    mock_update_progress_response(&server).await;

    let mapping_db_path = dir.path().join("mappings.db");
    let lib_config = LibraryConfig {
        ydb_path: ydb_path.to_str().unwrap().to_string(),
        library_root: dir.path().to_str().unwrap().to_string(),
        stump_library_id: "lib-1".into(),
        stump_library_path: "/srv/comics".into(),
    };
    let engine = make_engine(
        &server,
        mapping_db_path.to_str().unwrap(),
        vec![lib_config],
    );

    let report = engine.sync_all().await.unwrap();

    // A: pushed to Stump (mutation received for page 15)
    let requests = server.received_requests().await.unwrap();
    let updates = requests_containing(&requests, "UpdateProgress");
    assert_eq!(updates.len(), 1);
    assert!(updates[0].contains("media-a"));
    assert!(updates[0].contains("\"page\":15"));

    // B: pulled to .ydb (now at page 20)
    let comic_b = read_comic_from_ydb(&ydb_path, 2);
    assert_eq!(comic_b.current_page, 20);

    // C: unchanged (no mutation, .ydb unchanged)
    let comic_c = read_comic_from_ydb(&ydb_path, 3);
    assert_eq!(comic_c.current_page, 10);
    assert!(updates.iter().all(|b| !b.contains("media-c")));

    assert_eq!(report.pages_pushed, 1);
    assert_eq!(report.pages_pulled, 1);
    assert!(report.errors.is_empty());
}

#[tokio::test]
async fn test_conflict_resolution_max_page() {
    let dir = TempDir::new().unwrap();
    let ydb_path = create_test_ydb(dir.path());
    let conn = open_ydb(&ydb_path);
    insert_comic(
        &conn, 1, "hash-1", "001.cbz", "Comics", 10, 20, false, 1700000000,
    );
    drop(conn);

    let server = MockServer::start().await;
    mock_library_media_response(
        &server,
        &[mock_media_stump_ahead(
            "media-1",
            "/srv/comics/Comics/001.cbz",
            15,
            20,
        )],
    )
    .await;

    let mapping_db_path = dir.path().join("mappings.db");
    let lib_config = LibraryConfig {
        ydb_path: ydb_path.to_str().unwrap().to_string(),
        library_root: dir.path().to_str().unwrap().to_string(),
        stump_library_id: "lib-1".into(),
        stump_library_path: "/srv/comics".into(),
    };
    let engine = make_engine(
        &server,
        mapping_db_path.to_str().unwrap(),
        vec![lib_config],
    );

    let report = engine.sync_all().await.unwrap();

    // Stump wins (higher page), .ydb updated to page 15
    let comic = read_comic_from_ydb(&ydb_path, 1);
    assert_eq!(comic.current_page, 15);

    // No mutation sent to Stump
    let requests = server.received_requests().await.unwrap();
    let updates = requests_containing(&requests, "UpdateProgress");
    assert!(updates.is_empty());
    let completions = requests_containing(&requests, "MarkComplete");
    assert!(completions.is_empty());

    assert_eq!(report.pages_pulled, 1);
    assert_eq!(report.pages_pushed, 0);
}

#[tokio::test]
async fn test_completion_stump_to_yac() {
    let dir = TempDir::new().unwrap();
    let ydb_path = create_test_ydb(dir.path());
    let conn = open_ydb(&ydb_path);
    insert_comic(
        &conn, 1, "hash-1", "001.cbz", "Comics", 20, 20, false, 1700000000,
    );
    drop(conn);

    let server = MockServer::start().await;
    mock_library_media_response(
        &server,
        &[mock_media_stump_complete(
            "media-1",
            "/srv/comics/Comics/001.cbz",
            20,
            20,
        )],
    )
    .await;

    let mapping_db_path = dir.path().join("mappings.db");
    let lib_config = LibraryConfig {
        ydb_path: ydb_path.to_str().unwrap().to_string(),
        library_root: dir.path().to_str().unwrap().to_string(),
        stump_library_id: "lib-1".into(),
        stump_library_path: "/srv/comics".into(),
    };
    let engine = make_engine(
        &server,
        mapping_db_path.to_str().unwrap(),
        vec![lib_config],
    );

    let report = engine.sync_all().await.unwrap();

    let comic = read_comic_from_ydb(&ydb_path, 1);
    assert!(comic.read, "expected .ydb read=true after pulling completion");
    assert_eq!(comic.current_page, 20);

    assert_eq!(report.completions_pulled, 1);
    assert_eq!(report.pages_pulled, 0);
    assert!(report.errors.is_empty());
}

#[tokio::test]
async fn test_sync_state_tracking() {
    let dir = TempDir::new().unwrap();
    let ydb_path = create_test_ydb(dir.path());
    let ydb_str = ydb_path.to_str().unwrap().to_string();
    let conn = open_ydb(&ydb_path);
    insert_comic(
        &conn, 1, "hash-1", "001.cbz", "Comics", 5, 20, false, 1700000000,
    );
    drop(conn);

    let server = MockServer::start().await;
    mock_library_media_response(
        &server,
        &[mock_media_stump_ahead(
            "media-1",
            "/srv/comics/Comics/001.cbz",
            10,
            20,
        )],
    )
    .await;

    let mapping_db_path = dir.path().join("mappings.db");
    let mapping_db_str = mapping_db_path.to_str().unwrap().to_string();
    let lib_config = LibraryConfig {
        ydb_path: ydb_str.clone(),
        library_root: dir.path().to_str().unwrap().to_string(),
        stump_library_id: "lib-1".into(),
        stump_library_path: "/srv/comics".into(),
    };
    let engine = make_engine(&server, &mapping_db_str, vec![lib_config]);

    let report = engine.sync_all().await.unwrap();
    assert!(report.errors.is_empty());

    // Query sync_state from the mapping DB
    let mapping_db = MappingDb::open(&mapping_db_str).unwrap();
    let lib_id = mapping_db
        .ensure_library_mapping(&ydb_str, "lib-1", "/srv/comics")
        .unwrap();
    let mappings = mapping_db.get_all_mappings(lib_id).unwrap();
    assert_eq!(mappings.len(), 1);

    let state = mapping_db
        .get_sync_state(mappings[0].id)
        .unwrap()
        .expect("sync_state should exist after sync_all");
    // sync_state records pre-pull YAC page and the Stump page at sync time
    assert_eq!(state.yac_current_page, 5);
    assert_eq!(state.stump_current_page, 10);
    assert!(!state.yac_read);
    assert!(!state.stump_complete);
}

#[tokio::test]
async fn test_push_still_works() {
    let dir = TempDir::new().unwrap();
    let ydb_path = create_test_ydb(dir.path());
    let conn = open_ydb(&ydb_path);
    insert_comic(
        &conn, 1, "hash-1", "001.cbz", "Comics", 15, 20, false, 1700000000,
    );
    drop(conn);

    let server = MockServer::start().await;
    mock_library_media_response(
        &server,
        &[MockMedia {
            id: "media-1".into(),
            name: "001.cbz".into(),
            pages: 20,
            path: "/srv/comics/Comics/001.cbz".into(),
            read_page: Some(5),
            is_complete: false,
        }],
    )
    .await;
    mock_update_progress_response(&server).await;

    let mapping_db_path = dir.path().join("mappings.db");
    let lib_config = LibraryConfig {
        ydb_path: ydb_path.to_str().unwrap().to_string(),
        library_root: dir.path().to_str().unwrap().to_string(),
        stump_library_id: "lib-1".into(),
        stump_library_path: "/srv/comics".into(),
    };
    let engine = make_engine(
        &server,
        mapping_db_path.to_str().unwrap(),
        vec![lib_config],
    );

    let report = engine.push_all().await.unwrap();

    // Mutation sent for page 15
    let requests = server.received_requests().await.unwrap();
    let updates = requests_containing(&requests, "UpdateProgress");
    assert_eq!(updates.len(), 1);
    assert!(updates[0].contains("\"page\":15"));

    // .ydb NOT modified (push_all doesn't pull)
    let comic = read_comic_from_ydb(&ydb_path, 1);
    assert_eq!(comic.current_page, 15);

    assert_eq!(report.pages_pushed, 1);
    assert_eq!(report.pages_pulled, 0);
    assert!(report.errors.is_empty());
}
