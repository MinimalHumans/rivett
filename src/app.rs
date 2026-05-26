//! Top-level application state and the [`eframe::App`] implementation.

use eframe::CreationContext;
use egui::{CentralPanel, Context, Key, Vec2};
use std::time::{Duration, Instant};
use std::collections::{HashSet, VecDeque};

#[cfg(target_os = "windows")]
use chrono;

use crate::db::{Database, ImageRecord, TagRecord};
use std::path::Path;
use crate::image_loader::{ImageCache, DirectoryListing};
use crate::metadata::{read_metadata, MetaEntry};
use crate::formats::SupportedFormat;
use crate::session::{SessionState, Rotation, RatingFilter, RatingFilterOp};
use crate::settings::AppSettings;
use crate::viewer::ViewerState;
use crate::renderer::GammaRenderer;
use crate::utilities::UtilitiesState;
use std::sync::{Arc, Mutex};
use egui_glow::glow;

/// A wrapper to allow Arc<glow::Context> to be Send + Sync.
/// In eframe/egui on desktop, the GL context is only used on the main thread.
#[derive(Clone)]
struct SendSyncGl(Arc<glow::Context>);
unsafe impl Send for SendSyncGl {}
unsafe impl Sync for SendSyncGl {}

// ---------------------------------------------------------------------------
// RivettApp
// ---------------------------------------------------------------------------

pub struct RivettApp {
    db:              Option<Database>,
    viewer:          ViewerState,
    image_cache:     ImageCache,
    listing:         Option<DirectoryListing>,
    session:         SessionState,

    gamma_renderer:  Option<Arc<Mutex<GammaRenderer>>>,

    // UI state
    current_path:    Option<std::path::PathBuf>,
    current_record:  Option<ImageRecord>,
    current_image_tags: Vec<crate::db::TagRecord>,
    all_tags:        Vec<crate::db::TagRecord>,
    tag_input:       String,
    tag_suggestions: Vec<String>,
    tag_suggestion_idx: usize,
    note_buffer:     String,
    metadata:        Vec<MetaEntry>,

    show_info_panel: bool,
    show_help:       bool,
    toast:           Option<Toast>,
    delete_confirm:  Option<DeleteConfirm>,

    // Drag-out state
    pending_drag_out:  bool, // set on gesture detection; consumed at top of next update()

    // Save As state
    save_as_state:   Option<SaveAsState>,

    // Utility windows
    utilities:       UtilitiesState,

    // GPU pre-upload pipeline
    /// Ordered queue of images awaiting GPU texture upload (one per frame).
    gpu_upload_queue: VecDeque<(std::path::PathBuf, Arc<[f32]>, u32, u32)>,
    /// True while the current image's GPU texture has not yet been uploaded.
    gpu_upload_pending: bool,

    #[allow(dead_code)]
    settings:        AppSettings,
}

struct SaveAsState {
    output_path:       std::path::PathBuf,
    preserve_metadata: bool,
    focus_requested:   bool,
}

impl RivettApp {
    pub fn new(cc: &CreationContext<'_>, settings: AppSettings, initial_image: Option<std::path::PathBuf>) -> Self {
        // Platform-specific styling
        let mut visuals = egui::Visuals::dark();
        visuals.window_rounding = 0.0.into();
        cc.egui_ctx.set_visuals(visuals);

        let db_path = settings.central_db_resolved().unwrap_or_else(|| std::path::PathBuf::from("ratings.db"));
        let db = Database::open(&db_path).map_err(|e| {
            log::error!("failed to open database at {}: {e}", db_path.display());
            e
        }).ok();

        if let Some(gl) = &cc.gl {
            cc.egui_ctx.memory_mut(|mem| mem.data.insert_temp(egui::Id::new("gl_context"), SendSyncGl(gl.clone())));
        }

        let mut app = Self {
            db,
            viewer:          ViewerState::new(),
            image_cache:     ImageCache::new(32),
            listing:         None,
            session:         SessionState::new(settings.default_sort),
            gamma_renderer:  None,
            current_path:    None,
            current_record:  None,
            current_image_tags: vec![],
            all_tags:        vec![],
            tag_input:       String::new(),
            tag_suggestions: vec![],
            tag_suggestion_idx: 0,
            note_buffer:     String::new(),
            metadata:        vec![],
            show_info_panel: settings.show_info_panel,
            show_help:       false,
            toast:           None,
            delete_confirm:  None,
            pending_drag_out:     false,
            save_as_state:   None,
            utilities:       UtilitiesState::default(),
            gpu_upload_queue: VecDeque::new(),
            gpu_upload_pending: false,
            settings,
        };

        if let Some(path) = initial_image {
            app.open_image(path, &cc.egui_ctx);
        }

        app
    }

    // ── Toast helper ──────────────────────────────────────────────────────

    fn toast(&mut self, msg: impl Into<String>, kind: ToastKind) {
        self.toast = Some(Toast::new(msg.into(), kind));
    }

    // ── GPU pre-upload helpers ────────────────────────────────────────────

    /// Collect up to `n` non-ignored neighbor paths in each direction from cursor.
    fn neighbor_paths(&self, n: usize) -> Vec<std::path::PathBuf> {
        let Some(ref listing) = self.listing else { return vec![] };
        let idx = listing.current_index;
        let mut result = Vec::with_capacity(n * 2);

        let mut i = idx + 1;
        let mut found = 0;
        while i < listing.files.len() && found < n {
            let p = &listing.files[i];
            if !self.session_is_ignored(p) { result.push(p.clone()); found += 1; }
            i += 1;
        }

        let mut i = idx as i64 - 1;
        let mut found = 0;
        while i >= 0 && found < n {
            let p = &listing.files[i as usize];
            if !self.session_is_ignored(p) { result.push(p.clone()); found += 1; }
            i -= 1;
        }

        result
    }

    /// Queue an original image for GPU texture upload.
    fn queue_upload_for(&mut self, path: std::path::PathBuf, img: crate::image_loader::DecodedImage) {
        if self.gpu_upload_queue.iter().any(|(p, _, _, _)| p == &path) { return; }
        let gpu_ready = self.gamma_renderer.as_ref()
            .map_or(false, |r| r.lock().unwrap().has_cached(&path));
        if gpu_ready { return; }
        self.gpu_upload_queue.push_back((path, img.rgba.clone(), img.width, img.height));
    }

    // ── Opening / Loading ─────────────────────────────────────────────────

    pub fn open_image(&mut self, path: std::path::PathBuf, ctx: &Context) {
        if !path.exists() { return; }
        
        if path.is_file() {
            if let Some(dir) = path.parent() {
                let sort   = self.session_sort_order();
                let db     = self.db.as_ref();
                match DirectoryListing::scan(dir, sort, None, crate::session::TagFilter::default(), db) {
                    Ok(mut listing) => {
                        listing.seek_to(&path);
                        self.listing = Some(listing);
                    }
                    Err(e) => log::warn!("failed to scan directory: {e}"),
                }
            }
        }
        self.load_current(ctx, false);
    }

    fn load_current(&mut self, ctx: &Context, preserve_zoom: bool) {
        let path = match self.listing.as_ref().and_then(|l| l.current().cloned()) {
            Some(p) => p,
            None => {
                self.viewer.clear();
                self.current_path   = None;
                self.current_record = None;
                self.metadata       = vec![];
                return;
            }
        };

        // Clear existing image-status toast when moving to a new image
        if let Some(ref t) = self.toast {
            if t.kind == ToastKind::ImageStatus {
                self.toast = None;
            }
        }

        self.current_path = Some(path.clone());
        
        self.refresh_record();

        if let Some(ref rec) = self.current_record {
            if let Some(r) = rec.rating {
                self.toast(format!("Rated: {} stars", "★".repeat(r as usize)), ToastKind::ImageStatus);
            }
        }

        let rotation = self.session.rotation_for(&path);
        let mut adjustments = self.session.adjustments_for(&path).unwrap_or_default();

        if let Some(img) = self.image_cache.get(&path).cloned() {
            // Auto-set 2.2 gamma for linear/HDR images if no session adjustments yet
            if img.is_hdr && self.session.adjustments_for(&path).is_none() {
                adjustments.gamma = 2.2;
                self.session.set_adjustments(path.clone(), adjustments);
            }

            // Fast path: GPU texture already pre-uploaded — instant swap, no stall.
            let gpu_ready = self.gamma_renderer.as_ref()
                .map_or(false, |r| r.lock().unwrap().has_cached(&path));

            if gpu_ready {
                if let Some(ref r) = self.gamma_renderer {
                    r.lock().unwrap().set_active(path.clone());
                }
                self.viewer.load_image_meta(ctx, &img, rotation, adjustments, preserve_zoom);
                self.gpu_upload_pending = false;
            } else {
                // GPU texture not ready yet; queue the upload in the background.
                self.queue_upload_for(path.clone(), img.clone());
                self.gpu_upload_pending = true;
                // Set viewer metadata so zoom/pan math works while we wait.
                self.viewer.load_image_meta(ctx, &img, rotation, adjustments, preserve_zoom);
            }
        } else {
            // Not in cache. If already pending in background, just set loading state.
            self.gpu_upload_pending = false;
            if self.image_cache.is_pending(&path) {
                self.viewer.set_loading();
            } else {
                // Not in cache, not pending. Start prefetch.
                self.image_cache.prefetch(path.clone());
                self.viewer.set_loading();
            }
        }

        // CPU prefetch ±2; GPU queue ±1 for neighbors already in CPU cache.
        let neighbors_2 = self.neighbor_paths(2);
        let neighbors_1 = self.neighbor_paths(1);
        for p in &neighbors_2 {
            self.image_cache.prefetch(p.clone());
        }
        
        let neighbor_imgs: Vec<_> = neighbors_1.iter().filter_map(|p| {
            self.image_cache.get(p).map(|img| (p.clone(), img.clone()))
        }).collect();
        for (p, img) in neighbor_imgs {
            self.queue_upload_for(p, img);
        }

        self.metadata = read_metadata(&path);
    }

    fn refresh_record(&mut self) {
        self.current_record = self.current_path.as_ref().and_then(|path| {
            let db      = self.db.as_ref()?;
            let dir_str = path.parent()?.to_string_lossy();
            let fname   = path.file_name()?.to_str()?;
            let dir     = db.find_directory_by_path(&dir_str).ok()??;
            
            self.current_image_tags = db.get_image_tags(dir.id, fname).unwrap_or_default();
            self.all_tags = db.get_all_tags().unwrap_or_default();
            
            let rec = db.get_image(dir.id, fname).ok()?;
            self.note_buffer = rec.as_ref().and_then(|r| r.note.clone()).unwrap_or_default();
            rec
        });
        if self.current_record.is_none() {
            self.current_image_tags.clear();
            self.all_tags = self.db.as_ref().and_then(|db| db.get_all_tags().ok()).unwrap_or_default();
            self.note_buffer.clear();
        }
    }

    // ── Navigation ────────────────────────────────────────────────────────

    fn navigate_next(&mut self, ctx: &Context, preserve_zoom: bool) {
        self.navigate_next_n(ctx, preserve_zoom, 1);
    }

    fn navigate_next_n(&mut self, ctx: &Context, preserve_zoom: bool, n: usize) {
        let mut moved = false;
        if let Some(ref mut listing) = self.listing {
            for _ in 0..n {
                let mut step = false;
                while listing.go_next() {
                    step = true;
                    if let Some(p) = listing.current() {
                        if !self.session.ignored_images.contains(p) { break; }
                    }
                }
                if !step { break; }
                moved = true;
            }
        }
        if moved { self.load_current(ctx, preserve_zoom); }
    }

    fn navigate_prev(&mut self, ctx: &Context, preserve_zoom: bool) {
        self.navigate_prev_n(ctx, preserve_zoom, 1);
    }

