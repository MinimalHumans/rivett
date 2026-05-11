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
                            f.matches(p, img_rec.rating)
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

        let fresh = if self.dir_path.as_os_str().is_empty() {
            // Global/Recursive view
            if let (Some(db), Some(filter)) = (db, &self.rating_filter) {
                Self::scan_global(db, filter)?
            } else {
                return Err(std::io::Error::new(std::io::ErrorKind::Other, "Cannot refresh global view without DB or filter"));
            }
        } else {
            Self::scan(&self.dir_path, order, self.rating_filter.clone(), db)?
        };

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
        filter: &crate::session::RatingFilter,
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
            rating_filter: Some(filter.clone()),
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

/// Histograms for each color channel.
#[derive(Clone)]
pub struct Histograms {
    pub r: Vec<f32>,
    pub g: Vec<f32>,
    pub b: Vec<f32>,
}

/// A fully decoded RGBA image ready for upload to the GPU.
#[derive(Clone)]
pub struct DecodedImage {
    pub rgba:       Vec<f32>,
    pub width:      u32,
    pub height:     u32,
    pub histograms: Histograms,
}

impl DecodedImage {
    pub fn new(rgba: Vec<f32>, width: u32, height: u32) -> Self {
        let mut hist_r = vec![0u32; 256];
        let mut hist_g = vec![0u32; 256];
        let mut hist_b = vec![0u32; 256];

        for chunk in rgba.chunks_exact(4) {
            let r = (chunk[0].clamp(0.0, 1.0) * 255.0) as usize;
            let g = (chunk[1].clamp(0.0, 1.0) * 255.0) as usize;
            let b = (chunk[2].clamp(0.0, 1.0) * 255.0) as usize;

            hist_r[r.min(255)] += 1;
            hist_g[g.min(255)] += 1;
            hist_b[b.min(255)] += 1;
        }
        
        // Normalize histograms
        let max_r = (*hist_r.iter().max().unwrap_or(&1)).max(1) as f32;
        let max_g = (*hist_g.iter().max().unwrap_or(&1)).max(1) as f32;
        let max_b = (*hist_b.iter().max().unwrap_or(&1)).max(1) as f32;

        // We can normalize each channel individually or by the global max.
        // Usually, individual normalization is better for visibility of each channel.
        let histograms = Histograms {
            r: hist_r.into_iter().map(|v| v as f32 / max_r).collect(),
            g: hist_g.into_iter().map(|v| v as f32 / max_g).collect(),
            b: hist_b.into_iter().map(|v| v as f32 / max_b).collect(),
        };

        Self { rgba, width, height, histograms }
    }

    pub fn new_from_u8(rgba: Vec<u8>, width: u32, height: u32) -> Self {
        let f32_rgba = rgba.into_iter().map(|v| v as f32 / 255.0).collect();
        Self::new(f32_rgba, width, height)
    }
}

/// Decode `path` into a [`DecodedImage`].
pub fn load_image(path: &Path) -> Result<DecodedImage, String> {
    let fmt = SupportedFormat::from_path(path);
    
    // Special handling for SVG
    if let Some(SupportedFormat::Svg) = fmt {
        return load_svg(path);
    }

    // Special handling for RAW
    if let Some(SupportedFormat::Raw) = fmt {
        return load_raw(path);
    }

    // Standard image formats via the `image` crate
    let res = (|| {
        let file = std::fs::File::open(path)
            .map_err(|e| format!("could not open {}: {e}", path.display()))?;
        let reader = std::io::BufReader::new(file);
        let img_reader = image::ImageReader::new(reader)
            .with_guessed_format()
            .map_err(|e| format!("could not determine format for {}: {e}", path.display()))?;
        img_reader.decode()
            .map_err(|e| format!("{e}"))
    })();

    match res {
        Ok(mut img) => {
            if let Some(orientation) = crate::metadata::get_orientation(path) {
                img = apply_orientation_to_image(img, orientation);
            }
            if let Some(SupportedFormat::Exr) = fmt {
                let rgba = img.to_rgba32f();
                let (width, height) = rgba.dimensions();
                Ok(DecodedImage::new(rgba.into_raw(), width, height))
            } else {
                let rgba = img.to_rgba8();
                let (width, height) = rgba.dimensions();
                Ok(DecodedImage::new_from_u8(rgba.into_raw(), width, height))
            }
        }
        Err(e) => {
            // Fallback for TIFFs or other formats that might have embedded JPEGs
            // or require specialized decoding (like tiled DNG/TIFF).
            if let Some(SupportedFormat::Tiff) = fmt {
                if let Ok(decoded) = load_raw(path) {
                    return Ok(decoded);
                }
            }
            // Last resort: deep search for ANY embedded JPEG markers.
            // This is useful for legacy TIFFs (e.g. Fax3) that might have a thumbnail.
            if let Ok(decoded) = load_any_embedded_jpeg(path) {
                return Ok(decoded);
            }
            
            Err(format!("could not decode {}: {e}", path.display()))
        }
    }
}

