//! Utility windows — accessible from the right-click context menu under "Utilities".
//! No hotkeys; menu-only.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use egui::Context;
use crate::db::Database;
use crate::formats::SupportedFormat;

// ---------------------------------------------------------------------------
// Top-level container
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct UtilitiesState {
    purge:             Option<PurgeState>,
    db_health:         Option<DbHealthState>,
    tag_editor:        Option<TagEditorState>,
    consolidate:       Option<ConsolidateState>,
}

#[derive(Default)]
struct TagEditorState {
    tags:              Vec<crate::db::TagRecord>,
    /// The ID of the tag currently being renamed.
    editing_tag_id:    Option<i64>,
    /// The current text in the rename field.
    rename_buffer:     String,
    /// The tag name being considered for deletion.
    confirm_delete:    Option<String>,
    /// Number of images affected by the pending deletion.
    affected_count:    usize,
}


impl UtilitiesState {
    pub fn open_purge(&mut self, base_path: PathBuf) {
        self.purge = Some(PurgeState::new(base_path));
    }

    pub fn open_db_health(&mut self, db: Option<&Database>) {
        let total = db.and_then(|d| d.count_all_entries().ok()).unwrap_or(0);
        self.db_health = Some(DbHealthState::new(total));
    }

    pub fn open_tag_editor(&mut self, db: Option<&Database>) {
        if let Some(db) = db {
            if let Ok(tags) = db.get_all_tags() {
                self.tag_editor = Some(TagEditorState {
                    tags,
                    ..Default::default()
                });
            }
        }
    }

    pub fn open_consolidate(&mut self, files: Vec<PathBuf>) {
        self.consolidate = Some(ConsolidateState::new(files));
    }

    pub fn draw(&mut self, ctx: &Context, db: Option<&Database>) {
        if let Some(ref mut state) = self.purge {
            if !state.draw(ctx, db) { self.purge = None; }
        }
        if let Some(ref mut state) = self.db_health {
            if !state.draw(ctx, db) { self.db_health = None; }
        }
        self.draw_tag_editor(ctx, db);
        if let Some(ref mut state) = self.consolidate {
            if !state.draw(ctx, db) { self.consolidate = None; }
        }
    }

    fn draw_tag_editor(&mut self, ctx: &Context, db: Option<&Database>) {
        let Some(mut state) = self.tag_editor.take() else { return };
        let mut open = true;

        egui::Window::new("Tag Manager")
            .open(&mut open)
            .resizable(true)
            .default_width(350.0)
            .show(ctx, |ui| {
                if let Some(db) = db {
                    ui.label("Double-click tag name to rename. Click swatch for color.");
                    ui.separator();

                    egui::ScrollArea::vertical().max_height(350.0).show(ui, |ui| {
                        let mut tag_to_delete = None;
                        let mut refresh_needed = false;

                        egui::Grid::new("tag_manager_grid")
                            .num_columns(3)
                            .spacing([8.0, 4.0])
                            .show(ui, |ui| {
                                for tag in &state.tags {
                                    // 1. Color Swatch
                                    let mut color = egui::Color32::from_rgb(
                                        ((tag.color >> 16) & 0xFF) as u8,
                                        ((tag.color >> 8) & 0xFF) as u8,
                                        (tag.color & 0xFF) as u8,
                                    );
                                    if egui::color_picker::color_edit_button_srgba(ui, &mut color, egui::color_picker::Alpha::Opaque).changed() {
                                        let new_color = ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | (color.b() as u32);
                                        let _ = db.update_tag_color(tag.id, new_color);
                                        refresh_needed = true;
                                    }

                                    // 2. Name / Edit field
                                    if state.editing_tag_id == Some(tag.id) {
                                        let res = ui.text_edit_singleline(&mut state.rename_buffer);
                                        if res.lost_focus() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                            if !state.rename_buffer.trim().is_empty() && state.rename_buffer != tag.name {
                                                let _ = db.rename_tag(&tag.name, &state.rename_buffer);
                                                refresh_needed = true;
                                            }
                                            state.editing_tag_id = None;
                                        } else if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                            state.editing_tag_id = None;
                                        }
                                        res.request_focus();
                                    } else {
                                        let label = ui.selectable_label(false, &tag.name);
                                        if label.double_clicked() {
                                            state.editing_tag_id = Some(tag.id);
                                            state.rename_buffer = tag.name.clone();
                                        }
                                    }

                                    // 3. Delete button
                                    if ui.button(" ✖ ").on_hover_text("Delete tag").clicked() {
                                        tag_to_delete = Some(tag.name.clone());
                                    }
                                    ui.end_row();
                                }
                            });

                        if let Some(tag_name) = tag_to_delete {
                            state.confirm_delete = Some(tag_name.clone());
                            state.affected_count = db.count_images_with_tag(&tag_name).unwrap_or(0);
                        }
                        if refresh_needed {
                            if let Ok(t) = db.get_all_tags() { state.tags = t; }
                        }
                    });

                    if let Some(tag) = state.confirm_delete.clone() {
                        egui::Window::new("Confirm Tag Deletion")
                            .collapsible(false)
                            .resizable(false)
                            .pivot(egui::Align2::CENTER_CENTER)
                            .show(ctx, |ui| {
                                ui.label(egui::RichText::new(format!(
                                    "Delete tag '{}'?", tag
                                )).strong());
                                
                                ui.add_space(4.0);
                                ui.label(format!(
                                    "This will remove the tag from {} image{}.",
                                    state.affected_count,
                                    if state.affected_count == 1 { "" } else { "s" }
                                ));
                                
                                ui.add_space(10.0);
                                ui.horizontal(|ui| {
                                    let abort_btn = ui.button("Abort");
                                    let delete_btn = ui.button(egui::RichText::new("Delete").color(egui::Color32::from_rgb(220, 80, 80)));
                                    
                                    let mut should_abort = abort_btn.clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape));
                                    let mut should_delete = delete_btn.clicked();

                                    // Enter handling:
                                    if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                        if delete_btn.has_focus() {
                                            should_delete = true;
                                        } else {
                                            should_abort = true;
                                        }
                                    }

                                    if should_abort {
                                        state.confirm_delete = None;
                                    } else if should_delete {
                                        let _ = db.delete_tag(&tag);
                                        state.confirm_delete = None;
                                        if let Ok(t) = db.get_all_tags() { state.tags = t; }
                                    }
                                });
                            });
                    }
                } else {
                    ui.label("Database not available.");
                }
            });

        if open {
            self.tag_editor = Some(state);
        }
    }
}

