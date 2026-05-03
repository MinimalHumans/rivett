//! Directory scanning, sort management, and image decoding.
//!
//! [`DirectoryListing`] owns the ordered list of image paths and a cursor.
//! Navigation never wraps: the list has a hard start and end, matching the
//! spec.
//!
//! [`load_image`] is a thin wrapper around `image::ImageReader` that returns a
//! descriptive error string instead of an `image::ImageError`.

use std::path::{Path, PathBuf};

use crate::formats::SupportedFormat;
use crate::settings::SortOrder;

// ---------------------------------------------------------------------------
// DirectoryListing
// ---------------------------------------------------------------------------

/// Sorted list of supported image files in a single directory, with a cursor.
#[derive(Debug, Default)]
pub struct DirectoryListing {
    pub dir_path:      PathBuf,
    pub files:         Vec<PathBuf>,
    pub current_index: usize,
    /// When set, only images matching this filter (as found in the database) are shown.
    pub rating_filter: Option<crate::session::RatingFilter>,
}

impl DirectoryListing {
    /// Scan `dir` for supported image files and sort according to `order`.
    pub fn scan(
        dir:    &Path,
        order:  SortOrder,
        filter: Option<crate::session::RatingFilter>,
        db:     Option<&crate::db::Database>,
    ) -> std::io::Result<Self> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file() && SupportedFormat::from_path(p).is_some())
            .collect();

        if let Some(db) = db {
            let dir_str = dir.to_string_lossy();
            if let Ok(Some(d_rec)) = db.find_directory_by_path(&dir_str) {
                // Prune records for files that no longer exist
                let _ = Self::prune_missing_images(db, d_rec.id, &files);

                if let Some(f) = filter {
                    files.retain(|p| {
                        let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                        if let Ok(Some(img_rec)) = db.get_image(d_rec.id, fname) {
                            f.matches(img_rec.rating)
                        } else {
                            false
                        }
                    });
                }
            } else if filter.is_some() {
                files.clear();
            }
        }

        sort_paths(&mut files, order);

        Ok(Self {
            dir_path: dir.to_path_buf(),
            files,
            current_index: 0,
            rating_filter: filter,
        })
    }

    /// Re-scan the directory in-place, preserving cursor position where possible.
    pub fn refresh(
        &mut self,
        order: SortOrder,
        db:    Option<&crate::db::Database>,
    ) -> std::io::Result<()> {
        let current_file = self.current().cloned();
        let fresh = Self::scan(&self.dir_path, order, self.rating_filter, db)?;
        *self = fresh;
        if let Some(path) = current_file {
            self.seek_to(&path);
        }
        Ok(())
    }

    fn prune_missing_images(db: &crate::db::Database, directory_id: i64, current_files: &[PathBuf]) -> rusqlite::Result<()> {
        let db_images = db.get_images(directory_id)?;
        let disk_names: std::collections::HashSet<_> = current_files.iter()
            .filter_map(|p| p.file_name()?.to_str())
            .collect();

        for img in db_images {
            if !disk_names.contains(img.filename.as_str()) {
                db.delete_image(directory_id, &img.filename)?;
            }
        }
        Ok(())
    }


    /// Create a listing of all rated images across the library that match `filter`.
    pub fn scan_global(
        db:     &crate::db::Database,
        filter: crate::session::RatingFilter,
    ) -> std::io::Result<Self> {
        let records = db.get_rated_filtered(filter)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        
        let mut files = Vec::new();
        for (dir, rec) in records {
            let path = dir.join(&rec.filename);
            if path.exists() {
                files.push(path);
            } else {
                // Quietly prune missing file from DB
                let _ = db.delete_image(rec.directory_id, &rec.filename);
            }
        }

        Ok(Self {
            dir_path: PathBuf::new(), // Special empty path for global view
            files,
            current_index: 0,
            rating_filter: Some(filter),
        })
    }

    /// Move the cursor to `target`. Returns `false` if it is not in the list.
    pub fn seek_to(&mut self, target: &Path) -> bool {
        if let Some(idx) = self.files.iter().position(|p| p == target) {
            self.current_index = idx;
            true
        } else {
            false
        }
    }

    pub fn go_next(&mut self) -> bool {
        if self.can_go_next() {
            self.current_index += 1;
            true
        } else {
            false
        }
    }

    pub fn go_prev(&mut self) -> bool {
        if self.can_go_prev() {
            self.current_index -= 1;
            true
        } else {
            false
        }
    }

    pub fn can_go_next(&self) -> bool {
        !self.files.is_empty() && self.current_index < self.files.len() - 1
    }

    pub fn can_go_prev(&self) -> bool {
        self.current_index > 0
    }

    pub fn current(&self) -> Option<&PathBuf> {
        self.files.get(self.current_index)
    }

    pub fn go_to_first(&mut self) { self.current_index = 0; }
    pub fn go_to_last(&mut self)  { self.current_index = self.files.len().saturating_sub(1); }

    pub fn len(&self) -> usize { self.files.len() }
    pub fn is_empty(&self) -> bool { self.files.is_empty() }

    /// Human-readable cursor position, e.g. "4 / 20".
    pub fn position_label(&self) -> String {
        if self.files.is_empty() {
            "0 / 0".to_string()
        } else {
            format!("{} / {}", self.current_index + 1, self.files.len())
        }
    }
}