    fn navigate_prev_n(&mut self, ctx: &Context, preserve_zoom: bool, n: usize) {
        let mut moved = false;
        if let Some(ref mut listing) = self.listing {
            for _ in 0..n {
                let mut step = false;
                while listing.go_prev() {
                    step = true;
                    if let Some(p) = listing.current() {
                        if !self.session.ignored_images.contains(p) { break; }
                    }
                }
                if !step { break; }
                moved = true;
            }
        }
        if moved { self.load_current(ctx, preserve_zoom); }
    }

    // ── Navigate to parent directory ─────────────────────────────────────

    fn navigate_to_parent(&mut self, ctx: &Context) {
        let current_dir = self.listing.as_ref()
            .and_then(|l| l.current().and_then(|p| p.parent()).map(|p| p.to_path_buf()))
            .or_else(|| self.current_path.as_ref().and_then(|p| p.parent()).map(|p| p.to_path_buf()));

        let Some(dir) = current_dir else { return };
        let Some(parent) = dir.parent() else { return };

        let sort = self.session_sort_order();
        let db   = self.db.as_ref();
        match DirectoryListing::scan(parent, sort, None, crate::session::TagFilter::default(), db) {
            Ok(mut listing) => {
                listing.go_to_first();
                self.listing = Some(listing);
                self.load_current(ctx, false);
            }
            Err(e) => log::warn!("failed to scan parent directory: {e}"),
        }
    }

    // ── Navigate to list boundaries ───────────────────────────────────────

    fn navigate_first(&mut self, ctx: &Context, preserve_zoom: bool) {
        if let Some(ref mut listing) = self.listing {
            listing.go_to_first();
            while listing.current().map(|p| self.session.ignored_images.contains(p)).unwrap_or(false) {
                if !listing.go_next() { break; }
            }
        }
        self.load_current(ctx, preserve_zoom);
    }

    fn navigate_last(&mut self, ctx: &Context, preserve_zoom: bool) {
        if let Some(ref mut listing) = self.listing {
            listing.go_to_last();
            while listing.current().map(|p| self.session.ignored_images.contains(p)).unwrap_or(false) {
                if !listing.go_prev() { break; }
            }
        }
        self.load_current(ctx, preserve_zoom);
    }

    // ── Hide (ignore) ─────────────────────────────────────────────────────

    fn hide_current(&mut self, ctx: &Context) {
        let Some(path) = self.current_path.clone() else { return };
        self.session.ignore_image(path.clone());
        self.toast(format!("Hidden: {}", path.file_name()
            .and_then(|n| n.to_str()).unwrap_or("?")), ToastKind::ImageStatus);
        let before = self.current_path.clone();
        self.navigate_next(ctx, false);
        if self.current_path == before {
            self.navigate_prev(ctx, false);
        }
    }

    // ── Rating ────────────────────────────────────────────────────────────

    fn set_rating(&mut self, rating: Option<u8>) {
        if let Some(path) = &self.current_path {
            if let (Some(db), Some(dir_str), Some(fname)) = (
                &self.db,
                path.parent().map(|p| p.to_string_lossy().into_owned()),
                path.file_name().and_then(|n| n.to_str()).map(str::to_string),
            ) {
                match db.upsert_directory_by_path(&dir_str) {
                    Ok(dir) => {
                        if let Err(e) = db.set_rating(dir.id, &fname, rating) {
                            self.toast(format!("Database error: {e}"), ToastKind::Error);
                        } else {
                            self.toast(match rating {
                                Some(r) => format!("Rated: {} stars", "★".repeat(r as usize)),
                                None    => "Rating cleared".to_string(),
                            }, ToastKind::ImageStatus);
                            self.refresh_record();
                        }
                    }
                    Err(e) => {
                        self.toast(format!("Database error: {e}"), ToastKind::Error);
                    }
                }
            } else if self.db.is_none() {
                self.toast("Database not available", ToastKind::Error);
            }
        }
    }

    fn rotate_current(&mut self, cw: bool, ctx: &Context) {
        let Some(path) = self.current_path.clone() else { return };
        if cw { self.session.rotate_cw(path.clone()); } else { self.session.rotate_ccw(path.clone()); }

        self.gpu_upload_queue.retain(|(p, _, _, _)| p != &path);
        if let Some(ref r) = self.gamma_renderer {
            r.lock().unwrap().invalidate(None, &path);
        }

        self.load_current(ctx, true);
    }

    // ── Delete ────────────────────────────────────────────────────────────

    fn confirm_delete(&mut self) {
        self.delete_confirm = Some(DeleteConfirm::new());
        self.toast("Press Delete again to confirm — Esc to cancel", ToastKind::General);
    }

    fn execute_delete(&mut self, ctx: &Context) {
        self.delete_confirm = None;
        let Some(path) = self.current_path.clone() else { return };
        match trash::delete(&path) {
            Ok(()) => {
                let name = path.file_name()
                    .and_then(|n| n.to_str()).unwrap_or("?").to_string();

                let old_index = self.listing.as_ref().map(|l| l.current_index).unwrap_or(0);
                let sort = self.session_sort_order();
                let db_ref = self.db.as_ref();
                if let Some(ref mut listing) = self.listing {
                    let _ = listing.refresh(sort, db_ref);
                    listing.current_index = old_index.min(listing.files.len().saturating_sub(1));
                }

                self.toast(format!("Moved to Trash: {name}"), ToastKind::General);
                self.current_path   = None;
                self.current_record = None;
                self.metadata       = vec![];
                self.viewer.clear();
                self.load_current(ctx, false);
            }
            Err(e) => {
                self.toast(format!("Delete failed: {e}"), ToastKind::General);
            }
        }
    }

    // ── Hard refresh ─────────────────────────────────────────────────────

    fn hard_refresh(&mut self, ctx: &Context) {
        self.session.flush();
        if let Some(dir) = self.listing.as_ref().map(|l| l.dir_path.clone()) {
            let sort = self.session_sort_order();
            if let Ok(mut fresh) = DirectoryListing::scan(&dir, sort, None, crate::session::TagFilter::default(), self.db.as_ref()) {
                if let Some(ref cur) = self.current_path.clone() {
                    fresh.seek_to(cur);
                }
                self.listing = Some(fresh);
            }
        }
        self.load_current(ctx, false);
    }

    // ── Soft refresh (Ctrl+R) ────────────────────────────────────────────

    fn soft_refresh(&mut self, ctx: &Context) {
        let sort = self.session_sort_order();
        let db_ref = self.db.as_ref();
        if let Some(ref mut listing) = self.listing {
            let old_index = listing.current_index;
            let had_current = listing.current().cloned();
            if listing.refresh(sort, db_ref).is_ok() {
                if !had_current.as_ref().map(|p| listing.seek_to(p)).unwrap_or(false) {
                    listing.current_index = old_index.min(listing.files.len().saturating_sub(1));
                }
            }
        }
        self.load_current(ctx, false);
        self.toast("Directory refreshed", ToastKind::General);
    }

    // ── Drag-out ──────────────────────────────────────────────────────────

    #[cfg(target_os = "windows")]
    fn execute_drag_out(&mut self) {
        let Some(path) = self.current_path.clone() else { return };
        let win = OwnedWindowHandle(1);

        // Build a small thumbnail for the drag cursor from the cached decoded image.
        // Image::File would load the full-resolution image, making the cursor giant.
        let drag_image = 'img: {
            let Some(decoded) = self.image_cache.get(&path) else {
                break 'img drag::Image::File(path.clone());
            };
            let Some(src) = image::RgbaImage::from_raw(
                decoded.width, decoded.height, decoded.to_u8(),
            ) else {
                break 'img drag::Image::File(path.clone());
            };
            let thumb = image::imageops::thumbnail(&src, 128, 128);
            // Centre the thumbnail on a 128×128 transparent canvas.
            // Use replace (not overlay) — no alpha compositing, direct pixel copy.
            let tw = thumb.width();
            let th = thumb.height();
            let ox = 128u32.saturating_sub(tw) / 2;
            let oy = 128u32.saturating_sub(th) / 2;
            let mut canvas = image::RgbaImage::new(128, 128);
            image::imageops::replace(&mut canvas, &thumb, ox as i64, oy as i64);
            let mut png = Vec::new();
            if image::DynamicImage::ImageRgba8(canvas)
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .is_ok()
            {
                drag::Image::Raw(png)
            } else {
                drag::Image::File(path.clone())
            }
        };

