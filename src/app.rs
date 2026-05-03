//! Top-level application state and the [`eframe::App`] implementation.

use eframe::CreationContext;
use egui::{CentralPanel, Context, Key, Vec2};
use std::time::{Duration, Instant};

use crate::db::{Database, ImageRecord};
use std::path::Path;
use crate::image_loader::{load_image, ImageCache, DirectoryListing};
use crate::metadata::{read_metadata, MetaEntry};
use crate::formats::SupportedFormat;
use crate::session::{SessionState, Rotation, RatingFilter, RatingFilterOp};
use crate::settings::AppSettings;
use crate::viewer::ViewerState;

// ---------------------------------------------------------------------------
// RivettApp
// ---------------------------------------------------------------------------

pub struct RivettApp {
    db:              Option<Database>,
    viewer:          ViewerState,
    image_cache:     ImageCache,
    listing:         Option<DirectoryListing>,
    session:         SessionState,

    // UI state
    current_path:    Option<std::path::PathBuf>,
    current_record:  Option<ImageRecord>,
    metadata:        Vec<MetaEntry>,
    show_info_panel: bool,
    toast:           Option<Toast>,
    delete_confirm:  Option<DeleteConfirm>,

    // Drag-out state
    pending_drag_out:  bool, // set on gesture detection; consumed at top of next update()
    middle_btn_on_canvas: bool, // tracks middle button pressed-while-hovering canvas

    #[allow(dead_code)]
    settings:        AppSettings,
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

        let mut app = Self {
            db,
            viewer:          ViewerState::new(),
            image_cache:     ImageCache::new(32),
            listing:         None,
            session:         SessionState::new(settings.default_sort),
            current_path:    None,
            current_record:  None,
            metadata:        vec![],
            show_info_panel: settings.show_info_panel,
            toast:           None,
            delete_confirm:  None,
            pending_drag_out:     false,
            middle_btn_on_canvas: false,
            settings,
        };

        if let Some(path) = initial_image {
            app.open_image(path, &cc.egui_ctx);
        }