// ---------------------------------------------------------------------------
// Image loading
// ---------------------------------------------------------------------------

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::thread;
use std::sync::mpsc::{self, Receiver, Sender};

/// A fully decoded RGBA image ready for upload to the GPU.
#[derive(Clone)]
pub struct DecodedImage {
    pub rgba:      Vec<u8>,
    pub width:     u32,
    pub height:    u32,
    /// Luminance histogram (256 bins).
    pub histogram: Vec<f32>,
}

impl DecodedImage {
    pub fn new(rgba: Vec<u8>, width: u32, height: u32) -> Self {
        let mut hist = vec![0u32; 256];
        for chunk in rgba.chunks_exact(4) {
            // Simple luminance: 0.299R + 0.587G + 0.114B
            let l = (chunk[0] as f32 * 0.299 + chunk[1] as f32 * 0.587 + chunk[2] as f32 * 0.114) as usize;
            hist[l.min(255)] += 1;
        }
        
        // Normalize histogram
        let max = *hist.iter().max().unwrap_or(&1) as f32;
        let histogram = hist.into_iter().map(|v| v as f32 / max).collect();

        Self { rgba, width, height, histogram }
    }
}

/// Decode `path` into a [`DecodedImage`].
pub fn load_image(path: &Path) -> Result<DecodedImage, String> {
    // Special handling for SVG
    let ext = path.extension().and_then(|s| s.to_str()).map(|s| s.to_ascii_lowercase());
    if ext == Some("svg".to_string()) {
        return load_svg(path);
    }

    // Special handling for RAW
    if let Some(SupportedFormat::Raw) = SupportedFormat::from_path(path) {
        return load_raw(path);
    }

    let file = std::fs::File::open(path)
        .map_err(|e| format!("could not open {}: {e}", path.display()))?;
    let reader = std::io::BufReader::new(file);
    
    let img_reader = image::ImageReader::new(reader)
        .with_guessed_format()
        .map_err(|e| format!("could not determine format for {}: {e}", path.display()))?;
        
    let mut img = img_reader.decode()
        .map_err(|e| format!("could not decode {}: {e}", path.display()))?;
    
    // Apply EXIF orientation if applicable
    if let Some(orientation) = crate::metadata::get_orientation(path) {
        img = match orientation {
            2 => img.fliph(),
            3 => img.rotate180(),
            4 => img.flipv(),
            5 => img.rotate90().fliph(),
            6 => img.rotate90(),
            7 => img.rotate270().fliph(),
            8 => img.rotate270(),
            _ => img,
        };
    }

    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(DecodedImage::new(rgba.into_raw(), width, height))
}