        let _ = drag::start_drag(
            &win,
            drag::DragItem::Files(vec![path]),
            drag_image,
            |_result, _pos| {},
            drag::Options::default(),
        );
    }

    #[cfg(not(target_os = "windows"))]
    fn execute_drag_out(&mut self) {
        // Drag-out is currently only implemented for Windows due to 
        // library compatibility with egui on Linux.
    }

    // ── Save changes (Ctrl+S) ───────────────────────────────────────────

    fn copy_to_clipboard(&mut self) {
        let Some(path) = self.current_path.clone() else { return };
        let Some(decoded) = self.image_cache.get(&path) else {
            self.toast("Image not in cache", ToastKind::Error);
            return;
        };

        let rotation = self.session.rotation_for(&path);
        
        let pixels = decoded.to_u8();
        let width = decoded.width;
        let height = decoded.height;

        let (rotated_pixels, w, h) = if rotation.is_identity() {
            (pixels, width, height)
        } else {
            let src = image::RgbaImage::from_raw(width, height, pixels).unwrap();
            let rotated = match rotation {
                Rotation::Cw90  => image::imageops::rotate90(&src),
                Rotation::Cw180 => image::imageops::rotate180(&src),
                Rotation::Cw270 => image::imageops::rotate270(&src),
                _ => unreachable!(),
            };
            let (nw, nh) = rotated.dimensions();
            (rotated.into_raw(), nw, nh)
        };

        match arboard::Clipboard::new() {
            Ok(mut clipboard) => {
                let img_data = arboard::ImageData {
                    width: w as usize,
                    height: h as usize,
                    bytes: std::borrow::Cow::from(rotated_pixels),
                };
                if let Err(e) = clipboard.set_image(img_data) {
                    self.toast(format!("Clipboard error: {e}"), ToastKind::Error);
                } else {
                    self.toast("Image copied to clipboard", ToastKind::General);
                }
            }
            Err(e) => self.toast(format!("Failed to open clipboard: {e}"), ToastKind::Error),
        }
    }

    fn save_current_changes(&mut self, ctx: &Context) {
        let Some(path) = self.current_path.clone() else { return };
        let rotation = self.session.rotation_for(&path);
        let strip_metadata = self.session.is_metadata_stripped(&path);
        
        if rotation.is_identity() && !strip_metadata {
            self.toast("No changes to save", ToastKind::General);
            return;
        }

        let Some(fmt) = SupportedFormat::from_path(&path) else {
            self.toast("Unknown format — cannot save", ToastKind::General);
            return;
        };

        // If we only want to strip metadata (no rotation), or if it's a format
        // where we always re-encode anyway, we can use save_image_as to the same path.
        let cached_clone = self.image_cache.get(&path).cloned();
        
        let result = if strip_metadata {
            save_image_as(&path, &path, rotation, cached_clone.as_ref(), false)
        } else {
            match fmt {
                SupportedFormat::Jpeg => save_jpeg_exif_rotation(&path, rotation),
                SupportedFormat::Svg  => { self.toast("Cannot save changes for SVG files", ToastKind::General); return; }
                SupportedFormat::Raw  => { self.toast("Cannot save changes for RAW files", ToastKind::General); return; }
                _                     => save_pixel_rotation(&path, fmt, rotation, cached_clone),
            }
        };

        match result {
            Ok(()) => {
                self.session.set_rotation(path.clone(), Rotation::None);
                if strip_metadata {
                    self.session.toggle_metadata_strip(path.clone());
                }
                self.image_cache.remove(&path);
                self.load_current(ctx, true);
                self.toast("Saved", ToastKind::General);
            }
            Err(e) => self.toast(format!("Save failed: {e}"), ToastKind::General),
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────

    fn apply_sort_order(&mut self, order: crate::settings::SortOrder, ctx: &Context) {
        self.session.sort_order = order;
        self.refresh_listing(ctx);
    }

    fn session_sort_order(&self) -> crate::settings::SortOrder {
        self.session.sort_order
    }

    fn session_is_ignored(&self, _path: &std::path::Path) -> bool {
        false
    }

    fn window_title(&self) -> String {
        let mut title = if let Some(ref p) = self.current_path {
            format!("{} — Rivett", p.display())
        } else {
            "Rivett".to_string()
        };

        if let Some(ref filter) = self.session.rating_filter {
            let scope = if self.listing.as_ref().map(|l| l.dir_path.as_os_str().is_empty()).unwrap_or(false) {
                "Library"
            } else {
                "Folder"
            };
            title = format!("{title} ({scope}: ★ {}+)", filter.value);
        }

        title
    }

    fn reveal_in_file_manager(&self) {
        if let Some(ref p) = self.current_path {
            let _ = showfile::show_path_in_file_manager(p);
        }
    }

    // ── Keyboard ─────────────────────────────────────────────────────

    fn handle_keyboard(&mut self, ctx: &Context) {
        let input = ctx.input(|i| i.clone());
        let widget_focused = ctx.memory(|m| m.focused().is_some());

        if input.key_pressed(Key::Escape) {
            if self.show_help {
                self.show_help = false;
                return;
            }
            if self.delete_confirm.is_some() {
                self.delete_confirm = None;
                self.toast("Delete cancelled", ToastKind::General);
            }
        }

        if widget_focused { return; }

        let typed_question_mark = input.events.iter().any(|e| matches!(e, egui::Event::Text(t) if t == "?"));
        if typed_question_mark {
            self.show_help = !self.show_help;
            return;
        }

        let shift = input.modifiers.shift;
        let preserve_zoom = shift;

        if input.key_pressed(Key::ArrowRight) {
            self.navigate_next(ctx, preserve_zoom);
        }
        if input.key_pressed(Key::ArrowLeft) {
            self.navigate_prev(ctx, preserve_zoom);
        }
        if input.key_pressed(Key::ArrowDown) || input.key_pressed(Key::PageDown) {
            self.navigate_next_n(ctx, preserve_zoom, 10);
        }
        if input.key_pressed(Key::ArrowUp) || input.key_pressed(Key::PageUp) {
            self.navigate_prev_n(ctx, preserve_zoom, 10);
        }
        if input.modifiers.alt && input.key_pressed(Key::ArrowUp) {
            self.navigate_to_parent(ctx);
        }
        if input.key_pressed(Key::Home) { self.navigate_first(ctx, preserve_zoom); }
        if input.key_pressed(Key::End)  { self.navigate_last(ctx, preserve_zoom); }

        if input.key_pressed(Key::I) { 
            self.show_info_panel = !self.show_info_panel;
            self.settings.show_info_panel = self.show_info_panel;
            let _ = self.settings.save();
        }

        if input.key_pressed(Key::T) {
            let tag_field_id = egui::Id::new("tag_input_field");
            if !ctx.memory(|m| m.has_focus(tag_field_id)) {
                self.show_info_panel = true;
                ctx.memory_mut(|m| m.request_focus(tag_field_id));
                // Consume the 't' so it doesn't end up in the field
                ctx.input_mut(|i| {
                    i.events.retain(|e| !matches!(e, egui::Event::Text(t) if t == "t" || t == "T"));
                });
            }
        }

        if input.modifiers.is_none() {
            for r in 0..=5 {
                let key = match r {
                    0 => Key::Num0,
                    1 => Key::Num1,
                    2 => Key::Num2,
                    3 => Key::Num3,
                    4 => Key::Num4,
                    5 => Key::Num5,
                    _ => unreachable!(),
                };
                let rating = if r == 0 { None } else { Some(r as u8) };
                if input.key_pressed(key) { self.set_rating(rating); }
            }
        }

        if input.key_pressed(Key::H) { self.hide_current(ctx); }

        if input.key_pressed(Key::OpenBracket) {
            self.rotate_current(false, ctx);
        }
        if input.key_pressed(Key::CloseBracket) {
            self.rotate_current(true, ctx);
        }

        let ctrl = input.modifiers.ctrl;
        if input.key_pressed(Key::F) {
            self.viewer.toggle_fit(ctx.screen_rect().size());
        }

        if input.key_pressed(Key::Delete) {
            if self.delete_confirm.as_ref().map(|d| d.alive()).unwrap_or(false) {
                self.execute_delete(ctx);
            } else {
                self.confirm_delete();
            }
        }

        if ctrl && input.modifiers.shift && input.key_pressed(Key::R) {
            self.hard_refresh(ctx);
        } else if ctrl && input.key_pressed(Key::R) {
            self.soft_refresh(ctx);
        }

        if ctrl && !input.modifiers.shift && input.key_pressed(Key::S) {
            self.save_current_changes(ctx);
        }
        if ctrl && !input.modifiers.shift && input.key_pressed(Key::C) {
            self.copy_to_clipboard();
        }
        if ctrl && input.modifiers.shift && input.key_pressed(Key::S) {
            self.save_as(ctx);
        }
    }

    // ── Save As ──────────────────────────────────────────────────────────

    fn save_as(&mut self, _ctx: &Context) {
        let Some(path) = self.current_path.clone() else { return };

        let dialog = rfd::FileDialog::new()
            .set_file_name(path.file_name().unwrap_or_default().to_string_lossy())
            .add_filter("JPEG", &["jpg", "jpeg"])
            .add_filter("PNG", &["png"]);

        if let Some(output_path) = dialog.save_file() {
            // Only show the confirmation modal if there is actual metadata that can be toggled.
            // If the metadata list only contains headers or is empty, skip the modal.
            let has_togglable_metadata = self.metadata.iter().any(|m| !m.is_header);
            
            if has_togglable_metadata {
                self.save_as_state = Some(SaveAsState {
                    output_path,
                    preserve_metadata: self.settings.preserve_metadata,
                    focus_requested: false,
                });
            } else {
                // No metadata to preserve or configure -> save immediately
                let state = SaveAsState {
                    output_path,
                    preserve_metadata: false,
                    focus_requested: true,
                };
                self.perform_save_as(&state);
            }
        }
    }

    fn draw_save_as_modal(&mut self, ctx: &Context) {
        let Some(mut state) = self.save_as_state.take() else { return };
        let mut should_close = false;

        egui::Window::new("Save Image As")
            .collapsible(false)
            .resizable(false)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.screen_rect().center())
            .show(ctx, |ui| {
                ui.vertical(|ui| {
                    ui.label(format!("Path: {}", state.output_path.display()));
                    ui.add_space(8.0);

                    let has_metadata = self.metadata.iter().any(|m| !m.is_header);
                    if has_metadata {
                        if ui.checkbox(&mut state.preserve_metadata, "Preserve metadata").changed() {
                            self.settings.preserve_metadata = state.preserve_metadata;
                            let _ = self.settings.save();
                        }
                        ui.add_space(12.0);
                    } else {
                        state.preserve_metadata = false;
                    }

                    ui.horizontal(|ui| {
                        let save_btn = ui.button("Save");
                        
                        if !state.focus_requested {
                            save_btn.request_focus();
                            state.focus_requested = true;
                        }

                        if save_btn.clicked() {
                            self.perform_save_as(&state);
                            should_close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            should_close = true;
                        }
                    });
                });
            });

        if !should_close {
            self.save_as_state = Some(state);
        }
    }

    fn draw_help_overlay(&mut self, ctx: &Context) {
        if !self.show_help { return; }

        let mut open = true;
        egui::Window::new("Shortcuts & Interactions")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.screen_rect().center())
            .show(ctx, |ui| {
                let row = |ui: &mut egui::Ui, key: &str, desc: &str| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(key).monospace().strong());
                    });
                    ui.label(desc);
                    ui.end_row();
                };
                let section = |ui: &mut egui::Ui, title: &str| {
                    ui.label(egui::RichText::new(title).small().color(egui::Color32::from_gray(140)));
                    ui.label("");
                    ui.end_row();
                };

                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        egui::Grid::new("help_grid_kb")
                            .num_columns(2)
                            .spacing([16.0, 3.0])
                            .show(ui, |ui| {
                                section(ui, "KEYBOARD NAVIGATION");
                                row(ui, "→",            "Next image");
                                row(ui, "←",            "Previous image");
                                row(ui, "↓  /  PgDn",   "Jump forward 10");
                                row(ui, "↑  /  PgUp",   "Jump back 10");
                                row(ui, "Home",          "First image");
                                row(ui, "End",           "Last image");
                                row(ui, "Alt+↑",         "Open parent directory");
                                row(ui, "Shift + nav",   "Preserve zoom");

                                ui.label(""); ui.label(""); ui.end_row();
                                section(ui, "VIEW");
                                row(ui, "F",  "Toggle fit / actual size");
                                row(ui, "I",  "Toggle info panel");
                                row(ui, "?",  "Show / hide this help");

                                ui.label(""); ui.label(""); ui.end_row();
                                section(ui, "RATING");
                                row(ui, "1 – 5", "Set star rating");
                                row(ui, "0",     "Clear rating");

                                ui.label(""); ui.label(""); ui.end_row();
                                section(ui, "FILE MANAGEMENT");
                                row(ui, "H / Alt+H",   "Hide / ignore image (Session only)");
                                row(ui, "Del × 2",     "Move to trash");
                                row(ui, "Escape",       "Cancel delete");

                            });
                    });

                    ui.add_space(32.0);

                    ui.vertical(|ui| {
                        egui::Grid::new("help_grid_mouse")
                            .num_columns(2)
                            .spacing([16.0, 3.0])
                            .show(ui, |ui| {
                                section(ui, "MOUSE CANVAS");
                                row(ui, "Left-drag",        "Pan image");
                                row(ui, "Ctrl + Drag",      "Drag out to OS");
                                row(ui, "Scroll / Pinch",   "Zoom in / out");
                                row(ui, "Double-click",     "Open file picker");
                                row(ui, "Right-click",      "Context menu");
                                row(ui, "Drop file",        "Open image");
                                
                                ui.label(""); ui.label(""); ui.end_row();
                                section(ui, "HISTOGRAM & ADJUSTMENTS");
                                row(ui, "Drag handles",     "Adjust black/white pt");
                                row(ui, "Drag center",      "Shift both points");
                                row(ui, "Dbl-click handle", "Reset point");
                                row(ui, "Shift + Drag",     "Coarse exposure adj");
                                row(ui, "Dbl-click slider", "Reset exp/gamma");

                                ui.label(""); ui.label(""); ui.end_row();
                                section(ui, "ADJUSTMENTS");
                                row(ui, "[",  "Rotate counter-clockwise");
                                row(ui, "]",  "Rotate clockwise");
                                
                                ui.label(""); ui.label(""); ui.end_row();
                                section(ui, "SAVE & REFRESH");
                                row(ui, "Ctrl+S",       "Save changes");
                                row(ui, "Ctrl+Shift+S", "Save As");
                                row(ui, "Ctrl+C",       "Copy image to clipboard");
                                row(ui, "Ctrl+R",       "Soft refresh");
                                row(ui, "Ctrl+Shift+R", "Hard refresh");
                            });
                    });
                });
            });
        if !open { self.show_help = false; }
    }

    fn perform_save_as(&mut self, state: &SaveAsState) {
        let Some(src_path) = self.current_path.clone() else { return };
        
        let rotation = self.session.rotation_for(&src_path);
        let cached = self.image_cache.get(&src_path);
        
        match save_image_as(&src_path, &state.output_path, rotation, cached, state.preserve_metadata) {
            Ok(()) => self.toast("Saved successfully", ToastKind::General),
            Err(e) => self.toast(format!("Save failed: {e}"), ToastKind::General),
        }
    }

    // ── Info panel ────────────────────────────────────────────────────────

    fn draw_info_panel(&mut self, ctx: &Context) {
        egui::SidePanel::right("info_panel")
            .resizable(true)
            .min_width(280.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let label_kv = |ui: &mut egui::Ui, key: &str, value: String| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            ui.label(egui::RichText::new(format!("{key}:")).strong());
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.add(egui::Label::new(&value).selectable(true).truncate())
                                    .on_hover_text(&value);
                            });
                        });
                        ui.add_space(2.0);
                    };

                    if let Some(path) = self.current_path.clone() {
                        ui.horizontal(|ui| {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if let Some(ref listing) = self.listing {
                                    ui.label(egui::RichText::new(format!("{}/{}", listing.current_index + 1, listing.files.len())).strong());
                                }
                                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                                    ui.add(egui::Label::new(egui::RichText::new(name).strong()).truncate());
                                });
                            });
                        });
                        ui.separator();

                        // Rating — centered, no label, second row
                        {
                            let rating = self.current_record.as_ref().and_then(|r| r.rating);
                            let stars = match rating {
                                None    => "—".to_string(),
                                Some(r) => format!("{} ({})", "★".repeat(r as usize), r),
                            };
                            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                                ui.label(stars);
                            });
                        }

                        if let Some(parent) = path.parent() {
                            label_kv(ui, "Folder", parent.display().to_string());
                        }

                        if let Ok(meta) = path.metadata() {
                            #[cfg(target_os = "windows")]
                            if let Ok(modified) = meta.modified() {
                                let datetime: chrono::DateTime<chrono::Local> = modified.into();
                                label_kv(ui, "Date", datetime.format("%Y-%m-%d %H:%M:%S").to_string());
                            }
                        }

                        let dim = self.viewer.image_size;
                        if dim != Vec2::ZERO {
                            let mut info = format!("{}×{} 8bit RGB", dim.x as u32, dim.y as u32);
                            if let Ok(meta) = path.metadata() {
                                let kb = meta.len() as f64 / 1024.0;
                                let size_str = if kb < 1024.0 {
                                    format!("{kb:.1} KB")
                                } else {
                                    format!("{:.1} MB", kb / 1024.0)
                                };
                                info = format!("{info} ({size_str})");
                            }
                            label_kv(ui, "Size", info);
                        }

                        label_kv(ui, "Zoom", format!("{:.0}%", self.viewer.zoom * 100.0));

                        ui.separator();

                        let mut adj = self.session.adjustments_for(&path).unwrap_or_default();
                        let mut changed = false;
                        let shift_held = ui.input(|i| i.modifiers.shift);

                        if let Some(img) = self.image_cache.get(&path) {
                            let hist_height = 80.0;
                            let bar_w = 6.0;
                            let (rect, response) = ui.allocate_at_least(egui::vec2(ui.available_width(), hist_height + 20.0), egui::Sense::drag());
                            let hist_rect = egui::Rect::from_min_max(
                                egui::pos2(rect.min.x + bar_w, rect.min.y),
                                egui::pos2(rect.max.x - bar_w, rect.min.y + hist_height),
                            );

                            ui.painter().rect_filled(hist_rect, 2.0, egui::Color32::from_gray(30));

                            let mut low_count = [0u32; 3];
                            let mut high_count = [0u32; 3];

                            if adj.remap_min == 0.0 && adj.remap_max == 1.0 {
                                low_count = img.histograms.low_clips;
                                high_count = img.histograms.high_clips;
                            } else {
                                for chunk in img.rgba.chunks_exact(4) {
                                    for i in 0..3 {
                                        if chunk[i] < adj.remap_min { low_count[i] += 1; }
                                        if chunk[i] > adj.remap_max { high_count[i] += 1; }
                                    }
                                }
                            }

                            let total = img.histograms.total_pixels as f32;
                            let draw_clip_bar = |ui: &mut egui::Ui, side_rect: egui::Rect, counts: [u32; 3], label: &str| {
                                let max_c = *counts.iter().max().unwrap_or(&0);
                                if max_c > 0 {
                                    let pct = (max_c as f32 / total * 100.0).max(0.1);
                                    let color = if pct > 1.0 { egui::Color32::from_rgb(255, 50, 50) } else { egui::Color32::from_rgb(255, 200, 0) };
                                    ui.painter().rect_filled(side_rect, 0.0, color);
                                    let resp = ui.interact(side_rect, egui::Id::new(label), egui::Sense::hover());
                                    if resp.hovered() {
                                        egui::show_tooltip_at_pointer(ui.ctx(), ui.layer_id(), egui::Id::new(label).with("tip"), |ui: &mut egui::Ui| {
                                            ui.label(egui::RichText::new(format!("{} clipping", label)).strong());
                                            ui.label(format!("  {} pixels total ({:.1}%)", max_c, max_c as f32 / total * 100.0));
                                            let channels = ["red", "green", "blue"];
                                            for i in 0..3 {
                                                ui.label(format!("  {} {} ({:.1}%)", counts[i], channels[i], counts[i] as f32 / total * 100.0));
                                            }
                                        });
                                    }
                                }
                            };

                            draw_clip_bar(ui, egui::Rect::from_min_max(rect.left_top(), egui::pos2(rect.left() + bar_w, hist_rect.bottom())), low_count, "Shadow");
                            draw_clip_bar(ui, egui::Rect::from_min_max(egui::pos2(rect.right() - bar_w, rect.top()), egui::pos2(rect.right(), hist_rect.bottom())), high_count, "Highlight");

                            let paint_channel = |ui: &mut egui::Ui, bins: &[f32], color: egui::Color32| {
                                if bins.is_empty() { return; }
                                let bin_width = hist_rect.width() / bins.len() as f32;
                                let mut points = Vec::with_capacity(bins.len() * 2);
                                for (i, &val) in bins.iter().enumerate() {
                                    let x = hist_rect.min.x + i as f32 * bin_width;
                                    let h = val * hist_height;
                                    let y = hist_rect.max.y - h;
                                    points.push(egui::pos2(x, y));
                                    points.push(egui::pos2(x + bin_width, y));
                                }
                                ui.painter().add(egui::Shape::line(points, egui::Stroke::new(1.2, color)));
                            };

                            paint_channel(ui, &img.histograms.r, egui::Color32::from_rgba_unmultiplied(255, 50, 50, 180));
                            paint_channel(ui, &img.histograms.g, egui::Color32::from_rgba_unmultiplied(50, 255, 50, 180));
                            paint_channel(ui, &img.histograms.b, egui::Color32::from_rgba_unmultiplied(50, 50, 255, 180));

                            if img.is_hdr {
                                let to_x = |val: f32| hist_rect.min.x + val.clamp(0.0, 1.0) * hist_rect.width();
                                let from_x = |x: f32| (x - hist_rect.min.x) / hist_rect.width();

                                let min_x = to_x(adj.remap_min);
                                let max_x = to_x(adj.remap_max);

                                let handle_w = 8.0;
                                let draw_handle = |ui: &mut egui::Ui, x: f32, id: &str| {
                                    let h_rect = egui::Rect::from_center_size(egui::pos2(x, hist_rect.bottom() + 8.0), egui::vec2(handle_w, 16.0));
                                    let res = ui.interact(h_rect, egui::Id::new(id), egui::Sense::click_and_drag());
                                    let color = if res.dragged() || res.hovered() { egui::Color32::WHITE } else { egui::Color32::from_gray(180) };
                                    ui.painter().rect_filled(h_rect, 1.0, color);
                                    ui.painter().line_segment([egui::pos2(x, hist_rect.top()), egui::pos2(x, hist_rect.bottom())], egui::Stroke::new(1.0, color.gamma_multiply(0.5)));
                                    res
                                };

                                let res_min = draw_handle(ui, min_x, "min_handle");
                                let res_max = draw_handle(ui, max_x, "max_handle");

                                if res_min.double_clicked() {
                                    adj.remap_min = 0.0;
                                    changed = true;
                                } else if res_min.dragged() {
                                    adj.remap_min = from_x(min_x + res_min.drag_delta().x);
                                    changed = true;
                                }
                                if res_max.double_clicked() {
                                    adj.remap_max = 1.0;
                                    changed = true;
                                } else if res_max.dragged() {
                                    adj.remap_max = from_x(max_x + res_max.drag_delta().x);
                                    changed = true;
                                }

                                if response.dragged() && !res_min.dragged() && !res_max.dragged() {
                                    let delta = from_x(hist_rect.min.x + response.drag_delta().x) - from_x(hist_rect.min.x);
                                    adj.remap_min += delta;
                                    adj.remap_max += delta;
                                    changed = true;
                                }

                                // Black/white point inputs: left-aligned min, right-aligned max
                                ui.add_space(2.0);
                                ui.horizontal(|ui| {
                                    let r_min = ui.add(egui::DragValue::new(&mut adj.remap_min).speed(0.005))
                                        .on_hover_text("Black point");
                                    if r_min.changed() { changed = true; }
                                    if r_min.double_clicked() { adj.remap_min = 0.0; changed = true; }
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        let r_max = ui.add(egui::DragValue::new(&mut adj.remap_max).speed(0.005))
                                            .on_hover_text("White point");
                                        if r_max.changed() { changed = true; }
                                        if r_max.double_clicked() { adj.remap_max = 1.0; changed = true; }
                                    });
                                });
                            }
                        }

                        // Exposure — label above, DragValue + growing slider below
                        ui.label("Exposure");
                        ui.horizontal(|ui| {
                            let drag_speed = if shift_held { 0.5 } else { 0.05 };
                            let r_dv = ui.add(egui::DragValue::new(&mut adj.exposure)
                                .speed(drag_speed)
                                .custom_formatter(|n, _| format!("{:+.2}", n))
                                .custom_parser(|s| s.trim_start_matches('+').parse::<f64>().ok()));
                            if r_dv.changed() { changed = true; }
                            let remaining = (ui.available_width() - ui.spacing().item_spacing.x).max(0.0);
                            ui.style_mut().spacing.slider_width = remaining;
                            let r_sl = ui.add(egui::Slider::new(&mut adj.exposure, -4.0..=4.0).show_value(false));
                            if r_sl.changed() { changed = true; }
                            // Slider uses drag sense internally; overlay a click sense to catch double-click
                            if ui.interact(r_sl.rect, egui::Id::new("exp_slider_dc"), egui::Sense::click()).double_clicked() {
                                adj.exposure = 0.0;
                                changed = true;
                            }
                        });

                        // Gamma — same pattern
                        ui.label("Gamma");
                        ui.horizontal(|ui| {
                            let r_dv = ui.add(egui::DragValue::new(&mut adj.gamma).speed(0.01));
                            if r_dv.changed() { changed = true; }
                            let remaining = (ui.available_width() - ui.spacing().item_spacing.x).max(0.0);
                            ui.style_mut().spacing.slider_width = remaining;
                            let r_sl = ui.add(egui::Slider::new(&mut adj.gamma, 0.1..=4.0).show_value(false));
                            if r_sl.changed() { changed = true; }
                            if ui.interact(r_sl.rect, egui::Id::new("gamma_slider_dc"), egui::Sense::click()).double_clicked() {
                                adj.gamma = 1.0;
                                changed = true;
                            }
                        });

                        if changed && shift_held {
                            adj.exposure = (adj.exposure * 2.0).round() / 2.0;
                        }

                        if changed {
                            self.session.set_adjustments(path.clone(), adj);
                            self.viewer.exposure = adj.exposure;
                            self.viewer.gamma    = adj.gamma;
                            self.viewer.remap_min = adj.remap_min;
                            self.viewer.remap_max = adj.remap_max;
                        }

                        ui.separator();

                        // ── Tags Section ─────────────────────────────────────
                        let tag_field_id = egui::Id::new("tag_input_field");
                        let mut tag_to_commit = None;

                        // 1. Pre-process keyboard input for the focused tag field
                        if ui.memory(|m| m.has_focus(tag_field_id)) {
                            ui.input_mut(|i| {
                                if !self.tag_suggestions.is_empty() {
                                    if i.key_pressed(egui::Key::ArrowDown) {
                                        self.tag_suggestion_idx = (self.tag_suggestion_idx + 1).min(self.tag_suggestions.len().saturating_sub(1));
                                        i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown);
                                    } else if i.key_pressed(egui::Key::ArrowUp) {
                                        self.tag_suggestion_idx = self.tag_suggestion_idx.saturating_sub(1);
                                        i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp);
                                    } else if i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Tab) {
                                        if let Some(s) = self.tag_suggestions.get(self.tag_suggestion_idx) {
                                            log::debug!("Tags: committing suggestion '{}' via keyboard", s);
                                            tag_to_commit = Some(s.clone());
                                            // suggestions will be cleared at the end of the commit block
                                            i.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
                                            i.consume_key(egui::Modifiers::NONE, egui::Key::Tab);
                                        }
                                    } else if i.key_pressed(egui::Key::Escape) {
                                        log::debug!("Tags: suggestions cleared via Escape");
                                        self.tag_suggestions.clear();
                                        i.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
                                    }
                                } else {
                                    if i.key_pressed(egui::Key::Enter) && !self.tag_input.trim().is_empty() {
                                        log::debug!("Tags: committing typed tag '{}' via Enter", self.tag_input);
                                        tag_to_commit = Some(self.tag_input.clone());
                                        i.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
                                    }
                                }
                            });
                        }

                        ui.horizontal(|ui| {
                            let text_edit = egui::TextEdit::singleline(&mut self.tag_input)
                                .hint_text("Add tag…")
                                .id(tag_field_id);
                            
                            let res = ui.add_sized([ui.available_width() - 40.0, 20.0], text_edit);
                            let popup_pos = res.rect.left_bottom();

                            // 2. Update suggestions based on text input (threshold: 2 chars)
                            if res.has_focus() && tag_to_commit.is_none() {
                                if self.tag_input.len() >= 2 {
                                    let prev_suggestions = self.tag_suggestions.clone();
                                    self.tag_suggestions = self.all_tags.iter()
                                        .filter(|t| t.name.to_lowercase().contains(&self.tag_input.to_lowercase()))
                                        .filter(|t| !self.current_image_tags.iter().any(|ct| ct.name == t.name))
                                        .take(10)
                                        .map(|t| t.name.clone())
                                        .collect();
                                    
                                    if self.tag_suggestions != prev_suggestions {
                                        self.tag_suggestion_idx = 0;
                                    }
                                } else {
                                    self.tag_suggestions.clear();
                                }
                            }

                            // 3. Render anchored dropdown and handle clicks
                            if !self.tag_suggestions.is_empty() {
                                let mut dropdown_clicked_tag = None;
                                let area_id = egui::Id::new("tag_autocomplete_area");
                                
                                egui::Area::new(area_id)
                                    .fixed_pos(popup_pos)
                                    .order(egui::Order::Foreground)
                                    .interactable(true)
                                    .show(ui.ctx(), |ui| {
                                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                                            ui.set_min_width(res.rect.width());
                                            for (i, suggestion) in self.tag_suggestions.iter().enumerate() {
                                                let is_selected = i == self.tag_suggestion_idx;
                                                let item_res = ui.selectable_label(is_selected, suggestion);
                                                if item_res.clicked() {
                                                    log::debug!("Tags: suggestion '{}' clicked", suggestion);
                                                    dropdown_clicked_tag = Some(suggestion.clone());
                                                }
                                            }
                                        });
                                    });

                                if let Some(tag) = dropdown_clicked_tag {
                                    tag_to_commit = Some(tag);
                                } else if ui.input(|i| i.pointer.any_pressed()) {
                                    // If mouse pressed but NOT on the area, clear suggestions
                                    let is_over_dropdown = ui.ctx().layer_id_at(ui.input(|i| i.pointer.interact_pos()).unwrap_or_default())
                                        .map(|l| l.id == area_id).unwrap_or(false);
                                    
                                    if !is_over_dropdown && !ui.memory(|m| m.has_focus(tag_field_id)) {
                                        self.tag_suggestions.clear();
                                    }
                                }
                            }

                            if ui.button("Add").clicked() {
                                log::debug!("Tags: committing '{}' via Add button", self.tag_input);
                                tag_to_commit = Some(self.tag_input.clone());
                            }
                        });

                        ui.add_space(4.0);

                        // Pill display with horizontal wrapping
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                            if self.current_image_tags.is_empty() {
                                ui.label(egui::RichText::new("no tags").weak().italics());
                            } else {
                                let mut tag_to_remove = None;
                                for tag in &self.current_image_tags {
                                    if draw_tag_pill(ui, tag) {
                                        tag_to_remove = Some(tag.name.clone());
                                    }
                                }

                                if let Some(name) = tag_to_remove {
                                    if let Some(path) = &self.current_path {
                                        if let (Some(db), Some(dir_str), Some(fname)) = (
                                            &self.db,
                                            path.parent().map(|p| p.to_string_lossy().into_owned()),
                                            path.file_name().and_then(|n| n.to_str()).map(str::to_string),
                                        ) {
                                            if let Ok(dir) = db.find_directory_by_path(&dir_str) {
                                                if let Some(dir) = dir {
                                                    let new_names: Vec<String> = self.current_image_tags.iter()
                                                        .filter(|t| t.name != name)
                                                        .map(|t| t.name.clone())
                                                        .collect();
                                                    let _ = db.set_image_tags(dir.id, &fname, &new_names);
                                                    log::debug!("Tags: removed '{}' from '{}'", name, fname);
                                                    self.refresh_record();
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        });

                        if let Some(tag) = tag_to_commit {
                            if !tag.trim().is_empty() {
                                let tag_clean = tag.trim().to_string();
                                if !self.current_image_tags.iter().any(|t| t.name == tag_clean) {
                                    if let Some(path) = &self.current_path {
                                        if let (Some(db), Some(dir_str), Some(fname)) = (
                                            &self.db,
                                            path.parent().map(|p| p.to_string_lossy().into_owned()),
                                            path.file_name().and_then(|n| n.to_str()).map(str::to_string),
                                        ) {
                                            if let Ok(dir) = db.upsert_directory_by_path(&dir_str) {
                                                log::debug!("Tags: SAVING '{}' to DB for '{}'", tag_clean, fname);
                                                let mut names: Vec<String> = self.current_image_tags.iter().map(|t| t.name.clone()).collect();
                                                names.push(tag_clean);
                                                if let Err(e) = db.set_image_tags(dir.id, &fname, &names) {
                                                    log::error!("Tags: DB SAVE FAILED: {}", e);
                                                }
                                                self.refresh_record();
                                                self.tag_input.clear();
                                                self.tag_suggestions.clear();
                                            }
                                        }
                                    }
                                } else {
                                    log::debug!("Tags: tag '{}' already present, skipping save", tag_clean);
                                    self.tag_input.clear();
                                    self.tag_suggestions.clear();
                                }
                            }
                        }

                        ui.separator();

                        // ── Notes Section ────────────────────────────────────
                        ui.label(egui::RichText::new("Notes:").strong());
                        let note_res = ui.add(
                            egui::TextEdit::multiline(&mut self.note_buffer)
                                .hint_text("Image notes…")
                                .desired_width(f32::INFINITY)
                                .desired_rows(1)
                                .lock_focus(true)
                        );
                        if note_res.lost_focus() {
                            if let (Some(ref db), Some(ref path), Some(ref rec)) = (&self.db, &self.current_path, &self.current_record) {
                                let note_val = if self.note_buffer.trim().is_empty() { None } else { Some(self.note_buffer.trim()) };
                                if let Err(e) = db.set_note(rec.directory_id, &rec.filename, note_val) {
                                    self.toast(format!("Failed to save note: {e}"), ToastKind::Error);
                                } else {
                                    log::debug!("Notes: saved for '{}'", rec.filename);
                                    self.refresh_record();
                                }
                            }
                        }

                        ui.separator();

                        if !self.metadata.is_empty() {
                            ui.separator();
                            ui.heading("Metadata");

                            for entry in &mut self.metadata {
                                if entry.is_header {
                                    ui.add_space(8.0);
                                    ui.heading(&entry.key);
                                    ui.separator();
                                    continue;
                                }

                                let is_multiline = entry.value.contains('\n');
                                let is_long      = entry.value.len() > 120;

                                if is_multiline || is_long {
                                    egui::CollapsingHeader::new(
                                        egui::RichText::new(&entry.key).strong()
                                    )
                                    .id_source(egui::Id::new(&entry.key))
                                    .default_open(is_multiline && matches!(
                                        entry.key.to_lowercase().as_str(),
                                        "parameters" | "prompt" | "negative prompt"
                                    ))
                                    .show(ui, |ui| {
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(&entry.value).monospace()
                                            )
                                            .selectable(true)
                                        );
                                    });
                                } else {
                                    label_kv(ui, &entry.key.replace(':', ""), entry.value.clone());
                                }
                            }
                        }
                    } else {
                        ui.label("No image loaded.");
                    }
                });
            });
    }

    fn apply_global_filter(&mut self, rf: Option<RatingFilter>, tf: crate::session::TagFilter, ctx: &Context) {
        let Some(ref db) = self.db else { return };
        match DirectoryListing::scan_global(db, rf.as_ref(), &tf) {
            Ok(listing) => {
                self.session.rating_filter = rf;
                self.session.tag_filter    = tf;
                self.listing = Some(listing);
                self.load_current(ctx, false);
            }
            Err(e) => log::warn!("failed to scan global ratings: {e}"),
        }
    }

    fn refresh_listing(&mut self, ctx: &Context) {
        let sort = self.session_sort_order();
        let db   = self.db.as_ref();
        if let Some(ref mut listing) = self.listing {
            if let Err(e) = listing.refresh(sort, db) {
                log::warn!("failed to refresh directory listing: {e}");
            }
            self.load_current(ctx, false);
        }
    }

    fn apply_local_filter(&mut self, rf: Option<RatingFilter>, tf: crate::session::TagFilter, ctx: &Context) {
        self.session.rating_filter = rf.clone();
        self.session.tag_filter    = tf.clone();
        if let Some(ref mut listing) = self.listing {
            listing.rating_filter = rf;
            listing.tag_filter    = tf;
        }
        self.refresh_listing(ctx);
    }

    fn draw_context_menu(&mut self, response: &egui::Response, ctx: &Context) {
        let has_image = self.current_path.is_some();

        response.context_menu(|ui| {
            if ui.add_enabled(has_image, egui::Button::new("Next Image")).clicked() {
                self.navigate_next(ctx, true);
                ui.close_menu();
            }
            if ui.add_enabled(has_image, egui::Button::new("Previous Image")).clicked() {
                self.navigate_prev(ctx, true);
                ui.close_menu();
            }

            ui.separator();

            ui.menu_button("Set rating", |ui| {
                for (label, r, key) in [
                    ("★ 1",       Some(1u8), "1"),
                    ("★★ 2",     Some(2),   "2"),
                    ("★★★ 3",   Some(3),   "3"),
                    ("★★★★ 4", Some(4),   "4"),
                    ("★★★★★ 5", Some(5),   "5"),
                    ("Clear",      None,      "0"),
                ] {
                    if ui.add_enabled(has_image, egui::Button::new(label).shortcut_text(key)).clicked() {
                        self.set_rating(r);
                        ui.close_menu();
                    }
                }
            });

            ui.menu_button("Filter", |ui| {
                ui.menu_button("Current folder", |ui| {
                    if ui.button("Unrated").clicked() {
                        let filter = RatingFilter {
                            op:    RatingFilterOp::Unrated,
                            value: 0,
                            path_prefix: None,
                        };
                        self.apply_local_filter(Some(filter), crate::session::TagFilter::default(), ctx);
                        ui.close_menu();
                    }
                    ui.separator();
                    for r in 1..=5 {
                        let filter = RatingFilter {
                            op:    RatingFilterOp::AtLeast,
                            value: r,
                            path_prefix: None,
                        };
                        if ui.button(format!("At least ★ {r}")).clicked() {
                            self.apply_local_filter(Some(filter), crate::session::TagFilter::default(), ctx);
                            ui.close_menu();
                        }
                    }
                });

                let has_db = self.db.is_some();
                ui.add_enabled_ui(has_db, |ui| {
                    ui.menu_button("Current folder & subfolders", |ui| {
                        if ui.button("Unrated").clicked() {
                            let prefix = self.listing.as_ref().map(|l| l.dir_path.clone());
                            let filter = RatingFilter {
                                op:    RatingFilterOp::Unrated,
                                value: 0,
                                path_prefix: prefix,
                            };
                            self.apply_global_filter(Some(filter), crate::session::TagFilter::default(), ctx);
                            ui.close_menu();
                        }
                        ui.separator();
                        for r in 1..=5 {
                            let prefix = self.listing.as_ref().map(|l| l.dir_path.clone());
                            let filter = RatingFilter {
                                op:    RatingFilterOp::AtLeast,
                                value: r,
                                path_prefix: prefix,
                            };
                            if ui.button(format!("At least ★ {r}")).clicked() {
                                self.apply_global_filter(Some(filter), crate::session::TagFilter::default(), ctx);
                                ui.close_menu();
                            }
                        }
                    });

                    ui.menu_button("Library", |ui| {
                        if ui.button("Unrated").clicked() {
                            let filter = RatingFilter {
                                op:    RatingFilterOp::Unrated,
                                value: 0,
                                path_prefix: None,
                            };
                            self.apply_global_filter(Some(filter), crate::session::TagFilter::default(), ctx);
                            ui.close_menu();
                        }
                        ui.separator();
                        for r in 1..=5 {
                            let filter = RatingFilter {
                                op:    RatingFilterOp::AtLeast,
                                value: r,
                                path_prefix: None,
                            };
                            if ui.button(format!("At least ★ {r}")).clicked() {
                                self.apply_global_filter(Some(filter), crate::session::TagFilter::default(), ctx);
                                ui.close_menu();
                            }
                        }
                    });
                });

                if ui.button("Clear Filter").clicked() {
                    self.apply_local_filter(None, crate::session::TagFilter::default(), ctx);
                    ui.close_menu();
                }
            });

            ui.menu_button("Tags", |ui| {
                if self.all_tags.is_empty() {
                    ui.label(egui::RichText::new("No tags in library").small().weak());
                } else {
                    let tags = self.all_tags.clone();
                    for tag in &tags {
                        let mut is_filtered = self.session.tag_filter.tags.contains(&tag.name);
                        if ui.checkbox(&mut is_filtered, &tag.name).changed() {
                            let mut tf = self.session.tag_filter.clone();
                            if is_filtered { tf.tags.insert(tag.name.clone()); }
                            else { tf.tags.remove(&tag.name); }
                            let rf = self.session.rating_filter.clone();
                            self.apply_local_filter(rf, tf, ctx);
                        }
                    }
                }
            });

            ui.menu_button("Sort by", |ui| {
                use crate::settings::SortOrder;
                for (label, order) in [
                    ("Name",          SortOrder::Name),
                    ("Date Modified", SortOrder::DateModified),
                    ("File Size",     SortOrder::FileSize),
                ] {
                    let is_selected = self.session.sort_order == order;
                    if ui.selectable_label(is_selected, label).clicked() {
                        self.apply_sort_order(order, ctx);
                        ui.close_menu();
                    }
                }
            });

            ui.separator();

            if ui.add_enabled(has_image, egui::Button::new("Hide image").shortcut_text("H")).clicked() {
                self.hide_current(ctx);
                ui.close_menu();
            }

            if ui.add_enabled(has_image, egui::Button::new("Delete").shortcut_text("Del"))
                .on_hover_text("Two-step confirmation required")
                .clicked()
            {
                self.confirm_delete();
                ui.close_menu();
            }

            ui.separator();

            if ui.add_enabled(has_image, egui::Button::new("Rotate Clockwise").shortcut_text("]")).clicked() {
                self.rotate_current(true, ctx);
                ui.close_menu();
            }
            if ui.add_enabled(has_image, egui::Button::new("Rotate Counter-Clockwise").shortcut_text("[")).clicked() {
                self.rotate_current(false, ctx);
                ui.close_menu();
            }

            ui.separator();

            if ui.add_enabled(has_image, egui::Button::new("Copy path")).clicked() {
                if let Some(ref p) = self.current_path {
                    ctx.copy_text(p.to_string_lossy().into_owned());
                }
                ui.close_menu();
            }
            if ui.add_enabled(has_image, egui::Button::new("Copy Image").shortcut_text("Ctrl+C")).clicked() {
                self.copy_to_clipboard();
                ui.close_menu();
            }
            if ui.add_enabled(has_image, egui::Button::new("Open folder")).clicked() {
                self.reveal_in_file_manager();
                ui.close_menu();
            }
            if ui.add_enabled(has_image, egui::Button::new("Save as...").shortcut_text("Ctrl+Shift+S")).clicked() {
                self.save_as(ctx);
                ui.close_menu();
            }

            ui.separator();

            let info_label = if self.show_info_panel { "Hide info" } else { "Show info" };
            if ui.add(egui::Button::new(info_label).shortcut_text("I")).clicked() {
                self.show_info_panel = !self.show_info_panel;
                self.settings.show_info_panel = self.show_info_panel;
                let _ = self.settings.save();
                ui.close_menu();
            }

            let fit_label = if self.viewer.fit_to_window {
                "Actual size"
            } else {
                "Fit to window"
            };
            let fit_shortcut = if self.viewer.fit_to_window { "Ctrl+0" } else { "F" };
            if ui.add(egui::Button::new(fit_label).shortcut_text(fit_shortcut)).clicked() {
                if self.viewer.fit_to_window {
                    self.viewer.zoom_actual_size();
                } else {
                    self.viewer.toggle_fit(ctx.screen_rect().size());
                }
                ui.close_menu();
            }

            ui.separator();

            if ui.add(egui::Button::new("Reset Session").shortcut_text("Ctrl+Shift+R")).clicked() {
                self.hard_refresh(ctx);
                ui.close_menu();
            }

            ui.separator();

            ui.menu_button("Utilities", |ui| {
                let base_path = self.listing
                    .as_ref()
                    .map(|l| l.dir_path.clone())
                    .or_else(|| self.current_path.as_ref().and_then(|p| p.parent()).map(|p| p.to_path_buf()))
                    .unwrap_or_default();
                let has_path = !base_path.as_os_str().is_empty();

                if ui.add_enabled(has_path, egui::Button::new("File System Purge…"))
                    .on_hover_text("Delete files by rating within the current directory")
                    .clicked()
                {
                    self.utilities.open_purge(base_path);
                    ui.close_menu();
                }

                if ui.add_enabled(self.db.is_some(), egui::Button::new("Database Health Check…"))
                    .on_hover_text("Find and remove database entries for missing files")
                    .clicked()
                {
                    self.utilities.open_db_health(self.db.as_ref());
                    ui.close_menu();
                }

                if ui.add_enabled(self.db.is_some(), egui::Button::new("Tag Manager…"))
                    .clicked()
                {
                    self.utilities.open_tag_editor(self.db.as_ref());
                    ui.close_menu();
                }

                ui.separator();

                let has_metadata = !self.metadata.is_empty();
                let is_stripped = self.current_path.as_ref().map(|p| self.session.is_metadata_stripped(p)).unwrap_or(false);
                let strip_label = if is_stripped { "Unstage metadata strip" } else { "Strip metadata" };
                if ui.add_enabled(has_image && has_metadata, egui::Button::new(strip_label)).clicked() {
                    if let Some(path) = self.current_path.clone() {
                        self.session.toggle_metadata_strip(path);
                    }
                    ui.close_menu();
                }
            });

            ui.separator();

            ui.vertical_centered(|ui| {
                ui.add_space(2.0);
                ui.label(egui::RichText::new(format!("Rivett v{}", env!("CARGO_PKG_VERSION")))
                    .small()
                    .color(egui::Color32::from_gray(120)));
                ui.hyperlink_to(
                    egui::RichText::new("github.com/krets/rivett").small(),
                    "https://github.com/krets/rivett"
                );
            });
        });
    }
}

