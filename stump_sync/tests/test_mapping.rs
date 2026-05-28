use stump_sync::mapping_db::MappingDb;
use tempfile::TempDir;

#[test]
fn test_full_mapping_lifecycle() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("mappings.db");
    let db = MappingDb::open(db_path.to_str().unwrap()).unwrap();

    let lib_id = db
        .ensure_library_mapping("/comics", "stump-lib-1", "/srv/comics")
        .unwrap();
    assert!(lib_id > 0);

    db.insert_mapping(
        lib_id, 100, 200, "sm-1", "Marvel/001.cbz", "001.cbz", "path",
    )
    .unwrap();
    db.insert_mapping(
        lib_id, 101, 201, "sm-2", "DC/001.cbz", "001.cbz", "path",
    )
    .unwrap();

    assert_eq!(db.get_stump_id(100).unwrap(), Some("sm-1".to_string()));
    assert_eq!(db.get_stump_id(101).unwrap(), Some("sm-2".to_string()));
    assert_eq!(db.get_yac_id("sm-1").unwrap(), Some(100));
    assert_eq!(db.get_yac_id("sm-2").unwrap(), Some(101));
    assert_eq!(db.get_stump_id(999).unwrap(), None);
    assert_eq!(db.get_yac_id("nonexistent").unwrap(), None);

    let mappings = db.get_all_mappings(lib_id).unwrap();
    assert_eq!(mappings.len(), 2);
    assert_eq!(mappings[0].relative_path, "Marvel/001.cbz");
    assert_eq!(mappings[1].relative_path, "DC/001.cbz");
}

#[test]
fn test_ensure_library_mapping_idempotent_across_sessions() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("mappings.db");

    let id1 = {
        let db = MappingDb::open(db_path.to_str().unwrap()).unwrap();
        db.ensure_library_mapping("/comics", "stump-lib-1", "/srv/comics")
            .unwrap()
    };

    let id2 = {
        let db = MappingDb::open(db_path.to_str().unwrap()).unwrap();
        db.ensure_library_mapping("/comics", "stump-lib-1", "/srv/comics")
            .unwrap()
    };

    assert_eq!(id1, id2);
}

#[test]
fn test_get_all_mappings_isolated_by_library() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("mappings.db");
    let db = MappingDb::open(db_path.to_str().unwrap()).unwrap();

    let lib1 = db
        .ensure_library_mapping("/comics1", "stump-lib-1", "/srv/comics1")
        .unwrap();
    let lib2 = db
        .ensure_library_mapping("/comics2", "stump-lib-2", "/srv/comics2")
        .unwrap();

    db.insert_mapping(
        lib1, 100, 200, "sm-1", "Marvel/001.cbz", "001.cbz", "path",
    )
    .unwrap();
    db.insert_mapping(
        lib1, 101, 201, "sm-2", "Marvel/002.cbz", "002.cbz", "path",
    )
    .unwrap();
    db.insert_mapping(
        lib2, 102, 202, "sm-3", "DC/001.cbz", "001.cbz", "path",
    )
    .unwrap();

    let mappings1 = db.get_all_mappings(lib1).unwrap();
    assert_eq!(mappings1.len(), 2);
    assert!(mappings1.iter().all(|m| m.library_mapping_id == lib1));

    let mappings2 = db.get_all_mappings(lib2).unwrap();
    assert_eq!(mappings2.len(), 1);
    assert_eq!(mappings2[0].stump_media_id, "sm-3");
}

#[test]
fn test_sync_state_roundtrip_with_realistic_data() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("mappings.db");
    let db = MappingDb::open(db_path.to_str().unwrap()).unwrap();

    let lib_id = db
        .ensure_library_mapping("/comics", "stump-lib-1", "/srv/comics")
        .unwrap();
    let m1 = db
        .insert_mapping(
            lib_id, 100, 200, "sm-1", "Marvel/001.cbz", "001.cbz", "path",
        )
        .unwrap();
    let m2 = db
        .insert_mapping(
            lib_id, 101, 201, "sm-2", "Marvel/002.cbz", "002.cbz", "path",
        )
        .unwrap();

    assert!(db.get_sync_state(m1).unwrap().is_none());

    db.update_sync_state(m1, 5, 3, false, false, None, None)
        .unwrap();
    db.update_sync_state(m2, 20, 20, true, true, Some("2024-06-01"), Some("2024-06-02"))
        .unwrap();

    let s1 = db.get_sync_state(m1).unwrap().unwrap();
    assert_eq!(s1.yac_current_page, 5);
    assert_eq!(s1.stump_current_page, 3);
    assert!(!s1.yac_read);
    assert!(!s1.stump_complete);
    assert!(s1.yac_last_modified.is_none());

    let s2 = db.get_sync_state(m2).unwrap().unwrap();
    assert_eq!(s2.yac_current_page, 20);
    assert_eq!(s2.stump_current_page, 20);
    assert!(s2.yac_read);
    assert!(s2.stump_complete);
    assert_eq!(s2.yac_last_modified.as_deref(), Some("2024-06-01"));
    assert_eq!(s2.stump_last_modified.as_deref(), Some("2024-06-02"));

    db.update_sync_state(m1, 15, 15, true, true, Some("2024-07-01"), None)
        .unwrap();
    let s1_updated = db.get_sync_state(m1).unwrap().unwrap();
    assert_eq!(s1_updated.yac_current_page, 15);
    assert!(s1_updated.yac_read);
}