// ---------------------------------------------------------------------------
// Path summarisation
// ---------------------------------------------------------------------------

/// One entry in the condensed path summary shown in the UI.
pub struct PathEntry {
    pub path:     String,
    pub counts:   [usize; 6], // [unrated, ★1 … ★5]
    pub excluded: bool,       // true = user has unchecked this path
}

const SUMMARY_TARGET: usize = 20;

/// Build a compact list of path prefixes with per-bucket counts.
///
/// Works by iteratively collapsing the deepest directory component until the
/// number of distinct prefixes is within `SUMMARY_TARGET`.  This is the
/// path-trie chain-compression idea expressed as simple iteration so that it
/// compiles without lifetime gymnastics.
pub fn summarise_paths(files: &[(PathBuf, usize)]) -> Vec<PathEntry> {
    if files.is_empty() { return vec![]; }

    for trim in 0usize.. {
        let mut map: HashMap<String, [usize; 6]> = HashMap::new();
        for (path, bucket) in files {
            let dir = path.parent().unwrap_or(path);
            let key = trim_tail(dir, trim);
            map.entry(key).or_insert([0usize; 6])[*bucket] += 1;
        }
        if map.len() <= SUMMARY_TARGET || trim > 30 {
            let mut out: Vec<PathEntry> = map
                .into_iter()
                .map(|(path, counts)| PathEntry { path, counts, excluded: false })
                .collect();
            out.sort_by(|a, b| a.path.cmp(&b.path));
            return out;
        }
    }
    vec![]
}