// ---------------------------------------------------------------------------
// Rotation save helpers (free functions)
// ---------------------------------------------------------------------------

/// Save rotation for JPEG by updating the EXIF Orientation tag in-place.
/// No pixel data is changed — purely a metadata update.
fn save_jpeg_exif_rotation(path: &Path, rotation: Rotation) -> Result<(), String> {
    use img_parts::{ImageEXIF, jpeg::Jpeg, Bytes}; // crate name: img-parts

    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let mut jpeg = Jpeg::from_bytes(Bytes::from(data)).map_err(|e| e.to_string())?;

    // The loader already bakes the current EXIF orientation into decoded pixels.
    // New file orientation = (current EXIF) + (session rotation).
    let current_exif_rot = crate::metadata::get_orientation(path)
        .map(exif_orientation_to_rotation)
        .unwrap_or(Rotation::None);
    let total = combine_rotations(current_exif_rot, rotation);
    let new_orientation = rotation_to_exif_orientation(total);

    let exif_bytes: Vec<u8> = match jpeg.exif() {
        Some(existing) => {
            let mut bytes = existing.to_vec();
            if !patch_exif_orientation(&mut bytes, new_orientation) {
                // EXIF exists but has no Orientation tag — we cannot insert one without
                // a full TIFF offset fixup, which would risk corrupting other metadata.
                return Err(
                    "JPEG has EXIF data but no Orientation tag; \
                     cannot save rotation without risking metadata loss".into()
                );
            }
            bytes
        }
        None => build_minimal_exif(new_orientation),
    };

    jpeg.set_exif(Some(Bytes::from(exif_bytes)));
    let out = jpeg.encoder().bytes();
    std::fs::write(path, out.as_ref()).map_err(|e| e.to_string())?;
    Ok(())
}