fn load_raw(path: &Path) -> Result<DecodedImage, String> {
    let ext = path.extension().and_then(|s| s.to_str()).map(|s| s.to_ascii_lowercase());
    
    // Special handling for modern Canon .CR3 (ISO BMFF container)
    if ext == Some("cr3".to_string()) {
        return load_any_embedded_jpeg(path);
    }

    // Standard RAW formats via rawloader
    match rawloader::decode_file(path) {
        Ok(raw) => {
            let width  = raw.width;
            let height = raw.height;
            match &raw.data {
                rawloader::RawImageData::Integer(data) => {
                    let mut rgba = Vec::with_capacity(width * height * 4);
                    if data.len() >= width * height * 3 {
                        for chunk in data.chunks_exact(3) {
                            rgba.push(chunk[0] as f32 / 65535.0);
                            rgba.push(chunk[1] as f32 / 65535.0);
                            rgba.push(chunk[2] as f32 / 65535.0);
                            rgba.push(1.0);
                        }
                        // Note: rawloader Integer data is typically 16-bit.
                        // We still use apply_orientation_to_image via a temporary DynamicImage
                        // if needed, but for now we'll just handle rotation in ViewerState if possible.
                        // Actually, let's keep the existing rotation logic by converting to f32 after rotation
                        // or rotating f32.
                        
                        // For simplicity, let's rotate the data if needed.
                        let mut final_rgba = rgba;
                        let mut final_w = width as u32;
                        let mut final_h = height as u32;

                        if let Some(orientation) = crate::metadata::get_orientation(path) {
                            // Temporary conversion to ImageBuffer<Rgba<f32>> to use image crate rotation
                            if let Some(buffer) = image::ImageBuffer::<image::Rgba<f32>, Vec<f32>>::from_raw(final_w, final_h, final_rgba) {
                                let mut dynamic_f32 = image::DynamicImage::ImageRgba32F(buffer);
                                dynamic_f32 = apply_orientation_to_image(dynamic_f32, orientation);
                                let rotated_rgba = dynamic_f32.to_rgba32f();
                                final_w = rotated_rgba.width();
                                final_h = rotated_rgba.height();
                                final_rgba = rotated_rgba.into_raw();
                            }
                        }

                        return Ok(DecodedImage::new(final_rgba, final_w, final_h));
                    }
                    Err("Raw sensor data requires debayering (not yet implemented)".to_string())
                }
                _ => Err("Unsupported raw data format (non-integer)".to_string()),
            }
        }
        Err(raw_err) => {
            // Fallback 1: Try the image crate directly (handles some TIFF/DNG variants rawloader rejects)
            let res = (|| {
                let file = std::fs::File::open(path).ok()?;
                let reader = std::io::BufReader::new(file);
                image::ImageReader::new(reader).with_guessed_format().ok()?.decode().ok()
            })();

            if let Some(mut img) = res {
                if let Some(orientation) = crate::metadata::get_orientation(path) {
                    img = apply_orientation_to_image(img, orientation);
                }
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                return Ok(DecodedImage::new_from_u8(rgba.into_raw(), w, h));
            }

            // Fallback 2: Parse TIFF/DNG IFDs manually and extract JPEG tiles.
            // This handles compression 34892 (lossy-JPEG DNG, e.g. Google Pixel Enhanced).
            if let Ok(decoded) = load_dng_jpeg_tiles(path) {
                return Ok(decoded);
            }

            // Fallback 3: Generic JPEG search
            if let Ok(decoded) = load_any_embedded_jpeg(path) {
                return Ok(decoded);
            }

            Err(format!("rawloader decode failed: {raw_err:?}"))
        }
    }
}