fn load_raw(path: &Path) -> Result<DecodedImage, String> {
    let ext = path.extension().and_then(|s| s.to_str()).map(|s| s.to_ascii_lowercase());
    
    // Special handling for modern Canon .CR3 (ISO BMFF container)
    if ext == Some("cr3".to_string()) {
        return load_cr3(path);
    }

    // Standard RAW formats via rawloader, with image-crate fallback for
    // formats rawloader rejects (e.g. lossy-JPEG DNG, compression 34892).
    let raw = match rawloader::decode_file(path) {
        Ok(r) => r,
        Err(raw_err) => {
            // Try the image crate; it handles more TIFF/DNG compression variants.
            let file = std::fs::File::open(path)
                .map_err(|e| format!("could not open {}: {e}", path.display()))?;
            let reader = std::io::BufReader::new(file);
            match image::ImageReader::new(reader)
                .with_guessed_format()
                .map_err(|e| e.to_string())
                .and_then(|r| r.decode().map_err(|e| e.to_string()))
            {
                Ok(img) => {
                    let mut img = img;
                    if let Some(orientation) = crate::metadata::get_orientation(path) {
                        img = apply_orientation_to_image(img, orientation);
                    }
                    let rgba = img.to_rgba8();
                    let (w, h) = rgba.dimensions();
                    return Ok(DecodedImage::new(rgba.into_raw(), w, h));
                }
                Err(_) => return Err(format!("rawloader decode failed: {raw_err:?}")),
            }
        }
    };
    
    let width  = raw.width;
    let height = raw.height;
    
    match &raw.data {
        rawloader::RawImageData::Integer(data) => {
            let mut rgba = Vec::with_capacity(width * height * 4);
            // Simple preview conversion for rawloader formats
            if data.len() >= width * height * 3 {
                for chunk in data.chunks_exact(3) {
                    rgba.push((chunk[0] >> 8) as u8);
                    rgba.push((chunk[1] >> 8) as u8);
                    rgba.push((chunk[2] >> 8) as u8);
                    rgba.push(255);
                }
                
                let mut img = image::DynamicImage::ImageRgba8(
                    image::ImageBuffer::from_raw(width as u32, height as u32, rgba).unwrap()
                );
                
                if let Some(orientation) = crate::metadata::get_orientation(path) {
                    img = apply_orientation_to_image(img, orientation);
                }

                let rgba = img.to_rgba8();
                let (width, height) = rgba.dimensions();
                Ok(DecodedImage::new(rgba.into_raw(), width, height))
            } else {
                Err("Raw sensor data requires debayering (not yet implemented)".to_string())
            }
        }
        _ => Err("Unsupported raw data format (non-integer)".to_string()),
    }
}

fn load_cr3(path: &Path) -> Result<DecodedImage, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    
    let mut best_match = None;
    let mut search_pos = 0;
    
    while let Some(pos) = find_subsequence(&data[search_pos..], &[0xFF, 0xD8, 0xFF]) {
        let start = search_pos + pos;
        if let Some(end_pos) = find_subsequence(&data[start..], &[0xFF, 0xD9]) {
            let end = start + end_pos + 2;
            let len = end - start;
            if len > best_match.map(|(s, e)| e - s).unwrap_or(0) {
                best_match = Some((start, end));
            }
        }
        search_pos = start + 3;
        if search_pos > data.len() - 3 { break; }
    }

    if let Some((start, end)) = best_match {
        let jpeg_bytes = &data[start..end];
        let mut img = image::load_from_memory(jpeg_bytes)
            .map_err(|e| format!("failed to decode extracted CR3 preview: {e}"))?;
        
        // Try extracting orientation from the embedded JPEG bytes first.
        // If missing (common in .CR3), fallback to the main file's deep metadata.
        let orientation = crate::metadata::get_orientation_from_bytes(jpeg_bytes)
            .or_else(|| crate::metadata::get_orientation(path));

        if let Some(orientation) = orientation {
            img = apply_orientation_to_image(img, orientation);
        }

        let rgba = img.to_rgba8();
        let (width, height) = rgba.dimensions();
        Ok(DecodedImage::new(rgba.into_raw(), width, height))
    } else {
        Err("Could not find embedded JPEG preview in .CR3 file".to_string())
    }
}