/// Convert pure-rotation EXIF orientation values to our Rotation enum.
fn exif_orientation_to_rotation(o: u32) -> Rotation {
    match o {
        6 => Rotation::Cw90,
        3 => Rotation::Cw180,
        8 => Rotation::Cw270,
        _ => Rotation::None,
    }
}

/// Map our Rotation to the EXIF Orientation tag value for pure rotations.
fn rotation_to_exif_orientation(r: Rotation) -> u16 {
    match r {
        Rotation::None  => 1,
        Rotation::Cw90  => 6,
        Rotation::Cw180 => 3,
        Rotation::Cw270 => 8,
    }
}

/// Save the image to a new path with optional metadata preservation.
fn save_image_as(
    src_path: &Path,
    dst_path: &Path,
    rotation: Rotation,
    cached: Option<&crate::image_loader::DecodedImage>,
    preserve_metadata: bool,
) -> Result<(), String> {
    use img_parts::{Bytes, ImageEXIF, jpeg::Jpeg, png::Png};

    // 1. Get the source image
    let decoded = cached
        .ok_or_else(|| "image not in cache — navigate away and back, then retry".to_string())?;
    let src = image::RgbaImage::from_raw(decoded.width, decoded.height, decoded.to_u8())
        .ok_or("invalid pixel buffer")?;
    let rotated = match rotation {
        Rotation::None  => image::DynamicImage::ImageRgba8(src),
        Rotation::Cw90  => image::DynamicImage::ImageRgba8(image::imageops::rotate90(&src)),
        Rotation::Cw180 => image::DynamicImage::ImageRgba8(image::imageops::rotate180(&src)),
        Rotation::Cw270 => image::DynamicImage::ImageRgba8(image::imageops::rotate270(&src)),
    };

    // 2. Determine output format
    let ext = dst_path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
    let format = match ext.as_str() {
        "jpg" | "jpeg" => image::ImageFormat::Jpeg,
        "png" => image::ImageFormat::Png,
        _ => return Err("Unsupported output format (use .jpg or .png)".to_string()),
    };

    // 3. Encode image
    let mut encoded_data = Vec::new();
    rotated.write_to(&mut std::io::Cursor::new(&mut encoded_data), format).map_err(|e| e.to_string())?;

    // 4. Preserve metadata if requested
    if preserve_metadata {
        if format == image::ImageFormat::Jpeg {
            if let Ok(src_bytes) = std::fs::read(src_path) {
                let exif = if let Ok(src_jpeg) = Jpeg::from_bytes(Bytes::from(src_bytes.clone())) {
                    src_jpeg.exif().map(|b| b.clone())
                } else {
                    None
                };

                if let Some(exif_bytes) = exif {
                    if let Ok(mut dst_jpeg) = Jpeg::from_bytes(Bytes::from(encoded_data.clone())) {
                        dst_jpeg.set_exif(Some(exif_bytes));
                        encoded_data = dst_jpeg.encoder().bytes().to_vec();
                    }
                }
            }
        } else if format == image::ImageFormat::Png {
            if let Ok(src_bytes) = std::fs::read(src_path) {
                if let Ok(src_png) = Png::from_bytes(Bytes::from(src_bytes)) {
                    if let Ok(mut dst_png) = Png::from_bytes(Bytes::from(encoded_data.clone())) {
                        for chunk in src_png.chunks() {
                            let kind = chunk.kind();
                            if kind == *b"tEXt" || kind == *b"iTXt" || kind == *b"zTXt" || kind == *b"eXIf" {
                                let len = dst_png.chunks().len();
                                if len > 0 {
                                    dst_png.chunks_mut().insert(len - 1, chunk.clone());
                                } else {
                                    dst_png.chunks_mut().push(chunk.clone());
                                }
                            }
                        }
                        encoded_data = dst_png.encoder().bytes().to_vec();
                    }
                }
            }
        }
    }

    std::fs::write(dst_path, encoded_data).map_err(|e| e.to_string())?;
    Ok(())
}