/// Return `path` with the last `n` components removed, appending `/…` if any
/// components were dropped.
fn trim_tail(path: &Path, n: usize) -> String {
    let comps: Vec<_> = path.components().collect();
    let keep = comps.len().saturating_sub(n);
    if keep == 0 {
        return comps
            .first()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    let mut p = PathBuf::new();
    for c in comps.iter().take(keep) { p.push(c); }
    if keep < comps.len() {
        format!("{}/…", p.display())
    } else {
        p.to_string_lossy().into_owned()
    }
}

/// Returns `true` if `file_path` falls under any excluded `PathEntry`.
///
/// Uses component-level `Path::starts_with` so that a prefix of
/// `C:\Photos\2024` does not accidentally match `C:\Photos\2024extra`.
/// Trie-collapsed entries (ending in `/…`) have the suffix stripped before
/// the comparison.
fn path_is_excluded(file_path: &Path, summary: &[PathEntry]) -> bool {
    let dir = match file_path.parent() {
        Some(p) => p,
        None    => return false,
    };
    summary.iter().filter(|e| e.excluded).any(|e| {
        let prefix = e.path.trim_end_matches("/…");
        dir.starts_with(Path::new(prefix))
    })
}

// ---------------------------------------------------------------------------
// Utility 1 — File System Purge
// ---------------------------------------------------------------------------

pub struct PurgeState {
    base_path: PathBuf,
    recursive: bool,
    /// Files indexed by bucket: 0 = unrated, 1–5 = star rating.
    buckets:   [Vec<PathBuf>; 6],
    scanned:   bool,
    summary:   Vec<PathEntry>,
    confirm:   Option<PurgeConfirm>,
    result:    Option<String>,
}

struct PurgeConfirm {
    bucket: usize,
    files:  Vec<PathBuf>,
}

impl PurgeState {
    pub fn new(base_path: PathBuf) -> Self {
        Self {
            base_path,
            recursive: false,
            buckets:   Default::default(),
            scanned:   false,
            summary:   vec![],
            confirm:   None,
            result:    None,
        }
    }

    fn scan(&mut self, db: Option<&Database>) {
        // 1. Walk the file system.
        let files = collect_images(&self.base_path, self.recursive);

        // 2. Build a (dir_path, filename) → bucket lookup from the DB.
        let mut rating_map: HashMap<(String, String), usize> = HashMap::new();
        if let Some(db) = db {
            if let Ok(entries) = db.get_all_image_paths() {
                for (_, dir_path, filename, rating) in entries {
                    let bucket = rating.map(|r| r as usize).unwrap_or(0);
                    rating_map.insert((dir_path, filename), bucket);
                }
            }
        }

        // 3. Bucket every file.
        for b in &mut self.buckets { b.clear(); }
        let mut flat: Vec<(PathBuf, usize)> = Vec::new();
        for file in files {
            let dir = file.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            let fname = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let bucket = rating_map.get(&(dir, fname)).copied().unwrap_or(0);
            self.buckets[bucket].push(file.clone());
            flat.push((file, bucket));
        }

        self.summary = summarise_paths(&flat);
        self.scanned = true;
    }

    /// Count of files in `bucket` that are not under any excluded trie path.
    fn filtered_count(&self, bucket: usize) -> usize {
        self.buckets[bucket].iter()
            .filter(|f| !path_is_excluded(f, &self.summary))
            .count()
    }

    /// Non-excluded files in `bucket` — used to populate `PurgeConfirm`.
    fn files_for_deletion(&self, bucket: usize) -> Vec<PathBuf> {
        self.buckets[bucket].iter()
            .filter(|f| !path_is_excluded(f, &self.summary))
            .cloned()
            .collect()
    }

    /// Draw the window; returns `false` when the window is closed.
    pub fn draw(&mut self, ctx: &Context, db: Option<&Database>) -> bool {
        let mut open = true;

        egui::Window::new("File System Purge")
            .collapsible(false)
            .resizable(true)
            .min_width(500.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.add(egui::Label::new(
                    egui::RichText::new(self.base_path.to_string_lossy().as_ref()).monospace().small()
                ).truncate());
                ui.add_space(6.0);

                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.recursive, false, "Current directory only");
                    ui.radio_value(&mut self.recursive, true,  "Recursive (include subdirectories)");
                });
                ui.add_space(6.0);

                if ui.button("Scan").clicked() {
                    self.scan(db);
                    self.result = None;
                }

                if self.scanned {
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(4.0);

                    // Snapshot filtered counts before any mutable borrow of self.summary.
                    let filtered: [usize; 6] = std::array::from_fn(|i| self.filtered_count(i));

                    // Directory breakdown with exclusion checkboxes — shown first so the
                    // user can adjust exclusions before reading the rating counts below.
                    if !self.summary.is_empty() {
                        ui.strong("Directory breakdown");
                        ui.label(egui::RichText::new(
                            "Uncheck paths to exclude them from the deletion pool."
                        ).small().weak());
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical()
                            .id_source("purge_summary_scroll")
                            .max_height(140.0)
                            .show(ui, |ui| {
                                egui::Grid::new("purge_dir_summary")
                                    .num_columns(8)
                                    .spacing([8.0, 2.0])
                                    .show(ui, |ui| {
                                        ui.strong("");
                                        ui.strong("Path");
                                        for h in &["∅", "★1", "★2", "★3", "★4", "★5"] {
                                            ui.strong(*h);
                                        }
                                        ui.end_row();
                                        for entry in &mut self.summary {
                                            let mut included = !entry.excluded;
                                            ui.checkbox(&mut included, "");
                                            entry.excluded = !included;
                                            ui.add(egui::Label::new(
                                                egui::RichText::new(&entry.path).monospace().small()
                                            ).truncate());
                                            for c in &entry.counts {
                                                if *c > 0 {
                                                    ui.label(c.to_string());
                                                } else {
                                                    ui.label(egui::RichText::new("—").weak());
                                                }
                                            }
                                            ui.end_row();
                                        }
                                    });
                            });
                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(4.0);
                    }

                    // Rating counts table — counts reflect only non-excluded paths.
                    egui::Grid::new("purge_rating_table")
                        .num_columns(3)
                        .striped(true)
                        .spacing([16.0, 4.0])
                        .show(ui, |ui| {
                            ui.strong("Rating");
                            ui.strong("Files");
                            ui.strong("");
                            ui.end_row();

                            let labels = ["Unrated", "★ 1", "★ 2", "★ 3", "★ 4", "★ 5"];
                            for (i, label) in labels.iter().enumerate() {
                                let count = filtered[i];
                                ui.label(*label);
                                ui.label(count.to_string());
                                if count > 0 {
                                    let btn = egui::Button::new(
                                        egui::RichText::new("Delete all…")
                                            .color(egui::Color32::from_rgb(220, 80, 80))
                                    );
                                    if ui.add(btn).on_hover_text(
                                        format!("Permanently delete {} {} file{}", count, label, if count == 1 { "" } else { "s" })
                                    ).clicked() {
                                        self.confirm = Some(PurgeConfirm {
                                            bucket: i,
                                            files:  self.files_for_deletion(i),
                                        });
                                    }
                                } else {
                                    ui.label(egui::RichText::new("—").weak());
                                }
                                ui.end_row();
                            }
                        });
                }

                if let Some(ref msg) = self.result {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(msg).small());
                }
            });

        self.draw_confirm(ctx);
        open
    }

    fn draw_confirm(&mut self, ctx: &Context) {
        let Some(ref confirm) = self.confirm else { return };
        let label     = ["Unrated", "★ 1", "★ 2", "★ 3", "★ 4", "★ 5"][confirm.bucket];
        let count     = confirm.files.len();
        let scope_str = if self.recursive { "Recursive" } else { "Current directory only" };

        let mut do_delete = false;
        let mut cancel    = false;

        egui::Window::new("Confirm Deletion")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(egui::RichText::new(format!(
                    "Permanently delete {} file{}?",
                    count, if count == 1 { "" } else { "s" }
                )).strong());
                ui.add_space(8.0);

                egui::Grid::new("purge_confirm_details")
                    .num_columns(2)
                    .spacing([12.0, 4.0])
                    .show(ui, |ui| {
                        ui.strong("Rating:");    ui.label(label);             ui.end_row();
                        ui.strong("Files:");     ui.label(count.to_string()); ui.end_row();
                        ui.strong("Base path:"); ui.add(egui::Label::new(
                            egui::RichText::new(self.base_path.to_string_lossy().as_ref()).monospace().small()
                        ).truncate()); ui.end_row();
                        ui.strong("Scope:");     ui.label(scope_str);         ui.end_row();
                    });

                if self.recursive {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(
                        "⚠ Recursive mode — files in all subdirectories will be deleted."
                    ).color(egui::Color32::from_rgb(255, 180, 0)).small());
                }

                ui.add_space(10.0);
                ui.label(egui::RichText::new(
                    "This is permanent and cannot be undone."
                ).color(egui::Color32::from_rgb(200, 80, 80)).small());
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    let del_btn = egui::Button::new(
                        egui::RichText::new("Delete permanently")
                            .color(egui::Color32::from_rgb(220, 60, 60))
                    );
                    if ui.add(del_btn).clicked() { do_delete = true; }
                    if ui.button("Cancel").clicked() { cancel = true; }
                });
            });

        if cancel   { self.confirm = None; }
        if do_delete {
            if let Some(confirm) = self.confirm.take() {
                self.execute_deletion(confirm);
            }
        }
    }

    fn execute_deletion(&mut self, confirm: PurgeConfirm) {
        let mut deleted_set: HashSet<PathBuf> = HashSet::new();
        let mut failed = 0usize;
        for path in &confirm.files {
            match std::fs::remove_file(path) {
                Ok(())  => { deleted_set.insert(path.clone()); }
                Err(_)  => { failed += 1; }
            }
        }
        let deleted = deleted_set.len();

        // Remove only the successfully deleted files; excluded files in this
        // bucket remain untouched.
        self.buckets[confirm.bucket].retain(|f| !deleted_set.contains(f));

        // Rebuild summary from remaining files and re-apply existing excluded flags.
        let flat: Vec<(PathBuf, usize)> = self.buckets.iter().enumerate()
            .flat_map(|(i, files)| files.iter().map(move |f| (f.clone(), i)))
            .collect();
        let old_excluded: HashMap<String, bool> = self.summary.iter()
            .map(|e| (e.path.clone(), e.excluded))
            .collect();
        self.summary = summarise_paths(&flat).into_iter().map(|mut e| {
            if let Some(&ex) = old_excluded.get(&e.path) { e.excluded = ex; }
            e
        }).collect();

        self.result = Some(format!(
            "Deleted {deleted} file{}{}.",
            if deleted == 1 { "" } else { "s" },
            if failed > 0 { format!(", {failed} failed") } else { String::new() },
        ));
    }
}