/// Generic fallback that scans a file for JPEG markers and returns the largest valid one.
/// Useful for RAW files and legacy TIFFs that include an embedded preview/thumbnail.
fn load_any_embedded_jpeg(path: &Path) -> Result<DecodedImage, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    
    let mut best_match = None;
    let mut search_pos = 0;
    
    // Scan for JPEG SOI marker: FF D8 FF
    while let Some(pos) = find_subsequence(&data[search_pos..], &[0xFF, 0xD8, 0xFF]) {
        let start = search_pos + pos;
        // Scan for JPEG EOI marker: FF D9
        if let Some(end_pos) = find_subsequence(&data[start..], &[0xFF, 0xD9]) {
            let end = start + end_pos + 2;
            let len = end - start;
            // Heuristic: the largest JPEG in the file is probably the high-res preview we want.
            if len > best_match.map(|(s, e)| e - s).unwrap_or(0) {
                best_match = Some((start, end));
            }
        }
        search_pos = start + 3;
        if search_pos > data.len().saturating_sub(3) { break; }
    }

    if let Some((start, end)) = best_match {
        let jpeg_bytes = &data[start..end];
        let mut img = image::load_from_memory(jpeg_bytes)
            .map_err(|e| format!("failed to decode extracted preview: {e}"))?;
        
        // Try extracting orientation from the embedded JPEG bytes first, then main file metadata.
        let orientation = crate::metadata::get_orientation_from_bytes(jpeg_bytes)
            .or_else(|| crate::metadata::get_orientation(path));

        if let Some(orientation) = orientation {
            img = apply_orientation_to_image(img, orientation);
        }

        let rgba = img.to_rgba8();
        let (width, height) = rgba.dimensions();
        Ok(DecodedImage::new_from_u8(rgba.into_raw(), width, height))
    } else {
        Err("Could not find any embedded JPEG preview in file".to_string())
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
    
    Ok(DecodedImage::new_from_u8(pixmap.take(), pixmap_size.width(), pixmap_size.height()))
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
// DNG lossy-JPEG fallback decoder
//
// Handles DNG files that use compression 34892 (lossy JPEG tiles — e.g. Google
// Pixel "Enhanced" DNGs).  Neither rawloader nor the image/tiff crate decode
// this format; we parse the TIFF IFD chain ourselves to locate each JPEG
// strip/tile and decode it with image::load_from_memory_with_format.
// ---------------------------------------------------------------------------

fn tiff_r16(data: &[u8], off: usize, le: bool) -> u16 {
    if off + 2 > data.len() { return 0; }
    if le { u16::from_le_bytes([data[off], data[off+1]]) }
    else  { u16::from_be_bytes([data[off], data[off+1]]) }
}

fn tiff_r32(data: &[u8], off: usize, le: bool) -> u32 {
    if off + 4 > data.len() { return 0; }
    if le { u32::from_le_bytes([data[off], data[off+1], data[off+2], data[off+3]]) }
    else  { u32::from_be_bytes([data[off], data[off+1], data[off+2], data[off+3]]) }
}

fn tiff_scalar(data: &[u8], le: bool, typ: u16, vpos: usize) -> u32 {
    match typ {
        1 | 6 | 7 => data.get(vpos).map_or(0, |&b| b as u32),
        3 | 8      => tiff_r16(data, vpos, le) as u32,
        _          => tiff_r32(data, vpos, le),
    }
}

fn tiff_array_u32(data: &[u8], le: bool, typ: u16, count: u32, vpos: usize) -> Vec<u32> {
    let item_size: usize = match typ { 1|2|6|7 => 1, 3|8 => 2, _ => 4 };
    let total = item_size.saturating_mul(count as usize);
    let base  = if total <= 4 { vpos } else { tiff_r32(data, vpos, le) as usize };
    (0..count as usize).map(|i| match typ {
        1|6|7 => data.get(base + i).map_or(0, |&b| b as u32),
        3|8   => tiff_r16(data, base + i * 2, le) as u32,
        _     => tiff_r32(data, base + i * 4, le),
    }).collect()
}

fn decode_jpeg_strip(data: &[u8], off: usize, len: usize) -> Result<image::RgbaImage, String> {
    let end = off.checked_add(len).filter(|&e| e <= data.len())
        .ok_or("strip offset out of bounds")?;
    let bytes = &data[off..end];
    if !bytes.starts_with(&[0xFF, 0xD8]) { return Err("not a JPEG marker".into()); }
    image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)
        .map_err(|e| e.to_string())
        .map(|i| i.to_rgba8())
}