/// Combine two rotations: result is `a` followed by `b`.
fn combine_rotations(a: Rotation, b: Rotation) -> Rotation {
    let steps = (a.as_u8() + b.as_u8()) % 4;
    Rotation::from_u8(steps)
}

/// Scan EXIF bytes for the Orientation IFD entry and overwrite the value.
/// `bytes` includes the "Exif\0\0" header (6 bytes) followed by a TIFF structure.
/// Returns `true` if the tag was found and patched.
/// `bytes` is pure TIFF data — img_parts strips the "Exif\0\0" prefix before returning
/// from `exif()` and re-adds it in `set_exif()`, so we never see or emit that prefix.
fn patch_exif_orientation(bytes: &mut Vec<u8>, new_value: u16) -> bool {
    if bytes.len() < 8 { return false; }

    let little_endian = bytes[0] == b'I' && bytes[1] == b'I';
    let read_u16 = |b: &[u8], off: usize| -> u16 {
        if off + 2 > b.len() { return 0; }
        if little_endian { u16::from_le_bytes([b[off], b[off+1]]) }
        else             { u16::from_be_bytes([b[off], b[off+1]]) }
    };
    let read_u32 = |b: &[u8], off: usize| -> u32 {
        if off + 4 > b.len() { return 0; }
        if little_endian { u32::from_le_bytes([b[off], b[off+1], b[off+2], b[off+3]]) }
        else             { u32::from_be_bytes([b[off], b[off+1], b[off+2], b[off+3]]) }
    };

    let ifd_offset = read_u32(bytes, 4) as usize;
    if ifd_offset + 2 > bytes.len() { return false; }
    let entry_count = read_u16(bytes, ifd_offset) as usize;

    for i in 0..entry_count {
        let entry_off = ifd_offset + 2 + i * 12;
        if entry_off + 12 > bytes.len() { break; }
        let tag = read_u16(bytes, entry_off);
        if tag == 0x0112 {
            let val_off = entry_off + 8;
            if val_off + 2 > bytes.len() { return false; }
            let encoded = if little_endian { new_value.to_le_bytes() } else { new_value.to_be_bytes() };
            bytes[val_off]     = encoded[0];
            bytes[val_off + 1] = encoded[1];
            return true;
        }
    }
    false
}