// ---------------------------------------------------------------------------
// Utility 2 — Database Health Check
// ---------------------------------------------------------------------------

pub struct DbHealthState {
    total_entries:  usize,
    scanned:        bool,
    /// (dir_id, dir_path, filename) for each orphaned entry.
    orphans:        Vec<(i64, String, String)>,
    /// Trie summary of orphan paths; users uncheck entries to exclude them.
    summary:        Vec<PathEntry>,
    confirm_prune:  bool,
    result:         Option<String>,
}

impl DbHealthState {
    pub fn new(total: usize) -> Self {
        Self {
            total_entries: total,
            scanned:       false,
            orphans:       vec![],
            summary:       vec![],
            confirm_prune: false,
            result:        None,
        }
    }

    fn scan(&mut self, db: &Database) {
        let Ok(entries) = db.get_all_image_paths() else { return };
        self.orphans.clear();

        for (dir_id, dir_path, filename, _) in entries {
            let path = PathBuf::from(&dir_path).join(&filename);
            if !path.exists() {
                self.orphans.push((dir_id, dir_path, filename));
            }
        }

        // Build trie from orphan directory paths.  Join a dummy filename so
        // that summarise_paths's parent() call recovers the directory exactly.
        let flat: Vec<(PathBuf, usize)> = self.orphans.iter()
            .map(|(_, dir, _)| (PathBuf::from(dir).join("_"), 0usize))
            .collect();
        self.summary = summarise_paths(&flat);
        self.scanned = true;
    }