fn assemble_jpeg_tiles(
    data: &[u8],
    offsets: &[u32], counts: &[u32],
    img_w: u32, img_h: u32,
    tile_w: u32, tile_h: u32,
) -> Result<image::RgbaImage, String> {
    let tiles_x = (img_w + tile_w - 1) / tile_w;
    let mut buf = vec![0u8; (img_w * img_h * 4) as usize];
    for (i, (&off, &cnt)) in offsets.iter().zip(counts.iter()).enumerate() {
        let tile = decode_jpeg_strip(data, off as usize, cnt as usize)?;
        let tx   = i as u32 % tiles_x;
        let ty   = i as u32 / tiles_x;
        let x0   = tx * tile_w;
        let y0   = ty * tile_h;
        let cw   = tile_w.min(img_w.saturating_sub(x0));
        let ch   = tile_h.min(img_h.saturating_sub(y0));
        for row in 0..ch {
            let src = (row * tile.width() * 4) as usize;
            let dst = ((y0 + row) * img_w * 4 + x0 * 4) as usize;
            let n   = (cw * 4) as usize;
            if src + n <= tile.as_raw().len() && dst + n <= buf.len() {
                buf[dst..dst+n].copy_from_slice(&tile.as_raw()[src..src+n]);
            }
        }
    }
    image::ImageBuffer::from_raw(img_w, img_h, buf)
        .ok_or_else(|| "buffer size mismatch for tiled assembly".into())
}

fn assemble_jpeg_strips(
    data: &[u8],
    offsets: &[u32], counts: &[u32],
    img_w: u32, img_h: u32,
) -> Result<image::RgbaImage, String> {
    let mut buf = vec![0u8; (img_w * img_h * 4) as usize];
    let mut y = 0u32;
    for (&off, &cnt) in offsets.iter().zip(counts.iter()) {
        let strip  = decode_jpeg_strip(data, off as usize, cnt as usize)?;
        let copy_h = strip.height().min(img_h.saturating_sub(y));
        let copy_w = img_w.min(strip.width());
        for row in 0..copy_h {
            let src = (row * strip.width() * 4) as usize;
            let dst = ((y + row) * img_w * 4) as usize;
            let n   = (copy_w * 4) as usize;
            if src + n <= strip.as_raw().len() && dst + n <= buf.len() {
                buf[dst..dst+n].copy_from_slice(&strip.as_raw()[src..src+n]);
            }
        }
        y += strip.height();
        if y >= img_h { break; }
    }
    image::ImageBuffer::from_raw(img_w, img_h, buf)
        .ok_or_else(|| "buffer size mismatch for strip assembly".into())
}

