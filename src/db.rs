//! SQLite persistence layer for ratings, bookmarks, notes, and directory
//! records. Opened in WAL mode for graceful concurrent-instance behaviour.
//!
//! Sparse storage: only images that have at least one of (rating, bookmark,
//! note, rotation) produce a row in the `images` table. Rows that become empty are
//! deleted automatically by [`Database::gc_empty_record`].

use rusqlite::{params, Connection, Result};
use std::path::Path;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Schema SQL
// ---------------------------------------------------------------------------

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS directories (
    id                      INTEGER PRIMARY KEY AUTOINCREMENT,
    uuid                    TEXT    NOT NULL UNIQUE,
    path                    TEXT    NOT NULL UNIQUE,
    sort_override           TEXT,
    created_at              INTEGER NOT NULL,
    path_last_verified_at   INTEGER
);

CREATE TABLE IF NOT EXISTS images (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    directory_id        INTEGER NOT NULL REFERENCES directories(id),
    filename            TEXT    NOT NULL,
    file_size           INTEGER NOT NULL DEFAULT 0,
    file_modified_at    INTEGER NOT NULL DEFAULT 0,
    rating              INTEGER,
    bookmarked          INTEGER NOT NULL DEFAULT 0,
    rotation            INTEGER NOT NULL DEFAULT 0,
    note                TEXT,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    UNIQUE(directory_id, filename)
);

