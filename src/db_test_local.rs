#[test]
fn test_db_open() {
    let _ = std::fs::remove_file("test_open.db");
    let db = crate::db::Database::open(std::path::Path::new("test_open.db")).unwrap();
}