    /// Number of orphans not under any excluded trie path.
    fn candidate_count(&self) -> usize {
        self.orphans.iter().filter(|(_, dir, fname)| {
            !path_is_excluded(&PathBuf::from(dir).join(fname), &self.summary)
        }).count()
    }

    fn prune(&mut self, db: &Database) {
        let mut pruned = 0usize;
        let mut failed = 0usize;
        let mut keep_orphans: Vec<(i64, String, String)> = Vec::new();

        for (dir_id, dir_path, filename) in &self.orphans {
            let file = PathBuf::from(dir_path).join(filename);
            if path_is_excluded(&file, &self.summary) {
                keep_orphans.push((*dir_id, dir_path.clone(), filename.clone()));
            } else {
                match db.delete_image(*dir_id, filename) {
                    Ok(()) => pruned += 1,
                    Err(_) => {
                        failed += 1;
                        keep_orphans.push((*dir_id, dir_path.clone(), filename.clone()));
                    }
                }
            }
        }

        self.orphans       = keep_orphans;
        self.total_entries = self.total_entries.saturating_sub(pruned);

        // Rebuild trie from remaining orphans, preserving excluded flags.
        let old_excluded: HashMap<String, bool> = self.summary.iter()
            .map(|e| (e.path.clone(), e.excluded))
            .collect();
        let flat: Vec<(PathBuf, usize)> = self.orphans.iter()
            .map(|(_, dir, _)| (PathBuf::from(dir).join("_"), 0usize))
            .collect();
        self.summary = summarise_paths(&flat).into_iter().map(|mut e| {
            if let Some(&ex) = old_excluded.get(&e.path) { e.excluded = ex; }
            e
        }).collect();

        self.result = Some(format!(
            "Removed {pruned} entr{}{}.",
            if pruned == 1 { "y" } else { "ies" },
            if failed > 0 { format!(", {failed} failed") } else { String::new() },
        ));
    }

