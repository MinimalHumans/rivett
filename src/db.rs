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
CREATE TABLE IF NOT EXISTS tags (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    name    TEXT    NOT NULL UNIQUE,
    color   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS image_tags (
...
pub struct TagRecord {
    pub id:    i64,
    pub name:  String,
    pub color: u32,
}

    image_id    INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
    tag_id      INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (image_id, tag_id)
);

CREATE TABLE IF NOT EXISTS settings (
    key     TEXT PRIMARY KEY,
    value   TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_images_dir      ON images(directory_id);
CREATE INDEX IF NOT EXISTS idx_dirs_uuid       ON directories(uuid);
CREATE INDEX IF NOT EXISTS idx_dirs_path       ON directories(path);
CREATE INDEX IF NOT EXISTS idx_tags_name       ON tags(name);
CREATE INDEX IF NOT EXISTS idx_image_tags_tag  ON image_tags(tag_id);
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRecord {
    pub id:    i64,
    pub name:  String,
    pub color: u32,
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

        // Migration: add color column to tags if missing
        let has_color = {
            let mut stmt = self.conn.prepare("PRAGMA table_info(tags)")?;
            let mut rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
            rows.any(|r| r.as_deref() == Ok("color"))
        };
        if !has_color {
            let _ = self.conn.execute("ALTER TABLE tags ADD COLUMN color INTEGER NOT NULL DEFAULT 0", []);
        }

        Ok(())
    }

    fn migrate(&self) -> Result<()> {
        // Add rotation column to images if missing
        let has_rotation = {
            let mut stmt = self.conn.prepare("PRAGMA table_info(images)")?;
            let mut rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
            rows.any(|r| r.as_deref() == Ok("rotation"))
        };
        if !has_rotation {
            let _ = self.conn.execute("ALTER TABLE images ADD COLUMN rotation INTEGER NOT NULL DEFAULT 0", []);
        }
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

    // ── Tags ─────────────────────────────────────────────────────────────

    pub fn get_all_tags(&self) -> Result<Vec<TagRecord>> {
        let mut stmt = self.conn.prepare("SELECT id, name, color FROM tags ORDER BY name")?;
        let tags: Result<Vec<TagRecord>> = stmt.query_map([], |row| Ok(TagRecord {
            id:    row.get(0)?,
            name:  row.get(1)?,
            color: row.get::<_, i64>(2)? as u32,
        }))?.collect();
        tags
    }

    pub fn get_image_tags(&self, directory_id: i64, filename: &str) -> Result<Vec<TagRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.name, t.color FROM tags t
             JOIN image_tags it ON t.id = it.tag_id
             JOIN images i ON it.image_id = i.id
             WHERE i.directory_id = ?1 AND i.filename = ?2
             ORDER BY t.name",
        )?;
        let tags: Result<Vec<TagRecord>> = stmt.query_map(params![directory_id, filename], |row| Ok(TagRecord {
            id:    row.get(0)?,
            name:  row.get(1)?,
            color: row.get::<_, i64>(2)? as u32,
        }))?.collect();
        tags
    }

    pub fn set_image_tags(&self, directory_id: i64, filename: &str, tag_names: &[String]) -> Result<()> {
        self.ensure_image_exists(directory_id, filename)?;
        let image_id: i64 = self.conn.query_row(
            "SELECT id FROM images WHERE directory_id = ?1 AND filename = ?2",
            params![directory_id, filename],
            |row| row.get(0),
        )?;

        self.conn.execute("DELETE FROM image_tags WHERE image_id = ?1", params![image_id])?;

        for tag_name in tag_names {
            let tag_name = tag_name.trim();
            if tag_name.is_empty() { continue; }

            // Check if tag exists, otherwise create with color
            let existing_id: Result<Option<i64>> = self.conn.query_row(
                "SELECT id FROM tags WHERE name = ?1",
                params![tag_name],
                |row| Ok(Some(row.get(0)?)),
            ).or_else(|e| if matches!(e, rusqlite::Error::QueryReturnedNoRows) { Ok(None) } else { Err(e) });

            let tag_id = if let Some(id) = existing_id? {
                id
            } else {
                self.conn.execute(
                    "INSERT INTO tags (name, color) VALUES (?1, 0)",
                    params![tag_name],
                )?;
                let id = self.conn.last_insert_rowid();
                let color = color_from_id(id as u32);
                self.conn.execute(
                    "UPDATE tags SET color = ?1 WHERE id = ?2",
                    params![color as i64, id],
                )?;
                id
            };

            self.conn.execute(
                "INSERT OR IGNORE INTO image_tags (image_id, tag_id) VALUES (?1, ?2)",
                params![image_id, tag_id],
            )?;
        }

        self.conn.execute(
            "UPDATE images SET updated_at = ?1 WHERE id = ?2",
            params![Self::now(), image_id],
        )?;

        self.gc_empty_record(directory_id, filename)
    }

    pub fn update_tag_color(&self, tag_id: i64, color: u32) -> Result<()> {
        self.conn.execute(
            "UPDATE tags SET color = ?1 WHERE id = ?2",
            params![color as i64, tag_id],
        )?;
        Ok(())
    }

    pub fn delete_tag(&self, name: &str) -> Result<()> {
        self.conn.execute("DELETE FROM tags WHERE name = ?1", params![name])?;
        Ok(())
    }

    pub fn rename_tag(&self, old_name: &str, new_name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE tags SET name = ?1 WHERE name = ?2",
            params![new_name, old_name],
        )?;
        Ok(())
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
               AND (note IS NULL OR note = '')
               AND NOT EXISTS (SELECT 1 FROM image_tags WHERE image_id = images.id)",
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
}