fn apply_orientation_to_image(img: image::DynamicImage, orientation: u32) -> image::DynamicImage {
    match orientation {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img,
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}



fn load_svg(path: &Path) -> Result<DecodedImage, String> {
    let opt = resvg::usvg::Options::default();
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let tree = resvg::usvg::Tree::from_data(&data, &opt).map_err(|e| e.to_string())?;
    
    let pixmap_size = tree.size().to_int_size();
    let mut pixmap = resvg::tiny_skia::Pixmap::new(pixmap_size.width(), pixmap_size.height())
        .ok_or("Failed to create pixmap")?;
    
    resvg::render(&tree, resvg::tiny_skia::Transform::default(), &mut pixmap.as_mut());
    
    Ok(DecodedImage::new(pixmap.take(), pixmap_size.width(), pixmap_size.height()))
}

/// Simple LRU cache for decoded images.
pub struct ImageCache {
    /// Maps path to decoded image.
    images: HashMap<PathBuf, DecodedImage>,
    /// Order of access for LRU eviction.
    order:  VecDeque<PathBuf>,
    /// Maximum number of images to keep in memory.
    capacity: usize,

    /// Background loading channel
    tx: Sender<(PathBuf, DecodedImage)>,
    rx: Receiver<(PathBuf, DecodedImage)>,
    /// Paths currently being loaded in the background.
    pending: Arc<Mutex<HashMap<PathBuf, thread::JoinHandle<()>>>>,
}

impl ImageCache {
    pub fn new(capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            images: HashMap::new(),
            order:  VecDeque::new(),
            capacity,
            tx,
            rx,
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn get(&mut self, path: &PathBuf) -> Option<&DecodedImage> {
        if self.images.contains_key(path) {
            // Move to front of LRU
            self.order.retain(|p| p != path);
            self.order.push_back(path.clone());
            self.images.get(path)
        } else {
            None
        }
    }

    pub fn insert(&mut self, path: PathBuf, image: DecodedImage) {
        if self.images.contains_key(&path) {
            return;
        }
        if self.images.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.images.remove(&oldest);
            }
        }
        self.order.push_back(path.clone());
        self.images.insert(path, image);
    }

    /// Evict an entry so the next access reloads from disk.
    pub fn remove(&mut self, path: &PathBuf) {
        self.images.remove(path);
        self.order.retain(|p| p != path);
    }

    /// Check for any images that finished loading in the background.
    pub fn poll(&mut self) {
        while let Ok((path, img)) = self.rx.try_recv() {
            self.insert(path.clone(), img);
            if let Ok(mut pending) = self.pending.lock() {
                pending.remove(&path);
            }
        }
    }

    /// Start loading an image in a background thread if it's not already cached or pending.
    pub fn prefetch(&self, path: PathBuf) {
        if self.images.contains_key(&path) {
            return;
        }

        let mut pending = self.pending.lock().unwrap();
        if pending.contains_key(&path) {
            return;
        }

        let tx = self.tx.clone();
        let p = path.clone();
        let handle = thread::spawn(move || {
            if let Ok(img) = load_image(&p) {
                let _ = tx.send((p, img));
            }
        });
        pending.insert(path, handle);
    }
}



// ---------------------------------------------------------------------------
// Sorting
// ---------------------------------------------------------------------------