/// Build minimal pure TIFF data containing only the Orientation tag.
/// img_parts prepends "Exif\0\0" automatically in set_exif(), so we must not include it.
/// Layout: TIFF header (8 bytes) + IFD entry count (2) + 1 entry (12) + IFD terminator (4).
fn build_minimal_exif(orientation: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(26);
    // TIFF header: little-endian, magic 42, IFD offset = 8
    b.extend_from_slice(&[b'I', b'I', 42, 0]);
    b.extend_from_slice(&8u32.to_le_bytes());
    // IFD: 1 entry
    b.extend_from_slice(&1u16.to_le_bytes());
    // IFD entry: tag=0x0112 (Orientation), type=SHORT(3), count=1, value
    b.extend_from_slice(&0x0112u16.to_le_bytes());
    b.extend_from_slice(&3u16.to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&(orientation as u32).to_le_bytes());
    // IFD terminator
    b.extend_from_slice(&0u32.to_le_bytes());
    b
}

/// Save rotation for non-JPEG formats by pixel-rotating the cached image and re-encoding.
fn save_pixel_rotation(
    path: &Path,
    fmt: SupportedFormat,
    rotation: Rotation,
    cached: Option<crate::image_loader::DecodedImage>,
) -> Result<(), String> {
    let decoded = cached
        .ok_or_else(|| "image not in cache — navigate away and back, then retry".to_string())?;
    let src = image::RgbaImage::from_raw(decoded.width, decoded.height, decoded.to_u8())
        .ok_or("invalid pixel buffer")?;
    let rotated = match rotation {
        Rotation::None  => image::DynamicImage::ImageRgba8(src),
        Rotation::Cw90  => image::DynamicImage::ImageRgba8(image::imageops::rotate90(&src)),
        Rotation::Cw180 => image::DynamicImage::ImageRgba8(image::imageops::rotate180(&src)),
        Rotation::Cw270 => image::DynamicImage::ImageRgba8(image::imageops::rotate270(&src)),
    };

    let img_fmt = match fmt {
        SupportedFormat::Png  => image::ImageFormat::Png,
        SupportedFormat::WebP => image::ImageFormat::WebP,
        SupportedFormat::Bmp  => image::ImageFormat::Bmp,
        SupportedFormat::Tiff => image::ImageFormat::Tiff,
        SupportedFormat::Gif  => image::ImageFormat::Gif,
        SupportedFormat::Exr  => image::ImageFormat::OpenExr,
        _    => return Err(format!("unsupported format for pixel rotation: {:?}", fmt)),
    };

    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut buf = std::io::BufWriter::new(file);
    rotated.write_to(&mut buf, img_fmt).map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// eframe::App
// ---------------------------------------------------------------------------

impl eframe::App for RivettApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        let old_cache_len = self.image_cache.len();
        self.image_cache.poll();

        // If the viewer is waiting for an image that is no longer pending and never
        // arrived (failed decode), clear the loading state so we don't spin forever.
        if self.viewer.loading {
            if let Some(ref path) = self.current_path.clone() {
                if !self.image_cache.is_pending(path) && !self.image_cache.contains(path) {
                    self.viewer.set_error("Failed to load image".to_string());
                }
            }
        }

        // If something was loaded into the cache, check if it's the current image
        // that was previously in a loading state.
        if self.image_cache.len() > old_cache_len {
            if let Some(path) = self.current_path.clone() {
                if self.viewer.loading && self.image_cache.contains(&path) {
                    self.load_current(ctx, true);
                }
                // Also queue upload for newly cached neighbors.
                let neighbor_imgs: Vec<_> = self.neighbor_paths(1).into_iter().filter_map(|p| {
                    self.image_cache.get(&p).map(|img| (p, img.clone()))
                }).collect();
                for (p, img) in neighbor_imgs {
                    self.queue_upload_for(p, img);
                }
            }
        }

        // Request next frame only if there's actual in-flight work. Checked here —
        // after poll() and the GPU queue update — so the state is current.
        if self.image_cache.has_any_pending()
            || !self.gpu_upload_queue.is_empty()
            || self.gpu_upload_pending
            || self.viewer.loading
        {
            ctx.request_repaint();
        }

        // Execute any pending drag-out here, at the very top of the update loop,
        // before egui opens any closures. DoDragDrop must run while the main thread's
        // WndProc is in a clean state — calling it mid-closure breaks message routing.
        if self.pending_drag_out {
            self.pending_drag_out = false;
            self.execute_drag_out();
        }

        self.handle_keyboard(ctx);

        let hovered_files = ctx.input(|i| i.raw.hovered_files.clone());
        if !hovered_files.is_empty() {
            let screen = ctx.screen_rect();
            let overlay = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground, egui::Id::new("drop_overlay"),
            ));
            overlay.rect_filled(screen, 0.0, egui::Color32::from_rgba_unmultiplied(0, 0, 0, 110));
            overlay.text(
                screen.center(), egui::Align2::CENTER_CENTER,
                "Drop image to open",
                egui::FontId::proportional(28.0), egui::Color32::WHITE,
            );
        }

        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        for file in dropped {
            if let Some(path) = file.path {
                self.open_image(path, ctx);
                break;
            }
        }

        if let Some(ref dc) = self.delete_confirm {
            if !dc.alive() { self.delete_confirm = None; }
        }

        ctx.send_viewport_cmd(egui::ViewportCommand::Title(self.window_title()));

        if self.show_info_panel {
            self.draw_info_panel(ctx);
        }

        self.draw_save_as_modal(ctx);
        self.utilities.draw(ctx, self.db.as_ref());
        self.draw_help_overlay(ctx);

        CentralPanel::default().show(ctx, |ui| {
            let canvas = ui.max_rect();
            self.viewer.recalc_fit(ui.available_size());

            let response = ui.allocate_rect(canvas, egui::Sense::click_and_drag());

            let ctrl_held = ctx.input(|i| i.modifiers.ctrl);

            // Detect drag-out gesture; schedule for execution at the top of the next frame.
            let drag_out_trigger = response.drag_started_by(egui::PointerButton::Primary) && ctrl_held;
            if drag_out_trigger && !self.pending_drag_out && self.current_path.is_some() {
                self.pending_drag_out = true;
            }

            // Pan: primary drag only when Ctrl is not held
            if response.dragged_by(egui::PointerButton::Primary) && !ctrl_held {
                self.viewer.fit_to_window = false;
                self.viewer.pan += response.drag_delta();
            }

            if response.hovered() {
                let (scroll_y, zoom_delta) = ctx.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
                if zoom_delta != 1.0 {
                    let cursor = ctx.input(|i| i.pointer.latest_pos());
                    self.viewer.apply_zoom_delta(zoom_delta, cursor, canvas);
                } else if scroll_y != 0.0 {
                    let factor = if scroll_y > 0.0 { 1.1_f32 } else { 1.0 / 1.1 };
                    let cursor = ctx.input(|i| i.pointer.latest_pos());
                    self.viewer.apply_zoom_delta(factor, cursor, canvas);
                }
            }

            if response.double_clicked() {
                if let Some(ref err) = self.viewer.load_error {
                    ctx.copy_text(err.clone());
                    self.toast("Error message copied to clipboard", ToastKind::General);
                } else {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Images", &[
                            "png", "jpg", "jpeg", "webp", "bmp", "tiff", "tif", "gif", "exr", "svg",
                            "arw", "cr2", "cr3", "nef", "nrw", "orf", "raf", "rw2", "dng"
                        ])
                        .pick_file()
                    {
                        self.open_image(path, ctx);
                    }
                }
            }


            self.draw_context_menu(&response, ctx);

            // Process one GPU pre-upload per frame (from the rotation pipeline).
            // This runs regardless of whether viewer has a texture, so uploads
            // can proceed even while the loading spinner is showing.
            if let Some((path, rgba, w, h)) = self.gpu_upload_queue.pop_front() {
                let is_current = self.current_path.as_ref() == Some(&path);

                // Keep: current image ± 1 neighbors.
                let keep: HashSet<std::path::PathBuf> = {
                    let mut s = HashSet::new();
                    if let Some(ref p) = self.current_path { s.insert(p.clone()); }
                    for n in self.neighbor_paths(1) { s.insert(n); }
                    s
                };

                let renderer = self.gamma_renderer.get_or_insert_with(|| {
                    let gl = cc_gl_from_ctx(ctx).expect("Glow context not found");
                    Arc::new(Mutex::new(GammaRenderer::new(&gl)))
                }).clone();

                let path_clone = path.clone();
                let keep_closure = keep.clone();
                ui.painter().add(egui::PaintCallback {
                    rect: canvas,
                    callback: Arc::new(egui_glow::CallbackFn::new(move |_info, painter| {
                        let mut r = renderer.lock().unwrap();
                        r.cache_texture(painter.gl(), path_clone.clone(), w, h, &rgba);
                        r.evict_stale(painter.gl(), &keep_closure);
                    })),
                });

                if is_current {
                    if let Some(ref r) = self.gamma_renderer {
                        r.lock().unwrap().set_active(path.clone());
                    }
                    self.gpu_upload_pending = false;
                }

                ctx.request_repaint();
            }

            if self.viewer.texture.is_some() {
                let rect = self.viewer.image_rect(canvas);

                // Render with gamma shader
                if let Some(renderer) = &self.gamma_renderer {
                    let renderer = renderer.clone();
                    let adj = crate::session::ImageAdjustments {
                        exposure:  self.viewer.exposure,
                        gamma:     self.viewer.gamma,
                        remap_min: self.viewer.remap_min,
                        remap_max: self.viewer.remap_max,
                    };
                    let rotation = self.session.rotation_for(self.current_path.as_ref().unwrap());
                    ui.painter().add(egui::PaintCallback {
                        rect: canvas, // Cover the entire canvas to allow for zoom/pan clipping
                        callback: Arc::new(egui_glow::CallbackFn::new(move |_info, painter| {
                            let renderer = renderer.lock().unwrap();
                            renderer.paint(painter.gl(), rect, canvas, adj, rotation);
                        })),
                    });
                } else {
                    // Fallback to standard egui image if renderer is not initialized
                    if let Some(ref texture) = self.viewer.texture {
                        ui.painter().image(
                            texture.id(), rect,
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );
                    }
                }

                // Corner spinner: overlays the previous image while the next one loads.
                if self.gpu_upload_pending || self.viewer.loading {
                    let badge_center = egui::pos2(canvas.min.x + 22.0, canvas.min.y + 22.0);
                    let badge_rect = egui::Rect::from_center_size(badge_center, egui::vec2(28.0, 28.0));
                    ui.put(badge_rect, egui::Spinner::new().size(18.0));
                }
            } else if self.viewer.loading {
                ui.centered_and_justified(|ui| {
                    ui.add(egui::Spinner::new().size(40.0));
                });
            } else if let Some(ref err) = self.viewer.load_error {
                ui.painter().text(
                    canvas.center(), egui::Align2::CENTER_CENTER,
                    format!("Error loading image:\n{err}"),
                    egui::FontId::proportional(18.0),
                    egui::Color32::LIGHT_RED,
                );
            } else {
                ui.painter().text(
                    canvas.center(), egui::Align2::CENTER_CENTER,
                    "Drag an image here, or double-click to open",
                    egui::FontId::proportional(18.0),
                    egui::Color32::from_gray(130),
                );
            }

            let current_has_changes = self.current_path.as_ref().map(|p| {
                self.session.pending_rotations.contains_key(p)
                    || self.session.pending_crops.contains_key(p)
                    || self.session.pending_metadata_strips.contains(p)
            }).unwrap_or(false);
            if current_has_changes {
                let dot_pos = egui::pos2(canvas.max.x - 14.0, canvas.min.y + 14.0);
                let response = ui.interact(
                    egui::Rect::from_center_size(dot_pos, egui::vec2(12.0, 12.0)),
                    egui::Id::new("modified_badge"),
                    egui::Sense::hover(),
                );
                response.on_hover_text("Unsaved changes (rotation, crops, metadata) — Ctrl+S to save");
                ui.painter().circle_filled(dot_pos, 6.0, egui::Color32::from_rgb(255, 180, 0));
            }

            if self.delete_confirm.as_ref().map(|d| d.alive()).unwrap_or(false) {
                let bg = egui::Color32::from_rgba_unmultiplied(180, 30, 30, 210);
                let msg_rect = egui::Rect::from_center_size(
                    canvas.center(),
                    egui::vec2(420.0, 56.0),
                );
                ui.painter().rect_filled(msg_rect, 6.0, bg);
                ui.painter().text(
                    msg_rect.center(), egui::Align2::CENTER_CENTER,
                    "Press Delete to confirm — Esc to cancel",
                    egui::FontId::proportional(16.0), egui::Color32::WHITE,
                );
            }
        });

        if let Some(ref toast) = self.toast {
            let alpha = toast.alpha();
            if alpha > 0.0 {
                let screen  = ctx.screen_rect();
                let painter = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Tooltip, egui::Id::new("toast"),
                ));

                let font = egui::FontId::proportional(16.0);
                let galley = ctx.fonts(|f| f.layout_no_wrap(
                    toast.message.clone(), font.clone(),
                    egui::Color32::WHITE,
                ));
                let pad    = egui::vec2(16.0, 8.0);
                let size   = galley.size() + pad * 2.0;
                let center = egui::pos2(screen.center().x, screen.max.y - 48.0);
                let rect   = egui::Rect::from_center_size(center, size);

                let a = (alpha * 200.0) as u8;
                let bg_color = if toast.kind == ToastKind::Error {
                    egui::Color32::from_rgba_unmultiplied(180, 30, 30, a)
                } else {
                    egui::Color32::from_rgba_unmultiplied(30, 30, 30, a)
                };
                painter.rect_filled(rect, 6.0, bg_color);
                painter.galley(rect.min + pad, galley, egui::Color32::from_rgba_unmultiplied(255, 255, 255, (alpha * 255.0) as u8));
                ctx.request_repaint();
            }
        }

        if self.toast.as_ref().map(|t| !t.alive()).unwrap_or(false) {
            self.toast = None;
        }
    }
}