// ── Color Helpers ────────────────────────────────────────────────────────────

pub fn color_from_id(id: u32) -> u32 {
    let hue = (id as f32 * 137.508) % 360.0; // golden angle in degrees
    let s = 0.55_f32;
    let v = 0.80_f32;
    let color = hsv_to_rgb(hue, s, v);
    // Store as 0xRRGGBB
    ((color[0] as u32) << 16) | ((color[1] as u32) << 8) | (color[2] as u32)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [u8; 3] {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h as u32 {
        0..=59   => (c, x, 0.0),
        60..=119 => (x, c, 0.0),
        120..=179 => (0.0, c, x),
        180..=239 => (0.0, x, c),
        240..=299 => (x, 0.0, c),
        _         => (c, 0.0, x),
    };
    [
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    ]
}

impl Database {

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

    /// All images matching specific rating and/or tag filters.
    pub fn get_images_filtered(
        &self, 
        rating_filter: Option<&crate::session::RatingFilter>,
        tag_filter:    &crate::session::TagFilter,
    ) -> Result<Vec<(std::path::PathBuf, ImageRecord)>> {
        let mut query = String::from(
            "SELECT d.path,
                    i.id, i.directory_id, i.filename, i.file_size,
                    i.file_modified_at, i.rating, i.bookmarked, i.rotation, i.note,
                    i.created_at, i.updated_at
             FROM images i JOIN directories d ON i.directory_id = d.id"
        );

        let mut where_clauses = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(rf) = rating_filter {
            let op_sql = match rf.op {
                crate::session::RatingFilterOp::AtLeast => ">=",
                crate::session::RatingFilterOp::AtMost  => "<=",
                crate::session::RatingFilterOp::Exactly => "=",
            };
            where_clauses.push(format!("i.rating {} ?{}", op_sql, params.len() + 1));
            params.push(Box::new(rf.value as i64));

            if let Some(prefix) = &rf.path_prefix {
                where_clauses.push(format!("d.path LIKE ?{}", params.len() + 1));
                params.push(Box::new(format!("{}%", prefix.to_string_lossy())));
            }
        }

        if !tag_filter.is_empty() {
            for (idx, tag) in tag_filter.tags.iter().enumerate() {
                let it_alias = format!("it{}", idx);
                let t_alias = format!("t{}", idx);
                query.push_str(&format!(
                    " JOIN image_tags {} ON i.id = {}.image_id
                      JOIN tags {} ON {}.tag_id = {}.id AND {}.name = ?{}",
                    it_alias, it_alias, t_alias, it_alias, t_alias, t_alias, params.len() + 1
                ));
                params.push(Box::new(tag.clone()));
            }
        }

        if !where_clauses.is_empty() {
            query.push_str(" WHERE ");
            query.push_str(&where_clauses.join(" AND "));
        }

        query.push_str(" ORDER BY i.updated_at DESC");

        let mut stmt = self.conn.prepare(&query)?;
        
        let p_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let result: Result<Vec<_>> = stmt.query_map(p_refs.as_slice(), |row| {
            let dir: String = row.get(0)?;
            Ok((std::path::PathBuf::from(dir), map_image_row(row, 1)?))
        })?.collect();

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