    /// Draw the window; returns `false` when the window is closed.
    pub fn draw(&mut self, ctx: &Context, db: Option<&Database>) -> bool {
        let mut open = true;

        egui::Window::new("Database Health Check")
            .collapsible(false)
            .resizable(true)
            .min_width(540.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!("Total database entries: {}", self.total_entries));
                ui.add_space(6.0);

                if ui.button("Scan for orphans").clicked() {
                    if let Some(db) = db {
                        self.scan(db);
                        self.result = None;
                    }
                }

                if self.scanned {
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(4.0);

                    if self.orphans.is_empty() {
                        ui.label(egui::RichText::new(
                            "✓ No orphaned entries found."
                        ).color(egui::Color32::from_rgb(100, 210, 100)));
                    } else {
                        ui.label(egui::RichText::new(format!(
                            "{} orphaned entr{} found:",
                            self.orphans.len(),
                            if self.orphans.len() == 1 { "y" } else { "ies" }
                        )).strong());
                        ui.add_space(4.0);

                        ui.label(egui::RichText::new(
                            "Uncheck paths to exclude them from pruning (e.g. disconnected network drives)."
                        ).small().weak());
                        ui.add_space(4.0);

                        egui::ScrollArea::vertical()
                            .id_source("orphan_trie_scroll")
                            .max_height(200.0)
                            .show(ui, |ui| {
                                egui::Grid::new("orphan_trie_table")
                                    .num_columns(3)
                                    .spacing([8.0, 2.0])
                                    .show(ui, |ui| {
                                        ui.strong("");
                                        ui.strong("Path");
                                        ui.strong("Orphans");
                                        ui.end_row();
                                        for entry in &mut self.summary {
                                            let mut included = !entry.excluded;
                                            ui.checkbox(&mut included, "");
                                            entry.excluded = !included;
                                            ui.add(egui::Label::new(
                                                egui::RichText::new(&entry.path).monospace().small()
                                            ).truncate());
                                            let count = entry.counts[0];
                                            if count > 0 {
                                                ui.label(count.to_string());
                                            } else {
                                                ui.label(egui::RichText::new("—").weak());
                                            }
                                            ui.end_row();
                                        }
                                    });
                            });

                        ui.add_space(6.0);
                        let candidates = self.candidate_count();
                        ui.label(format!(
                            "{} candidate{} to remove",
                            candidates,
                            if candidates == 1 { "" } else { "s" }
                        ));
                        ui.add_space(4.0);

                        let prune_btn = egui::Button::new(
                            egui::RichText::new(format!(
                                "Prune {} candidate{}…",
                                candidates,
                                if candidates == 1 { "" } else { "s" }
                            )).color(egui::Color32::from_rgb(220, 80, 80))
                        );
                        ui.add_enabled_ui(candidates > 0, |ui| {
                            if ui.add(prune_btn).clicked() { self.confirm_prune = true; }
                        });
                    }
                }

                if let Some(ref msg) = self.result {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(msg).small());
                }
            });

        // Confirmation dialog
        if self.confirm_prune {
            let candidates = self.candidate_count();
            let mut do_prune = false;
            let mut cancel   = false;

            egui::Window::new("Confirm Pruning")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(egui::RichText::new(format!(
                        "Remove {} orphaned database entr{}?",
                        candidates,
                        if candidates == 1 { "y" } else { "ies" }
                    )).strong());
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(
                        "Only database records are affected — no files will be deleted."
                    ).small().weak());
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Remove entries").clicked() { do_prune = true; }
                        if ui.button("Cancel").clicked()         { cancel   = true; }
                    });
                });

            if cancel    { self.confirm_prune = false; }
            if do_prune  {
                self.confirm_prune = false;
                if let Some(db) = db { self.prune(db); }
            }
        }

        open
    }
}

// ---------------------------------------------------------------------------
// Utility 4 — Consolidate Filtered Images
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConsolidateOp { Copy, Move }

struct ConsolidateResults {
    succeeded: usize,
    skipped:   Vec<String>,
    failed:    Vec<String>,
}