fn cc_gl_from_ctx(ctx: &Context) -> Option<Arc<glow::Context>> {
    ctx.memory(|mem| mem.data.get_temp::<SendSyncGl>(egui::Id::new("gl_context")).map(|s| s.0))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[derive(PartialEq)]
enum ToastKind {
    General,
    ImageStatus,
    Error,
}

struct Toast {
    message: String,
    start:   Instant,
    kind:    ToastKind,
}

impl Toast {
    fn new(message: String, kind: ToastKind) -> Self {
        Self { message, start: Instant::now(), kind }
    }
    fn alive(&self) -> bool {
        self.start.elapsed() < Duration::from_secs(3)
    }
    fn alpha(&self) -> f32 {
        let elapsed = self.start.elapsed().as_secs_f32();
        if elapsed < 0.2 { elapsed / 0.2 }
        else if elapsed > 2.5 { 1.0 - (elapsed - 2.5) / 0.5 }
        else { 1.0 }
    }
}

struct DeleteConfirm {
    start: Instant,
}

impl DeleteConfirm {
    fn new() -> Self {
        Self { start: Instant::now() }
    }
    fn alive(&self) -> bool {
        self.start.elapsed() < Duration::from_secs(4)
    }
}

// ---------------------------------------------------------------------------
// Drag-out window handle helper
// ---------------------------------------------------------------------------

/// Wraps a raw Win32 HWND (as isize) so it can be passed to the `drag` crate
/// across thread boundaries. The HWND is valid for the lifetime of the app window.
#[cfg(target_os = "windows")]
struct OwnedWindowHandle(isize);

#[cfg(target_os = "windows")]
impl raw_window_handle::HasWindowHandle for OwnedWindowHandle {
    fn window_handle(&self) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let hwnd = std::num::NonZeroIsize::new(self.0)
            .ok_or(raw_window_handle::HandleError::NotSupported)?;
        let handle = raw_window_handle::Win32WindowHandle::new(hwnd);
        // SAFETY: the HWND is valid while the app window exists and DoDragDrop runs
        unsafe {
            Ok(raw_window_handle::WindowHandle::borrow_raw(
                raw_window_handle::RawWindowHandle::Win32(handle),
            ))
        }
    }
}

#[cfg(target_os = "windows")]
impl raw_window_handle::HasDisplayHandle for OwnedWindowHandle {
    fn display_handle(&self) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        unsafe {
            Ok(raw_window_handle::DisplayHandle::borrow_raw(
                raw_window_handle::RawDisplayHandle::Windows(
                    raw_window_handle::WindowsDisplayHandle::new(),
                ),
            ))
        }
    }
}

// ── Tag UI Helpers ───────────────────────────────────────────────────────────

fn text_color_for_bg(bg: egui::Color32) -> egui::Color32 {
    let perceived = (0.299 * bg.r() as f32
        + 0.587 * bg.g() as f32
        + 0.114 * bg.b() as f32) / 255.0;

    if perceived > 0.5 {
        egui::Color32::from_rgb(20, 20, 20)   // dark text on light bg
    } else {
        egui::Color32::from_rgb(240, 240, 240) // light text on dark bg
    }
}

fn draw_tag_pill(ui: &mut egui::Ui, tag: &TagRecord) -> bool {
    let padding = egui::vec2(6.0, 2.0);
    let x_gap = 4.0;
    let font_id = egui::FontId::proportional(12.0);
    
    let name_galley = ui.fonts(|f| f.layout_no_wrap(tag.name.clone(), font_id.clone(), egui::Color32::WHITE));
    let x_galley = ui.fonts(|f| f.layout_no_wrap("✖".to_string(), font_id.clone(), egui::Color32::WHITE));
    
    let size = egui::vec2(
        padding.x + name_galley.size().x + x_gap + x_galley.size().x + padding.x,
        (name_galley.size().y.max(x_galley.size().y) + padding.y * 2.0).max(18.0)
    );
    
    let (rect, response) = ui.allocate_at_least(size, egui::Sense::click());
    
    let bg_color = egui::Color32::from_rgb(
        ((tag.color >> 16) & 0xFF) as u8,
        ((tag.color >> 8) & 0xFF) as u8,
        (tag.color & 0xFF) as u8,
    );
    let fg_color = text_color_for_bg(bg_color);
    
    ui.painter().rect_filled(rect, 4.0, bg_color);
    
    // Draw text
    let text_pos = rect.left_center() + egui::vec2(padding.x, 0.0);
    ui.painter().text(text_pos, egui::Align2::LEFT_CENTER, &tag.name, font_id.clone(), fg_color);
    
    // Draw 'x'
    let x_pos = rect.right_center() - egui::vec2(padding.x, 0.0);
    ui.painter().text(x_pos, egui::Align2::RIGHT_CENTER, "✖", font_id, fg_color);

    response.clicked()
}