        app
    }

    // ── Toast helper ──────────────────────────────────────────────────────

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some(Toast::new(msg.into()));
    }

    // ── Opening / Loading ─────────────────────────────────────────────────

    pub fn open_image(&mut self, path: std::path::PathBuf, ctx: &Context) {
        if !path.exists() { return; }
        
        if path.is_file() {
            if let Some(dir) = path.parent() {
                let sort   = self.session_sort_order();
                let db     = self.db.as_ref();
                match DirectoryListing::scan(dir, sort, None, db) {
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

        self.current_path = Some(path.clone());
        
        self.refresh_record();

        let rotation = self.session.rotation_for(&path);

        if let Some(img) = self.image_cache.get(&path) {
            self.viewer.load_image(ctx, img, rotation, preserve_zoom);
        } else {
            match load_image(&path) {
                Ok(img) => {
                    self.image_cache.insert(path.clone(), img.clone());
                    self.viewer.load_image(ctx, &img, rotation, preserve_zoom);
                }
                Err(e)  => {
                    log::warn!("{e}");
                    self.viewer.set_error(e);
                }
            }
        }

        if let Some(ref listing) = self.listing {
            // Next
            let mut i = listing.current_index + 1;
            while i < listing.files.len() {
                let p = &listing.files[i];
                if !self.session_is_ignored(p) {
                    self.image_cache.prefetch(p.clone());
                    break;
                }
                i += 1;
            }
            // Prev
            let mut i = listing.current_index as i32 - 1;
            while i >= 0 {
                let p = &listing.files[i as usize];
                if !self.session_is_ignored(p) {
                    self.image_cache.prefetch(p.clone());
                    break;
                }
                i -= 1;
            }
        }

        self.metadata = read_metadata(&path);
    }

    fn refresh_record(&mut self) {
        self.current_record = self.current_path.as_ref().and_then(|path| {
            let db      = self.db.as_ref()?;
            let dir_str = path.parent()?.to_string_lossy();
            let fname   = path.file_name()?.to_str()?;
            let dir     = db.find_directory_by_path(&dir_str).ok()??;
            db.get_image(dir.id, fname).ok()?
        });
    }

    // ── Navigation ────────────────────────────────────────────────────────

    fn navigate_next(&mut self, ctx: &Context, preserve_zoom: bool) {
        let mut moved = false;
        if let Some(ref mut listing) = self.listing {
            while listing.go_next() {
                moved = true;
                if let Some(p) = listing.current() {
                    // Check ignore without using 'self' directly in the loop
                    if !self.session.ignored_images.contains(p) { break; }
                }
            }
        }
        if moved { self.load_current(ctx, preserve_zoom); }
    }

    fn navigate_prev(&mut self, ctx: &Context, preserve_zoom: bool) {
        let mut moved = false;
        if let Some(ref mut listing) = self.listing {
            while listing.go_prev() {
                moved = true;
                if let Some(p) = listing.current() {
                    if !self.session.ignored_images.contains(p) { break; }
                }
            }
        }
        if moved { self.load_current(ctx, preserve_zoom); }
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
            .and_then(|n| n.to_str()).unwrap_or("?")));
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
                if let Ok(dir) = db.upsert_directory_by_path(&dir_str) {
                    let _ = db.set_rating(dir.id, &fname, rating);
                    self.toast(match rating {
                        Some(r) => format!("Rated: {} stars", "★".repeat(r as usize)),
                        None    => "Rating cleared".to_string(),
                    });
                    self.refresh_record();
                }
            }
        }
    }

    fn rotate_current(&mut self, cw: bool, ctx: &Context) {
        let Some(path) = self.current_path.clone() else { return };
        if cw { self.session.rotate_cw(path); } else { self.session.rotate_ccw(path); }
        self.load_current(ctx, true);
    }

    // ── Delete ────────────────────────────────────────────────────────────

    fn confirm_delete(&mut self) {
        self.delete_confirm = Some(DeleteConfirm::new());
        self.toast("Press Delete again to confirm — Esc to cancel");
    }

    fn execute_delete(&mut self, ctx: &Context) {
        self.delete_confirm = None;
        let Some(path) = self.current_path.clone() else { return };
        match std::fs::remove_file(&path) {
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

                self.toast(format!("Deleted: {name}"));
                self.current_path   = None;
                self.current_record = None;
                self.metadata       = vec![];
                self.viewer.clear();
                self.load_current(ctx, false);
            }
            Err(e) => {
                self.toast(format!("Delete failed: {e}"));
            }
        }
    }

    // ── Hard refresh ─────────────────────────────────────────────────────

    fn hard_refresh(&mut self, ctx: &Context) {
        self.session.flush();
        if let Some(dir) = self.listing.as_ref().map(|l| l.dir_path.clone()) {
            let sort = self.session_sort_order();
            if let Ok(mut fresh) = DirectoryListing::scan(&dir, sort, None, self.db.as_ref()) {
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
        self.toast("Directory refreshed");
    }

    // ── Drag-out ──────────────────────────────────────────────────────────

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
                decoded.width, decoded.height, decoded.rgba.clone(),
            ) else {
                break 'img drag::Image::File(path.clone());
            };
            let thumb = image::imageops::thumbnail(&src, 128, 128);
            let mut png = Vec::new();
            if image::DynamicImage::ImageRgba8(thumb)
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

    // ── Save rotation (Ctrl+S) ────────────────────────────────────────────

    fn save_current_rotation(&mut self, ctx: &Context) {
        let Some(path) = self.current_path.clone() else { return };
        let rotation = self.session.rotation_for(&path);
        if rotation.is_identity() {
            self.toast("No rotation to save");
            return;
        }

        let Some(fmt) = SupportedFormat::from_path(&path) else {
            self.toast("Unknown format — cannot save");
            return;
        };

        let cached_clone = self.image_cache.get(&path).cloned();
        let result = match fmt {
            SupportedFormat::Jpeg => save_jpeg_exif_rotation(&path, rotation),
            SupportedFormat::Svg  => { self.toast("Cannot save rotation for SVG files"); return; }
            SupportedFormat::Raw  => { self.toast("Cannot save rotation for RAW files"); return; }
            _                     => save_pixel_rotation(&path, fmt, rotation, cached_clone),
        };

        match result {
            Ok(()) => {
                self.session.set_rotation(path.clone(), Rotation::None);
                self.image_cache.remove(&path);
                self.load_current(ctx, true);
                self.toast("Saved");
            }
            Err(e) => self.toast(format!("Save failed: {e}")),
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────

    fn session_sort_order(&self) -> crate::settings::SortOrder {
        crate::settings::SortOrder::Name
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

        if let Some(filter) = self.session.rating_filter {
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

        if input.key_pressed(Key::Escape) {
            if self.delete_confirm.is_some() {
                self.delete_confirm = None;
                self.toast("Delete cancelled");
            }
        }

        let shift = input.modifiers.shift;
        let preserve_zoom = shift;

        if input.key_pressed(Key::ArrowRight) || input.key_pressed(Key::PageDown) {
            self.navigate_next(ctx, preserve_zoom);
        }
        if input.key_pressed(Key::ArrowLeft) || input.key_pressed(Key::PageUp) {
            self.navigate_prev(ctx, preserve_zoom);
        }
        if input.key_pressed(Key::Home) { self.navigate_first(ctx, preserve_zoom); }
        if input.key_pressed(Key::End)  { self.navigate_last(ctx, preserve_zoom); }

        if input.key_pressed(Key::I) { 
            self.show_info_panel = !self.show_info_panel;
            self.settings.show_info_panel = self.show_info_panel;
            let _ = self.settings.save();
        }

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

        if input.key_pressed(Key::H) { self.hide_current(ctx); }

        if input.key_pressed(Key::OpenBracket) {
            self.rotate_current(false, ctx);
        }
        if input.key_pressed(Key::CloseBracket) {
            self.rotate_current(true, ctx);
        }

        let ctrl = input.modifiers.ctrl;
        if ctrl && input.key_pressed(Key::Num0) {
            self.viewer.zoom_actual_size();
        } else if input.key_pressed(Key::F) {
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
            self.save_current_rotation(ctx);
        }
    }

    // ── Info panel ────────────────────────────────────────────────────────

    fn draw_info_panel(&mut self, ctx: &Context) {
        egui::SidePanel::right("info_panel")
            .resizable(true)
            .min_width(280.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.heading("Image Info");
                    ui.separator();

                    if let Some(path) = self.current_path.clone() {
                        ui.label(format!("File: {}", path.file_name()
                            .and_then(|n| n.to_str()).unwrap_or("?")));
                        ui.label(format!("Path: {}", path.display()));

                        if let Ok(meta) = path.metadata() {
                            let kb = meta.len() as f64 / 1024.0;
                            if kb < 1024.0 {
                                ui.label(format!("Size: {kb:.1} KB"));
                            } else {
                                ui.label(format!("Size: {:.1} MB", kb / 1024.0));
                            }
                        }

                        let dim = self.viewer.image_size;
                        if dim != Vec2::ZERO {
                            ui.label(format!("Dimensions: {}×{}", dim.x as u32, dim.y as u32));
                        }

                        ui.label(format!("Zoom: {:.0}%", self.viewer.zoom * 100.0));

                        if let Some(ref listing) = self.listing {
                            ui.label(listing.position_label());
                        }

                        ui.separator();
                        ui.heading("Viewing Adjustment");
                        let mut g = self.viewer.gamma;
                        ui.horizontal(|ui| {
                            ui.label("Gamma:");
                            if ui.add(egui::Slider::new(&mut g, 0.1..=4.0)).changed() {
                                self.viewer.set_gamma(g, ctx);
                            }
                            if ui.button("Reset").clicked() {
                                self.viewer.set_gamma(1.0, ctx);
                            }
                        });

                        if let Some(img) = self.image_cache.get(&path) {
                            ui.separator();
                            ui.heading("Histogram (Luminance)");
                            let hist_height = 64.0;
                            let (rect, _) = ui.allocate_at_least(egui::vec2(ui.available_width(), hist_height), egui::Sense::hover());
                            let painter = ui.painter();
                            painter.rect_filled(rect, 2.0, egui::Color32::from_gray(30));
                            
                            let bin_width = rect.width() / 256.0;
                            for (i, &val) in img.histogram.iter().enumerate() {
                                let h = val * hist_height;
                                let x = rect.min.x + i as f32 * bin_width;
                                let bar_rect = egui::Rect::from_min_max(
                                    egui::pos2(x, rect.max.y - h),
                                    egui::pos2(x + bin_width, rect.max.y)
                                );
                                painter.rect_filled(bar_rect, 0.0, egui::Color32::from_gray(180));
                            }
                        }

                        ui.separator();
                        ui.heading("Rating");

                        let rating = self.current_record.as_ref()
                            .and_then(|r| r.rating);

                        let stars = match rating {
                            None    => "— (unrated)".to_string(),
                            Some(r) => format!("{} ({})", "★".repeat(r as usize), r),
                        };
                        ui.label(format!("Rating: {stars}"));

                        if let Some(ref rec) = self.current_record {
                            if let Some(ref note) = rec.note {
                                ui.label(format!("Note: {note}"));
                            }
                        }

                        if !self.metadata.is_empty() {
                            ui.separator();
                            ui.heading("Metadata");

                            for entry in &mut self.metadata {
                                let is_multiline = entry.value.contains('\n');
                                let is_long      = entry.value.len() > 120;

                                if is_multiline || is_long {
                                    egui::CollapsingHeader::new(
                                        egui::RichText::new(&entry.key).strong()
                                    )
                                    .id_source(egui::Id::new(&entry.key))
                                    .default_open(is_multiline && entry.key.to_lowercase() == "parameters")
                                    .show(ui, |ui| {
                                        ui.add(
                                            egui::TextEdit::multiline(
                                                &mut entry.value
                                            )
                                            .desired_width(f32::INFINITY)
                                            .font(egui::TextStyle::Monospace),
                                        );
                                    });
                                } else {
                                    ui.label(egui::RichText::new(&entry.key).strong());
                                    ui.label(&entry.value);
                                }
                                ui.add_space(2.0);
                            }
                        }
                    } else {
                        ui.label("No image loaded.");
                    }
                });
            });
    }

    fn apply_global_filter(&mut self, filter: RatingFilter, ctx: &Context) {
        let Some(ref db) = self.db else { return };
        self.session.rating_filter = Some(filter);
        match DirectoryListing::scan_global(db, filter) {
            Ok(listing) => {
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

    fn apply_local_filter(&mut self, filter: Option<RatingFilter>, ctx: &Context) {
        self.session.rating_filter = filter;
        if let Some(ref mut listing) = self.listing {
            listing.rating_filter = filter;
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
                    for r in 1..=5 {
                        let filter = RatingFilter {
                            op:    RatingFilterOp::AtLeast,
                            value: r,
                        };
                        if ui.button(format!("At least ★ {r}")).clicked() {
                            self.apply_local_filter(Some(filter), ctx);
                            ui.close_menu();
                        }
                    }
                });

                let has_db = self.db.is_some();
                ui.add_enabled_ui(has_db, |ui| {
                    ui.menu_button("Library", |ui| {
                        for r in 1..=5 {
                            let filter = RatingFilter {
                                op:    RatingFilterOp::AtLeast,
                                value: r,
                            };
                            if ui.button(format!("At least ★ {r}")).clicked() {
                                self.apply_global_filter(filter, ctx);
                                ui.close_menu();
                            }
                        }
                    });
                });

                if ui.button("Clear Filter").clicked() {
                    self.apply_local_filter(None, ctx);
                    ui.close_menu();
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
            if ui.add_enabled(has_image, egui::Button::new("Open folder")).clicked() {
                self.reveal_in_file_manager();
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
    let src = image::RgbaImage::from_raw(decoded.width, decoded.height, decoded.rgba.clone())
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
        self.image_cache.poll();
        ctx.request_repaint();

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

        CentralPanel::default().show(ctx, |ui| {
            let canvas = ui.max_rect();
            self.viewer.recalc_fit(ui.available_size());

            let response = ui.allocate_rect(canvas, egui::Sense::click_and_drag());

            let ctrl_held = ctx.input(|i| i.modifiers.ctrl);

            // Middle-mouse drag detection.
            // egui's Sense::click_and_drag() only tracks the primary button, so
            // drag_started_by(Middle) never fires. Track the middle button manually.
            let (middle_pressed, middle_down, pointer_moving) = ctx.input(|i| (
                i.pointer.button_pressed(egui::PointerButton::Middle),
                i.pointer.button_down(egui::PointerButton::Middle),
                i.pointer.is_moving(),
            ));
            if middle_pressed && response.hovered() {
                self.middle_btn_on_canvas = true;
            }
            if !middle_down {
                self.middle_btn_on_canvas = false;
            }
            let middle_drag_started = self.middle_btn_on_canvas && middle_down && pointer_moving;

            // Detect drag-out gesture; schedule for execution at the top of the next frame.
            let drag_out_trigger =
                (response.drag_started_by(egui::PointerButton::Primary) && ctrl_held)
                || middle_drag_started;
            if drag_out_trigger && !self.pending_drag_out && self.current_path.is_some() {
                self.pending_drag_out = true;
                self.middle_btn_on_canvas = false; // consume so it doesn't re-fire
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
                    self.toast("Error message copied to clipboard");
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

            if response.clicked() && self.viewer.load_error.is_some() {
                if let Some(ref err) = self.viewer.load_error {
                    ctx.copy_text(err.clone());
                    self.toast("Error message copied to clipboard");
                }
            }

            self.draw_context_menu(&response, ctx);

            let painter = ui.painter();
            if let Some(ref texture) = self.viewer.texture {
                let rect = self.viewer.image_rect(canvas);
                painter.image(
                    texture.id(), rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            } else if let Some(ref err) = self.viewer.load_error {
                painter.text(
                    canvas.center(), egui::Align2::CENTER_CENTER,
                    format!("Error loading image:\n{err}"),
                    egui::FontId::proportional(18.0),
                    egui::Color32::LIGHT_RED,
                );
            } else {
                painter.text(
                    canvas.center(), egui::Align2::CENTER_CENTER,
                    "Drag an image here, or double-click to open",
                    egui::FontId::proportional(18.0),
                    egui::Color32::from_gray(130),
                );
            }

            let current_has_changes = self.current_path.as_ref().map(|p| {
                self.session.pending_rotations.contains_key(p)
                    || self.session.pending_crops.contains_key(p)
            }).unwrap_or(false);
            if current_has_changes {
                let dot_pos = egui::pos2(canvas.max.x - 14.0, canvas.min.y + 14.0);
                let response = ui.interact(
                    egui::Rect::from_center_size(dot_pos, egui::vec2(12.0, 12.0)),
                    egui::Id::new("modified_badge"),
                    egui::Sense::hover(),
                );
                response.on_hover_text("Unsaved changes (rotation, crops) — Ctrl+S to save");
                painter.circle_filled(dot_pos, 6.0, egui::Color32::from_rgb(255, 180, 0));
            }

            if self.delete_confirm.as_ref().map(|d| d.alive()).unwrap_or(false) {
                let bg = egui::Color32::from_rgba_unmultiplied(180, 30, 30, 210);
                let msg_rect = egui::Rect::from_center_size(
                    canvas.center(),
                    egui::vec2(420.0, 56.0),
                );
                painter.rect_filled(msg_rect, 6.0, bg);
                painter.text(
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
                painter.rect_filled(rect, 6.0, egui::Color32::from_rgba_unmultiplied(30, 30, 30, a));
                painter.galley(rect.min + pad, galley, egui::Color32::from_rgba_unmultiplied(255, 255, 255, (alpha * 255.0) as u8));
                ctx.request_repaint();
            }
        }

        if self.toast.as_ref().map(|t| !t.alive()).unwrap_or(false) {
            self.toast = None;
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct Toast {
    message: String,
    start:   Instant,
}

impl Toast {
    fn new(message: String) -> Self {
        Self { message, start: Instant::now() }
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
struct OwnedWindowHandle(isize);

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