fn sort_paths(files: &mut [PathBuf], order: SortOrder) {
    match order {
        SortOrder::Name => {
            files.sort_by(|a, b| {
                let a = a.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let b = b.file_name().and_then(|n| n.to_str()).unwrap_or("");
                a.cmp(b)
            });
        }
        SortOrder::DateModified => {
            files.sort_by_key(|p| {
                p.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH)
            });
        }
        SortOrder::FileSize => {
            files.sort_by_key(|p| p.metadata().map(|m| m.len()).unwrap_or(0));
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // Create a temp directory with known image filenames.
    fn make_dir(names: &[&str]) -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        for name in names {
            fs::write(dir.path().join(name), b"").unwrap();
        }
        dir
    }

    #[test]
    fn scan_finds_supported_extensions() {
        let dir     = make_dir(&["b.png", "a.jpg", "c.bmp", "skip.txt"]);
        let listing = DirectoryListing::scan(dir.path(), SortOrder::Name, None, None).unwrap();
        assert_eq!(listing.len(), 3, "txt should be excluded");
    }

    #[test]
    fn scan_sorts_by_name_ascending() {
        let dir     = make_dir(&["c.gif", "a.png", "b.jpg"]);
        let listing = DirectoryListing::scan(dir.path(), SortOrder::Name, None, None).unwrap();
        let names: Vec<_> = listing.files.iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, vec!["a.png", "b.jpg", "c.gif"]);
    }

    #[test]
    fn navigation_does_not_wrap_at_end() {
        let dir     = make_dir(&["a.png", "b.png", "c.png"]);
        let mut l   = DirectoryListing::scan(dir.path(), SortOrder::Name, None, None).unwrap();
        while l.go_next() {}
        assert!(!l.can_go_next());
        assert!(l.can_go_prev());
        assert!(!l.go_next(), "go_next at end must return false");
    }

    #[test]
    fn navigation_does_not_wrap_at_start() {
        let dir   = make_dir(&["a.png", "b.png"]);
        let mut l = DirectoryListing::scan(dir.path(), SortOrder::Name, None, None).unwrap();
        assert!(!l.can_go_prev());
        assert!(!l.go_prev(), "go_prev at start must return false");
        assert_eq!(l.current_index, 0);
    }

    #[test]
    fn seek_to_positions_cursor_correctly() {
        let dir     = make_dir(&["a.png", "b.png", "c.png"]);
        let mut l   = DirectoryListing::scan(dir.path(), SortOrder::Name, None, None).unwrap();
        let target  = dir.path().join("b.png");
        assert!(l.seek_to(&target));
        assert_eq!(l.current_index, 1);
    }

    #[test]
    fn seek_to_unknown_returns_false() {
        let dir   = make_dir(&["a.png"]);
        let mut l = DirectoryListing::scan(dir.path(), SortOrder::Name, None, None).unwrap();
        assert!(!l.seek_to(&dir.path().join("nonexistent.png")));
        assert_eq!(l.current_index, 0, "cursor should be unchanged");
    }

    #[test]
    fn empty_directory_listing() {
        let dir   = make_dir(&["readme.txt"]);
        let l     = DirectoryListing::scan(dir.path(), SortOrder::Name, None, None).unwrap();
        assert!(l.is_empty());
        assert!(l.current().is_none());
        assert!(!l.can_go_next());
        assert!(!l.can_go_prev());
    }

    #[test]
    fn position_label_is_1_based() {
        let dir   = make_dir(&["a.png", "b.png", "c.png"]);
        let mut l = DirectoryListing::scan(dir.path(), SortOrder::Name, None, None).unwrap();
        assert_eq!(l.position_label(), "1 / 3");
        l.go_next();
        assert_eq!(l.position_label(), "2 / 3");
    }

    #[test]
    fn refresh_restores_cursor_to_same_file() {
        let dir    = make_dir(&["a.png", "b.png", "c.png"]);
        let mut l  = DirectoryListing::scan(dir.path(), SortOrder::Name, None, None).unwrap();
        l.seek_to(&dir.path().join("b.png"));
        l.refresh(SortOrder::Name, None).unwrap();
        assert_eq!(
            l.current().unwrap().file_name().unwrap(),
            "b.png",
        );
    }
}