struct ConsolidateState {
    files:         Vec<PathBuf>,
    destination:   String,
    operation:     ConsolidateOp,
    copy_rating:     bool,
    copy_tags:       bool,
    copy_notes:      bool,
    preserve_exif:   bool,
    move_update_db:  bool,
    confirmed:     bool,
    results:       Option<ConsolidateResults>,
}

impl ConsolidateState {
    fn new(files: Vec<PathBuf>) -> Self {
        Self {
            files,
            destination:   String::new(),
            operation:     ConsolidateOp::Copy,
            copy_rating:    true,
            copy_tags:      true,
            copy_notes:     true,
            preserve_exif:  true,
            move_update_db: true,
            confirmed:     false,
            results:       None,
        }
    }

    fn execute(&mut self, db: Option<&Database>) {
        let dest = PathBuf::from(&self.destination);
        let mut succeeded = 0usize;
        let mut skipped   = Vec::new();
        let mut failed    = Vec::new();

        let dest_str    = dest.to_string_lossy().to_string();
        let dest_dir_id = db.and_then(|d| d.upsert_directory_by_path(&dest_str).ok().map(|r| r.id));

        for path in &self.files {
            let fname = match path.file_name() {
                Some(n) => n.to_string_lossy().to_string(),
                None    => { failed.push(path.to_string_lossy().to_string()); continue; }
            };
            let dest_path = dest.join(&fname);

            if dest_path.exists() {
                skipped.push(fname);
                continue;
            }

            let file_ok: std::io::Result<()> = match self.operation {
                ConsolidateOp::Move => match std::fs::rename(path, &dest_path) {
                    Ok(()) => Ok(()),
                    Err(_) => std::fs::copy(path, &dest_path)
                        .map(|_| ())
                        .and_then(|()| std::fs::remove_file(path)),
                },
                ConsolidateOp::Copy => std::fs::copy(path, &dest_path).map(|_| ()),
            };

            if let Err(e) = file_ok {
                log::error!("consolidate: {} → {}: {}", fname, dest_path.display(), e);
                failed.push(fname);
                continue;
            }

            if !self.preserve_exif {
                if let Err(e) = strip_jpeg_exif(&dest_path) {
                    log::warn!("consolidate: strip_exif {}: {}", fname, e);
                }
            }

            if let (Some(db), Some(dest_id)) = (db, dest_dir_id) {
                if let Some(src_dir) = path.parent() {
                    let src_str = src_dir.to_string_lossy().to_string();
                    if let Ok(Some(src_rec)) = db.find_directory_by_path(&src_str) {
                        match self.operation {
                            ConsolidateOp::Move if self.move_update_db => {
                                let _ = db.move_image_record(src_rec.id, &fname, dest_id);
                            }
                            ConsolidateOp::Copy if self.copy_rating || self.copy_tags || self.copy_notes => {
                                let _ = db.copy_image_record(
                                    src_rec.id, &fname, dest_id,
                                    self.copy_rating, self.copy_tags, self.copy_notes,
                                );
                            }
                            _ => {}
                        }
                    }
                }
            }

            succeeded += 1;
        }

        self.results = Some(ConsolidateResults { succeeded, skipped, failed });
    }

    fn draw(&mut self, ctx: &Context, db: Option<&Database>) -> bool {
        let mut open = true;
        let n = self.files.len();

        egui::Window::new(format!("Consolidate {} Image{}", n, if n == 1 { "" } else { "s" }))
            .collapsible(false)
            .resizable(true)
            .min_width(420.0)
            .open(&mut open)
            .show(ctx, |ui| {
                if let Some(ref results) = self.results {
                    let op_label = match self.operation {
                        ConsolidateOp::Copy => "copied",
                        ConsolidateOp::Move => "moved",
                    };
                    ui.label(egui::RichText::new(format!(
                        "✓ {} file{} {}.",
                        results.succeeded,
                        if results.succeeded == 1 { "" } else { "s" },
                        op_label,
                    )).strong());

                    if !results.skipped.is_empty() {
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new(format!(
                            "Skipped — already exist at destination ({}):", results.skipped.len()
                        )).weak());
                        egui::ScrollArea::vertical()
                            .id_source("consolidate_skipped")
                            .max_height(120.0)
                            .show(ui, |ui| {
                                for name in &results.skipped {
                                    ui.add(egui::Label::new(
                                        egui::RichText::new(name).monospace().small().weak()
                                    ).truncate());
                                }
                            });
                    }

                    if !results.failed.is_empty() {
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new(format!(
                            "Failed ({}):", results.failed.len()
                        )).color(egui::Color32::from_rgb(220, 80, 80)));
                        egui::ScrollArea::vertical()
                            .id_source("consolidate_failed")
                            .max_height(120.0)
                            .show(ui, |ui| {
                                for name in &results.failed {
                                    ui.add(egui::Label::new(
                                        egui::RichText::new(name).monospace().small()
                                    ).truncate());
                                }
                            });
                    }
                    return;
                }

                // Destination
                ui.horizontal(|ui| {
                    ui.label("Destination:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.destination)
                            .desired_width(220.0)
                            .hint_text("Select a folder…")
                    );
                    if ui.button("Browse…").clicked() {
                        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                            self.destination = folder.to_string_lossy().to_string();
                        }
                    }
                });