fn load_dng_jpeg_tiles(path: &Path) -> Result<DecodedImage, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    if data.len() < 8 { return Err("file too small".into()); }

    let le = match &data[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return Err("not a TIFF/DNG file".into()),
    };
    if tiff_r16(&data, 2, le) != 42 { return Err("TIFF magic mismatch".into()); }

    struct Candidate {
        width: u32, height: u32,
        offsets: Vec<u32>, counts: Vec<u32>,
        tile_w: Option<u32>, tile_h: Option<u32>,
        subfile: u32,
    }
    let mut candidates: Vec<Candidate> = Vec::new();

    // Walk all IFDs including SubIFDs (tag 330) to find JPEG-compressed image data.
    let mut queue: Vec<usize> = vec![tiff_r32(&data, 4, le) as usize];
    let mut visited = std::collections::HashSet::<usize>::new();

    while let Some(ifd_off) = queue.pop() {
        if ifd_off == 0 || ifd_off + 2 > data.len() { continue; }
        if !visited.insert(ifd_off) { continue; }

        let nentries = tiff_r16(&data, ifd_off, le) as usize;
        let (mut width, mut height, mut compression, mut subfile) = (0u32, 0u32, 0u32, 0u32);
        let (mut offsets, mut counts) = (vec![], vec![]);
        let (mut tile_w, mut tile_h): (Option<u32>, Option<u32>) = (None, None);

        for i in 0..nentries {
            let e   = ifd_off + 2 + i * 12;
            if e + 12 > data.len() { break; }
            let tag = tiff_r16(&data, e,     le);
            let typ = tiff_r16(&data, e + 2, le);
            let cnt = tiff_r32(&data, e + 4, le);
            let vp  = e + 8;
            match tag {
                254 => subfile     = tiff_scalar(&data, le, typ, vp),
                256 => width       = tiff_scalar(&data, le, typ, vp),
                257 => height      = tiff_scalar(&data, le, typ, vp),
                259 => compression = tiff_scalar(&data, le, typ, vp),
                278 | 324 => offsets = tiff_array_u32(&data, le, typ, cnt, vp),
                279 | 325 => counts  = tiff_array_u32(&data, le, typ, cnt, vp),
                322 => tile_w = Some(tiff_scalar(&data, le, typ, vp)),
                323 => tile_h = Some(tiff_scalar(&data, le, typ, vp)),
                // SubIFD pointers — enqueue each
                330 => { for off in tiff_array_u32(&data, le, typ, cnt, vp) { queue.push(off as usize); } }
                _ => {}
            }
        }

        // Enqueue next chained IFD
        let next = tiff_r32(&data, ifd_off + 2 + nentries * 12, le) as usize;
        if next != 0 { queue.push(next); }

        // Only consider IFDs with JPEG compression and image data
        if (compression == 34892 || compression == 7 || compression == 6)
            && width > 0 && height > 0
            && !offsets.is_empty() && !counts.is_empty()
        {
            candidates.push(Candidate { width, height, offsets, counts, tile_w, tile_h, subfile });
        }
    }

    if candidates.is_empty() {
        return Err("no JPEG-compressed IFD found in DNG".into());
    }

    // Try non-thumbnails first, then largest by area
    candidates.sort_by(|a, b| {
        a.subfile.cmp(&b.subfile)
            .then((b.width * b.height).cmp(&(a.width * a.height)))
    });

    for c in &candidates {
        let result: Result<image::RgbaImage, String> = match (c.tile_w, c.tile_h) {
            (Some(tw), Some(th)) =>
                assemble_jpeg_tiles(&data, &c.offsets, &c.counts, c.width, c.height, tw, th),
            _ if c.offsets.len() == 1 =>
                decode_jpeg_strip(&data, c.offsets[0] as usize, c.counts[0] as usize),
            _ =>
                assemble_jpeg_strips(&data, &c.offsets, &c.counts, c.width, c.height),
        };
        if let Ok(rgba_img) = result {
            let mut img = image::DynamicImage::ImageRgba8(rgba_img);
            if let Some(o) = crate::metadata::get_orientation(path) {
                img = apply_orientation_to_image(img, o);
            }
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            return Ok(DecodedImage::new_from_u8(rgba.into_raw(), w, h));
        }
    }

    Err("failed to decode any JPEG-compressed IFD from DNG".into())
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
