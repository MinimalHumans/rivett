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
}

#[derive(Default)]
struct TagEditorState {
    tags:              Vec<crate::db::TagRecord>,
    rename_old:        String,
    rename_new:        String,
    confirm_delete:    Option<String>,
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

    pub fn draw(&mut self, ctx: &Context, db: Option<&Database>) {
        if let Some(ref mut state) = self.purge {
            if !state.draw(ctx, db) { self.purge = None; }
        }
        if let Some(ref mut state) = self.db_health {
            if !state.draw(ctx, db) { self.db_health = None; }
        }
        self.draw_tag_editor(ctx, db);
    }

    fn draw_tag_editor(&mut self, ctx: &Context, db: Option<&Database>) {
        let Some(mut state) = self.tag_editor.take() else { return };
        let mut open = true;

        egui::Window::new("Tag Manager")
            .open(&mut open)
            .resizable(true)
            .default_width(300.0)
            .show(ctx, |ui| {
                if let Some(db) = db {
                    ui.label("Manage all unique tags in the database.");
                    ui.separator();

                    egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                        let mut tag_to_delete = None;
                        for tag in &state.tags {
                            ui.horizontal(|ui| {
                                ui.label(&tag.name);
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.button("Rename").clicked() {
                                        state.rename_old = tag.name.clone();
                                        state.rename_new = tag.name.clone();
                                    }
                                    if ui.button("Delete").clicked() {
                                        tag_to_delete = Some(tag.name.clone());
                                    }
                                });
                            });
                        }

                        if let Some(tag_name) = tag_to_delete {
                            state.confirm_delete = Some(tag_name);
                        }
                    });

                    if !state.rename_old.is_empty() {
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label(format!("Rename '{}' to:", state.rename_old));
                            ui.text_edit_singleline(&mut state.rename_new);
                            if ui.button("OK").clicked() {
                                let _ = db.rename_tag(&state.rename_old, &state.rename_new);
                                state.rename_old.clear();
                                if let Ok(t) = db.get_all_tags() { state.tags = t; }
                            }
                            if ui.button("Cancel").clicked() {
                                state.rename_old.clear();
                            }
                        });
                    }

                    if let Some(tag) = state.confirm_delete.clone() {
                        egui::Window::new("Delete Tag?")
                            .collapsible(false)
                            .resizable(false)
                            .pivot(egui::Align2::CENTER_CENTER)
                            .show(ctx, |ui| {
                                ui.label(format!("Are you sure you want to delete the tag '{}' from ALL images?", tag));
                                ui.horizontal(|ui| {
                                    if ui.button("Yes, Delete").clicked() {
                                        let _ = db.delete_tag(&tag);
                                        state.confirm_delete = None;
                                        if let Ok(t) = db.get_all_tags() { state.tags = t; }
                                    }
                                    if ui.button("Cancel").clicked() {
                                        state.confirm_delete = None;
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
