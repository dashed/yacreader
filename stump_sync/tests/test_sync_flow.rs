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
async fn test_push_single_comic_progress() {
    let dir = TempDir::new().unwrap();
    let ydb_path = create_test_ydb(dir.path());
    let conn = open_ydb(&ydb_path);
    insert_comic(
        &conn, 1, "hash-batman-001", "001.cbz", "Batman", 15, 30, false, 1700000000,
    );
    drop(conn);

    let server = MockServer::start().await;
    mock_library_media_response(
        &server,
        &[MockMedia {
            id: "stump-media-1".into(),
            name: "001.cbz".into(),
            pages: 30,
            path: "/srv/comics/Batman/001.cbz".into(),
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

    engine.push_single(1, 1).await.unwrap();

    let requests = server.received_requests().await.unwrap();
    let updates = requests_containing(&requests, "UpdateProgress");
    assert_eq!(updates.len(), 1);
    assert!(updates[0].contains("\"page\":15"));
    assert!(updates[0].contains("stump-media-1"));
}

#[tokio::test]
async fn test_push_all_multiple_comics() {
    let dir = TempDir::new().unwrap();
    let ydb_path = create_test_ydb(dir.path());
    let conn = open_ydb(&ydb_path);
    // Comic A: YAC page 10/20, Stump page 5 → push page
    insert_comic(
        &conn, 1, "hash-a", "001.cbz", "Marvel/Spider-Man", 10, 20, false, 1700000000,
    );
    // Comic B: YAC page 20/20, read=true, Stump page 10 not complete → push page + mark complete
    insert_comic(
        &conn, 2, "hash-b", "Annual 1.cbz", "DC/Batman", 20, 20, true, 1700000000,
    );
    // Comic C: YAC page 3/30, Stump page 15 → no sync (Stump ahead)
    insert_comic(
        &conn, 3, "hash-c", "Chapter 1.cbz", "Manga/One Piece", 3, 30, false, 1700000000,
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
                path: "/srv/comics/Marvel/Spider-Man/001.cbz".into(),
                read_page: Some(5),
                is_complete: false,
            },
            MockMedia {
                id: "media-b".into(),
                name: "Annual 1.cbz".into(),
                pages: 20,
                path: "/srv/comics/DC/Batman/Annual 1.cbz".into(),
                read_page: Some(10),
                is_complete: false,
            },
            MockMedia {
                id: "media-c".into(),
                name: "Chapter 1.cbz".into(),
                pages: 30,
                path: "/srv/comics/Manga/One Piece/Chapter 1.cbz".into(),
                read_page: Some(15),
                is_complete: false,
            },
        ],
    )
    .await;
    mock_update_progress_response(&server).await;
    mock_mark_complete_response(&server).await;

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

    assert_eq!(report.libraries_processed, 1);
    assert_eq!(report.comics_matched, 3);
    assert_eq!(report.pages_pushed, 2);
    assert_eq!(report.completions_pushed, 1);
    assert!(report.errors.is_empty());

    let requests = server.received_requests().await.unwrap();

    let updates = requests_containing(&requests, "UpdateProgress");
    assert_eq!(updates.len(), 2);
    assert!(updates.iter().any(|b| b.contains("media-a") && b.contains("\"page\":10")));
    assert!(updates.iter().any(|b| b.contains("media-b") && b.contains("\"page\":20")));

    let completions = requests_containing(&requests, "MarkComplete");
    assert_eq!(completions.len(), 1);
    assert!(completions[0].contains("media-b"));

    // No mutations for comic C
    assert!(updates.iter().all(|b| !b.contains("media-c")));
    assert!(completions.iter().all(|b| !b.contains("media-c")));
}

#[tokio::test]
async fn test_no_sync_when_stump_ahead() {
    let dir = TempDir::new().unwrap();
    let ydb_path = create_test_ydb(dir.path());
    let conn = open_ydb(&ydb_path);
    insert_comic(
        &conn, 1, "hash-1", "001.cbz", "Comics", 5, 30, false, 1700000000,
    );
    drop(conn);

    let server = MockServer::start().await;
    mock_library_media_response(
        &server,
        &[MockMedia {
            id: "media-1".into(),
            name: "001.cbz".into(),
            pages: 30,
            path: "/srv/comics/Comics/001.cbz".into(),
            read_page: Some(15),
            is_complete: false,
        }],
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

    let report = engine.push_all().await.unwrap();

    assert_eq!(report.pages_pushed, 0);
    assert_eq!(report.completions_pushed, 0);

    let requests = server.received_requests().await.unwrap();
    let mutations = requests_containing(&requests, "UpdateProgress");
    assert!(mutations.is_empty());
    let completions = requests_containing(&requests, "MarkComplete");
    assert!(completions.is_empty());
}

#[tokio::test]
async fn test_path_matching_across_systems() {
    let dir = TempDir::new().unwrap();
    let ydb_path = create_test_ydb(dir.path());
    let conn = open_ydb(&ydb_path);
    insert_comic(
        &conn, 1, "hash-sm", "001.cbz", "Marvel/Spider-Man", 5, 20, false, 1700000000,
    );
    insert_comic(
        &conn, 2, "hash-bm", "Annual 1.cbz", "DC/Batman", 3, 20, false, 1700000000,
    );
    insert_comic(
        &conn, 3, "hash-op", "Chapter 1.cbz", "Manga/One Piece", 10, 50, false, 1700000000,
    );
    drop(conn);

    let server = MockServer::start().await;
    mock_library_media_response(
        &server,
        &[
            MockMedia {
                id: "sm-1".into(),
                name: "001.cbz".into(),
                pages: 20,
                path: "/srv/comics/Marvel/Spider-Man/001.cbz".into(),
                read_page: Some(1),
                is_complete: false,
            },
            MockMedia {
                id: "sm-2".into(),
                name: "Annual 1.cbz".into(),
                pages: 20,
                path: "/srv/comics/DC/Batman/Annual 1.cbz".into(),
                read_page: Some(1),
                is_complete: false,
            },
            MockMedia {
                id: "sm-3".into(),
                name: "Chapter 1.cbz".into(),
                pages: 50,
                path: "/srv/comics/Manga/One Piece/Chapter 1.cbz".into(),
                read_page: Some(1),
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

    let report = engine.push_all().await.unwrap();

    assert_eq!(report.comics_matched, 3);
    assert_eq!(report.pages_pushed, 3);
    assert!(report.errors.is_empty());

    let requests = server.received_requests().await.unwrap();
    let updates = requests_containing(&requests, "UpdateProgress");
    assert_eq!(updates.len(), 3);
    assert!(updates.iter().any(|b| b.contains("sm-1")));
    assert!(updates.iter().any(|b| b.contains("sm-2")));
    assert!(updates.iter().any(|b| b.contains("sm-3")));
}

#[tokio::test]
async fn test_unmatched_comics_skipped() {
    let dir = TempDir::new().unwrap();
    let ydb_path = create_test_ydb(dir.path());
    let conn = open_ydb(&ydb_path);
    insert_comic(
        &conn, 1, "hash-matched", "001.cbz", "Marvel", 10, 20, false, 1700000000,
    );
    insert_comic(
        &conn, 2, "hash-orphan", "002.cbz", "Indie/Obscure", 8, 20, false, 1700000000,
    );
    drop(conn);

    let server = MockServer::start().await;
    // Only one Stump media — the second YAC comic has no match
    mock_library_media_response(
        &server,
        &[MockMedia {
            id: "media-matched".into(),
            name: "001.cbz".into(),
            pages: 20,
            path: "/srv/comics/Marvel/001.cbz".into(),
            read_page: Some(3),
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

    assert_eq!(report.comics_matched, 1);
    assert_eq!(report.pages_pushed, 1);
    assert!(report.errors.is_empty());

    let requests = server.received_requests().await.unwrap();
    let updates = requests_containing(&requests, "UpdateProgress");
    assert_eq!(updates.len(), 1);
    assert!(updates[0].contains("media-matched"));
    assert!(updates[0].contains("\"page\":10"));
}