                ui.add_space(6.0);

                // Operation
                ui.horizontal(|ui| {
                    ui.label("Operation:");
                    ui.radio_value(&mut self.operation, ConsolidateOp::Copy, "Copy");
                    ui.radio_value(&mut self.operation, ConsolidateOp::Move, "Move");
                });

                ui.add_space(4.0);

                match self.operation {
                    ConsolidateOp::Copy => {
                        ui.label("Copy database metadata:");
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.copy_rating, "Ratings");
                            ui.checkbox(&mut self.copy_tags,   "Tags");
                            ui.checkbox(&mut self.copy_notes,  "Notes");
                        });
                        ui.add_space(4.0);
                    }
                    ConsolidateOp::Move => {
                        ui.checkbox(&mut self.move_update_db, "Update database record")
                            .on_hover_text("Moves the rating, tags, and notes to the destination.\nUncheck to leave the source record in place; orphans can be pruned later via Database Health Check.");
                        ui.add_space(4.0);
                    }
                }

                ui.checkbox(&mut self.preserve_exif, "Preserve embedded EXIF metadata");

                ui.add_space(8.0);

                if self.confirmed {
                    ui.separator();
                    ui.add_space(4.0);
                    let op_label = match self.operation {
                        ConsolidateOp::Copy => "Copy",
                        ConsolidateOp::Move => "Move",
                    };
                    ui.label(egui::RichText::new(format!(
                        "{} {} file{} to:", op_label, n, if n == 1 { "" } else { "s" }
                    )).strong());
                    ui.add(egui::Label::new(
                        egui::RichText::new(&self.destination).monospace().small()
                    ).truncate());
                    ui.add_space(6.0);

                    if self.operation == ConsolidateOp::Move {
                        ui.label(egui::RichText::new(
                            "⚠ Files will be removed from their current location."
                        ).color(egui::Color32::from_rgb(255, 180, 0)).small());
                        ui.add_space(4.0);
                    }

                    ui.horizontal(|ui| {
                        let color = match self.operation {
                            ConsolidateOp::Move => egui::Color32::from_rgb(220, 160, 60),
                            ConsolidateOp::Copy => egui::Color32::from_rgb(80, 200, 120),
                        };
                        let confirm_btn = egui::Button::new(
                            egui::RichText::new(format!("{} now", op_label)).color(color)
                        );
                        if ui.add(confirm_btn).clicked() {
                            self.execute(db);
                        }
                        if ui.button("Cancel").clicked() {
                            self.confirmed = false;
                        }
                    });
                } else {
                    let can_begin = !self.destination.is_empty() && !self.files.is_empty();
                    ui.add_enabled_ui(can_begin, |ui| {
                        if ui.button("Begin…").clicked() {
                            self.confirmed = true;
                        }
                    });
                }
            });

        open
    }
}

fn strip_jpeg_exif(path: &Path) -> Result<(), String> {
    use img_parts::{Bytes, ImageEXIF, jpeg::Jpeg};
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
    if ext != "jpg" && ext != "jpeg" { return Ok(()); }
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let mut jpeg = Jpeg::from_bytes(Bytes::from(data)).map_err(|e| e.to_string())?;
    jpeg.set_exif(None);
    std::fs::write(path, jpeg.encoder().bytes()).map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// File system helpers
// ---------------------------------------------------------------------------

fn collect_images(base: &Path, recursive: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_inner(base, recursive, &mut out);
    out
}

fn collect_inner(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && recursive {
            collect_inner(&path, true, out);
        } else if path.is_file() && SupportedFormat::from_path(&path).is_some() {
            out.push(path);
        }
    }
}