CREATE TABLE IF NOT EXISTS settings (
    key     TEXT PRIMARY KEY,
    value   TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_images_dir      ON images(directory_id);
CREATE INDEX IF NOT EXISTS idx_dirs_uuid       ON directories(uuid);
CREATE INDEX IF NOT EXISTS idx_dirs_path       ON directories(path);
";

// ---------------------------------------------------------------------------
// Record types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DirectoryRecord {
    pub id:                    i64,
    pub uuid:                  String,
    pub path:                  String,
    pub sort_override:         Option<String>,
    pub created_at:            i64,
    pub path_last_verified_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ImageRecord {
    pub id:               i64,
    pub directory_id:     i64,
    pub filename:         String,
    pub file_size:        i64,
    pub file_modified_at: i64,
    pub rating:           Option<u8>,
    pub bookmarked:       bool,
    pub rotation:         u8,
    pub note:             Option<String>,
    pub created_at:       i64,
    pub updated_at:       i64,
}

// ---------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------

/// A handle to a SQLite database opened in WAL mode.
pub struct Database {
    conn: Connection,
}

impl Database {
    /// Open (or create) a database at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                log::info!("creating database directory: {}", parent.display());
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let conn = Connection::open(path)?;
        let db = Self { conn };
        db.initialise()?;
        db.migrate()?;
        Ok(db)
    }

    /// Open a transient in-memory database (used in tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self { conn };
        db.initialise()?;
        Ok(db)
    }

    fn initialise(&self) -> Result<()> {
        self.conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        self.conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        self.conn.execute_batch(SCHEMA_SQL)?;
        Ok(())
    }

    fn migrate(&self) -> Result<()> {
        // Add rotation column if it doesn't exist
        let _ = self.conn.execute(
            "ALTER TABLE images ADD COLUMN rotation INTEGER NOT NULL DEFAULT 0",
            [],
        );
        Ok(())
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
    }

    // ── Directories ──────────────────────────────────────────────────────

    /// Find or create a directory record for `path`. Touches
    /// `path_last_verified_at` if the record already exists.
    pub fn upsert_directory_by_path(&self, path: &str) -> Result<DirectoryRecord> {
        let now = Self::now();
        if let Some(record) = self.find_directory_by_path(path)? {
            self.conn.execute(
                "UPDATE directories SET path_last_verified_at = ?1 WHERE id = ?2",
                params![now, record.id],
            )?;
            return Ok(record);
        }
        let uuid = Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO directories (uuid, path, created_at) VALUES (?1, ?2, ?3)",
            params![uuid, path, now],
        )?;
        Ok(DirectoryRecord {
            id: self.conn.last_insert_rowid(),
            uuid,
            path: path.to_string(),
            sort_override: None,
            created_at: now,
            path_last_verified_at: Some(now),
        })
    }

    pub fn find_directory_by_path(&self, path: &str) -> Result<Option<DirectoryRecord>> {
        query_opt(
            &self.conn,
            "SELECT id, uuid, path, sort_override, created_at, path_last_verified_at
             FROM directories WHERE path = ?1",
            params![path],
            map_dir_row,
        )
    }

    pub fn find_directory_by_uuid(&self, uuid: &str) -> Result<Option<DirectoryRecord>> {
        query_opt(
            &self.conn,
            "SELECT id, uuid, path, sort_override, created_at, path_last_verified_at
             FROM directories WHERE uuid = ?1",
            params![uuid],
            map_dir_row,
        )
    }

    pub fn update_directory_path(&self, id: i64, new_path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE directories SET path = ?1 WHERE id = ?2",
            params![new_path, id],
        )?;
        Ok(())
    }

    // ── Images ───────────────────────────────────────────────────────────

    pub fn get_image(&self, directory_id: i64, filename: &str) -> Result<Option<ImageRecord>> {
        query_opt(
            &self.conn,
            "SELECT id, directory_id, filename, file_size, file_modified_at,
                    rating, bookmarked, rotation, note, created_at, updated_at
             FROM images WHERE directory_id = ?1 AND filename = ?2",
            params![directory_id, filename],
            |row| map_image_row(row, 0),
        )
    }

    pub fn get_images(&self, directory_id: i64) -> Result<Vec<ImageRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, directory_id, filename, file_size, file_modified_at,
                    rating, bookmarked, rotation, note, created_at, updated_at
             FROM images WHERE directory_id = ?1",
        )?;
        let result: Result<Vec<_>> = stmt.query_map([directory_id], |row| map_image_row(row, 0))?.collect();
        result
    }

    pub fn set_rating(&self, directory_id: i64, filename: &str, rating: Option<u8>) -> Result<()> {
        self.ensure_image_exists(directory_id, filename)?;
        self.conn.execute(
            "UPDATE images SET rating = ?1, updated_at = ?2
             WHERE directory_id = ?3 AND filename = ?4",
            params![rating.map(|r| r as i64), Self::now(), directory_id, filename],
        )?;
        self.gc_empty_record(directory_id, filename)
    }

    pub fn set_bookmark(&self, directory_id: i64, filename: &str, bookmarked: bool) -> Result<()> {
        self.ensure_image_exists(directory_id, filename)?;
        self.conn.execute(
            "UPDATE images SET bookmarked = ?1, updated_at = ?2
             WHERE directory_id = ?3 AND filename = ?4",
            params![bookmarked as i64, Self::now(), directory_id, filename],
        )?;
        self.gc_empty_record(directory_id, filename)
    }

    pub fn set_rotation(&self, directory_id: i64, filename: &str, rotation: u8) -> Result<()> {
        self.ensure_image_exists(directory_id, filename)?;
        self.conn.execute(
            "UPDATE images SET rotation = ?1, updated_at = ?2
             WHERE directory_id = ?3 AND filename = ?4",
            params![rotation as i64, Self::now(), directory_id, filename],
        )?;
        self.gc_empty_record(directory_id, filename)
    }

    pub fn set_note(&self, directory_id: i64, filename: &str, note: Option<&str>) -> Result<()> {
        self.ensure_image_exists(directory_id, filename)?;
        self.conn.execute(
            "UPDATE images SET note = ?1, updated_at = ?2
             WHERE directory_id = ?3 AND filename = ?4",
            params![note, Self::now(), directory_id, filename],
        )?;
        self.gc_empty_record(directory_id, filename)
    }

    /// Insert a placeholder record if one doesn't already exist.
    fn ensure_image_exists(&self, directory_id: i64, filename: &str) -> Result<()> {
        let now = Self::now();
        self.conn.execute(
            "INSERT OR IGNORE INTO images
             (directory_id, filename, file_size, file_modified_at,
              rating, bookmarked, rotation, created_at, updated_at)
             VALUES (?1, ?2, 0, 0, NULL, 0, 0, ?3, ?3)",
            params![directory_id, filename, now],
        )?;
        Ok(())
    }

    /// Delete a record that has no data worth keeping (sparse-storage policy).
    fn gc_empty_record(&self, directory_id: i64, filename: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM images
             WHERE directory_id = ?1 AND filename = ?2
               AND rating IS NULL AND bookmarked = 0 AND rotation = 0
               AND (note IS NULL OR note = '')",
            params![directory_id, filename],
        )?;
        Ok(())
    }

    /// Delete a specific image record.
    pub fn delete_image(&self, directory_id: i64, filename: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM images WHERE directory_id = ?1 AND filename = ?2",
            params![directory_id, filename],
        )?;
        Ok(())
    }

    // ── Meta views ────────────────────────────────────────────────────────

    /// All bookmarked images, most recently bookmarked first.
    pub fn get_bookmarked(&self) -> Result<Vec<(String, ImageRecord)>> {
        let mut stmt = self.conn.prepare(
            "SELECT d.path,
                    i.id, i.directory_id, i.filename, i.file_size,
                    i.file_modified_at, i.rating, i.bookmarked, i.rotation, i.note,
                    i.created_at, i.updated_at
             FROM images i JOIN directories d ON i.directory_id = d.id
             WHERE i.bookmarked = 1
             ORDER BY i.updated_at DESC",
        )?;
        let result: Result<Vec<_>> = stmt.query_map([], |row| {
            let dir: String = row.get(0)?;
            Ok((dir, map_image_row(row, 1)?))
        })?.collect();
        result
    }

    /// All rated images, highest rating first.
    pub fn get_rated(&self) -> Result<Vec<(String, ImageRecord)>> {
        let mut stmt = self.conn.prepare(
            "SELECT d.path,
                    i.id, i.directory_id, i.filename, i.file_size,
                    i.file_modified_at, i.rating, i.bookmarked, i.rotation, i.note,
                    i.created_at, i.updated_at
             FROM images i JOIN directories d ON i.directory_id = d.id
             WHERE i.rating IS NOT NULL
             ORDER BY i.rating DESC, i.updated_at DESC",
        )?;
        let result: Result<Vec<_>> = stmt.query_map([], |row| {
            let dir: String = row.get(0)?;
            Ok((dir, map_image_row(row, 1)?))
        })?.collect();
        result
    }

    /// All rated images matching a specific filter.
    pub fn get_rated_filtered(&self, filter: &crate::session::RatingFilter) -> Result<Vec<(std::path::PathBuf, ImageRecord)>> {
        let op_sql = match filter.op {
            crate::session::RatingFilterOp::AtLeast => ">=",
            crate::session::RatingFilterOp::AtMost  => "<=",
            crate::session::RatingFilterOp::Exactly => "=",
        };

        let mut query = format!(
            "SELECT d.path,
                    i.id, i.directory_id, i.filename, i.file_size,
                    i.file_modified_at, i.rating, i.bookmarked, i.rotation, i.note,
                    i.created_at, i.updated_at
             FROM images i JOIN directories d ON i.directory_id = d.id
             WHERE i.rating {} ?1",
            op_sql
        );

        if filter.path_prefix.is_some() {
            query.push_str(" AND d.path LIKE ?2");
        }

        query.push_str(" ORDER BY i.rating DESC, i.updated_at DESC");

        let mut stmt = self.conn.prepare(&query)?;
        
        let result: Result<Vec<_>> = if let Some(prefix) = &filter.path_prefix {
            let prefix_str = prefix.to_string_lossy().to_string();
            let like_pattern = format!("{}%", prefix_str);
            stmt.query_map(rusqlite::params![filter.value, like_pattern], |row| {
                let dir: String = row.get(0)?;
                Ok((std::path::PathBuf::from(dir), map_image_row(row, 1)?))
            })?.collect()
        } else {
            stmt.query_map(rusqlite::params![filter.value], |row| {
                let dir: String = row.get(0)?;
                Ok((std::path::PathBuf::from(dir), map_image_row(row, 1)?))
            })?.collect()
        };

        result
    }

    // ── Utility queries ──────────────────────────────────────────────────

    /// Total number of image records across all directories.
    pub fn count_all_entries(&self) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM images", [], |row| row.get(0)
        )?;
        Ok(n as usize)
    }

    /// All image records joined with their directory path, for utility scans.
    /// Returns `(dir_id, dir_path, filename, rating)`.
    pub fn get_all_image_paths(&self) -> Result<Vec<(i64, String, String, Option<u8>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT i.directory_id, d.path, i.filename, i.rating
             FROM images i JOIN directories d ON i.directory_id = d.id
             ORDER BY d.path, i.filename",
        )?;
        let result: Result<Vec<_>> = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?.map(|r| r as u8),
            ))
        })?.collect();
        result
    }

    // ── Settings ─────────────────────────────────────────────────────────

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        query_opt(
            &self.conn,
            "SELECT value FROM settings WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Row mappers
// ---------------------------------------------------------------------------

fn map_dir_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DirectoryRecord> {
    Ok(DirectoryRecord {
        id:                    row.get(0)?,
        uuid:                  row.get(1)?,
        path:                  row.get(2)?,
        sort_override:         row.get(3)?,
        created_at:            row.get(4)?,
        path_last_verified_at: row.get(5)?,
    })
}

/// Map a row to `ImageRecord`, using `offset` to skip leading columns (e.g.
/// `d.path` in JOIN queries).
fn map_image_row(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<ImageRecord> {
    Ok(ImageRecord {
        id:               row.get(offset)?,
        directory_id:     row.get(offset + 1)?,
        filename:         row.get(offset + 2)?,
        file_size:        row.get(offset + 3)?,
        file_modified_at: row.get(offset + 4)?,
        rating:           row.get::<_, Option<i64>>(offset + 5)?.map(|r| r as u8),
        bookmarked:       row.get::<_, i64>(offset + 6)? != 0,
        rotation:         row.get::<_, i64>(offset + 7)? as u8,
        note:             row.get(offset + 8)?,
        created_at:       row.get(offset + 9)?,
        updated_at:       row.get(offset + 10)?,
    })
}

/// Run a query that returns zero or one rows.
fn query_opt<T, F>(
    conn:  &Connection,
    sql:   &str,
    params: impl rusqlite::Params,
    f:     F,
) -> Result<Option<T>>
where
    F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
{
    match conn.query_row(sql, params, f) {
        Ok(v)                                        => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows)    => Ok(None),
        Err(e)                                       => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::open_in_memory().expect("in-memory DB")
    }

    #[test]
    fn schema_initialises_cleanly() {
        db();
    }

    #[test]
    fn upsert_directory_creates_new_record() {
        let db  = db();
        let rec = db.upsert_directory_by_path("/photos").unwrap();
        assert_eq!(rec.path, "/photos");
        assert!(!rec.uuid.is_empty());
    }

    #[test]
    fn upsert_directory_is_idempotent() {
        let db  = db();
        let a   = db.upsert_directory_by_path("/photos").unwrap();
        let b   = db.upsert_directory_by_path("/photos").unwrap();
        assert_eq!(a.id,   b.id);
        assert_eq!(a.uuid, b.uuid);
    }

    #[test]
    fn set_and_get_rating() {
        let db  = db();
        let dir = db.upsert_directory_by_path("/photos").unwrap();
        db.set_rating(dir.id, "img.jpg", Some(4)).unwrap();
        let img = db.get_image(dir.id, "img.jpg").unwrap().unwrap();
        assert_eq!(img.rating, Some(4));
    }

    #[test]
    fn clearing_rating_gcs_otherwise_empty_record() {
        let db  = db();
        let dir = db.upsert_directory_by_path("/photos").unwrap();
        db.set_rating(dir.id, "img.jpg", Some(3)).unwrap();
        db.set_rating(dir.id, "img.jpg", None).unwrap();
        assert!(db.get_image(dir.id, "img.jpg").unwrap().is_none(),
                "record should be GC'd when it carries no data");
    }

    #[test]
    fn rating_and_bookmark_together_survive_gc() {
        let db  = db();
        let dir = db.upsert_directory_by_path("/photos").unwrap();
        db.set_rating(dir.id,   "img.jpg", Some(5)).unwrap();
        db.set_bookmark(dir.id, "img.jpg", true).unwrap();
        // Clearing rating alone should not delete the row (bookmark remains)
        db.set_rating(dir.id,   "img.jpg", None).unwrap();
        let img = db.get_image(dir.id, "img.jpg").unwrap();
        assert!(img.is_some(), "bookmark should keep the row alive");
        assert!(img.unwrap().bookmarked);
    }

    #[test]
    fn set_note_persists() {
        let db  = db();
        let dir = db.upsert_directory_by_path("/photos").unwrap();
        db.set_note(dir.id, "img.jpg", Some("keeper")).unwrap();
        let img = db.get_image(dir.id, "img.jpg").unwrap().unwrap();
        assert_eq!(img.note.as_deref(), Some("keeper"));
    }

    #[test]
    fn get_bookmarked_returns_only_bookmarked() {
        let db  = db();
        let dir = db.upsert_directory_by_path("/photos").unwrap();
        db.set_bookmark(dir.id, "a.jpg", true).unwrap();
        db.set_rating(dir.id,   "b.jpg", Some(5)).unwrap();  // rated, not bookmarked
        let results = db.get_bookmarked().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.filename, "a.jpg");
    }

    #[test]
    fn get_rated_returns_only_rated() {
        let db  = db();
        let dir = db.upsert_directory_by_path("/photos").unwrap();
        db.set_bookmark(dir.id, "a.jpg", true).unwrap();
        db.set_rating(dir.id,   "b.jpg", Some(3)).unwrap();
        let results = db.get_rated().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.filename, "b.jpg");
    }

    #[test]
    fn settings_round_trip() {
        let db = db();
        db.set_setting("theme", "dark").unwrap();
        assert_eq!(db.get_setting("theme").unwrap().as_deref(), Some("dark"));
    }

    #[test]
    fn settings_upsert_overwrites() {
        let db = db();
        db.set_setting("theme", "light").unwrap();
        db.set_setting("theme", "dark").unwrap();
        assert_eq!(db.get_setting("theme").unwrap().as_deref(), Some("dark"));
    }

    #[test]
    fn missing_setting_returns_none() {
        assert!(db().get_setting("no_such_key").unwrap().is_none());
    }

    #[test]
    fn set_and_get_rotation() {
        let db  = db();
        let dir = db.upsert_directory_by_path("/photos").unwrap();
        db.set_rotation(dir.id, "img.jpg", 1).unwrap();
        let img = db.get_image(dir.id, "img.jpg").unwrap().unwrap();
        assert_eq!(img.rotation, 1);
    }
}
