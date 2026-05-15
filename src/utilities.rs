//! Utility windows — accessible from the right-click context menu under "Utilities".
//! No hotkeys; menu-only.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use egui::Context;
use crate::db::Database;
use crate::formats::SupportedFormat;

// ---------------------------------------------------------------------------
// Top-level container
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct UtilitiesState {
    pub purge:     Option<PurgeState>,
    pub db_health: Option<DbHealthState>,
}

impl UtilitiesState {
    pub fn open_purge(&mut self, base_path: PathBuf) {
        self.purge = Some(PurgeState::new(base_path));
    }

    pub fn open_db_health(&mut self, db: Option<&Database>) {
        let total = db.and_then(|d| d.count_all_entries().ok()).unwrap_or(0);
        self.db_health = Some(DbHealthState::new(total));
    }

    pub fn draw(&mut self, ctx: &Context, db: Option<&Database>) {
        if let Some(ref mut state) = self.purge {
            if !state.draw(ctx, db) { self.purge = None; }
        }
        if let Some(ref mut state) = self.db_health {
            if !state.draw(ctx, db) { self.db_health = None; }
        }
    }
}

// ---------------------------------------------------------------------------
// Path summarisation
// ---------------------------------------------------------------------------

/// One entry in the condensed path summary shown in the UI.
pub struct PathEntry {
    pub path:   String,
    pub counts: [usize; 6], // [unrated, ★1 … ★5]
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
                .map(|(path, counts)| PathEntry { path, counts })
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

                    // Counts table
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
                                let count = self.buckets[i].len();
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
                                            files:  self.buckets[i].clone(),
                                        });
                                    }
                                } else {
                                    ui.label(egui::RichText::new("—").weak());
                                }
                                ui.end_row();
                            }
                        });

                    // Directory breakdown (trie summary)
                    if !self.summary.is_empty() {
                        ui.add_space(8.0);
                        ui.separator();
                        ui.strong("Directory breakdown");
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical()
                            .id_source("purge_summary_scroll")
                            .max_height(140.0)
                            .show(ui, |ui| {
                                egui::Grid::new("purge_dir_summary")
                                    .num_columns(7)
                                    .spacing([8.0, 2.0])
                                    .show(ui, |ui| {
                                        ui.strong("Path");
                                        for h in &["∅", "★1", "★2", "★3", "★4", "★5"] {
                                            ui.strong(*h);
                                        }
                                        ui.end_row();
                                        for entry in &self.summary {
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
                    }
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
        let mut deleted = 0usize;
        let mut failed  = 0usize;
        for path in &confirm.files {
            match std::fs::remove_file(path) {
                Ok(())  => deleted += 1,
                Err(_)  => failed  += 1,
            }
        }
        self.buckets[confirm.bucket].clear();

        // Rebuild summary from remaining files
        let flat: Vec<(PathBuf, usize)> = self.buckets.iter().enumerate()
            .flat_map(|(i, files)| files.iter().map(move |f| (f.clone(), i)))
            .collect();
        self.summary = summarise_paths(&flat);

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
    total_entries:    usize,
    scanned:          bool,
    /// (dir_id, dir_path, filename) for each orphaned entry.
    orphans:          Vec<(i64, String, String)>,
    selected:         Vec<bool>,
    excluded:         Vec<String>,
    prefix_input:     String,
    confirm_prune:    bool,
    result:           Option<String>,
}

impl DbHealthState {
    pub fn new(total: usize) -> Self {
        Self {
            total_entries: total,
            scanned:       false,
            orphans:       vec![],
            selected:      vec![],
            excluded:      vec![],
            prefix_input:  String::new(),
            confirm_prune: false,
            result:        None,
        }
    }

    fn scan(&mut self, db: &Database) {
        let Ok(entries) = db.get_all_image_paths() else { return };
        self.orphans.clear();
        for (dir_id, dir_path, filename, _) in entries {
            if self.excluded.iter().any(|ex| dir_path.starts_with(ex.as_str())) {
                continue;
            }
            let path = PathBuf::from(&dir_path).join(&filename);
            if !path.exists() {
                self.orphans.push((dir_id, dir_path, filename));
            }
        }
        self.selected = vec![true; self.orphans.len()];
        self.scanned  = true;
    }

    fn prune(&mut self, db: &Database) {
        let mut pruned = 0usize;
        let mut failed  = 0usize;

        let mut keep_orphans:   Vec<(i64, String, String)> = Vec::new();
        let mut keep_selected:  Vec<bool>                  = Vec::new();

        for ((dir_id, dir_path, filename), &sel) in self.orphans.iter().zip(&self.selected) {
            if sel {
                match db.delete_image(*dir_id, filename) {
                    Ok(()) => pruned += 1,
                    Err(_) => {
                        failed += 1;
                        keep_orphans.push((*dir_id, dir_path.clone(), filename.clone()));
                        keep_selected.push(true);
                    }
                }
            } else {
                keep_orphans.push((*dir_id, dir_path.clone(), filename.clone()));
                keep_selected.push(false);
            }
        }

        self.orphans          = keep_orphans;
        self.selected         = keep_selected;
        self.total_entries    = self.total_entries.saturating_sub(pruned);
        self.result           = Some(format!(
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

                // Exclusion list
                ui.collapsing("Exclude path prefixes from scan", |ui| {
                    ui.label(egui::RichText::new(
                        "Use this to skip network drives or paths that may be temporarily unavailable."
                    ).small().weak());
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.prefix_input)
                            .hint_text("e.g. /mnt/nas or Z:\\")
                            .desired_width(300.0));
                        if ui.button("Add").clicked() {
                            let trimmed = self.prefix_input.trim().to_string();
                            if !trimmed.is_empty() && !self.excluded.contains(&trimmed) {
                                self.excluded.push(trimmed);
                                self.prefix_input.clear();
                            }
                        }
                    });
                    ui.add_space(4.0);
                    let mut to_remove = None;
                    for (i, prefix) in self.excluded.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.monospace(prefix);
                            if ui.small_button("✕").clicked() { to_remove = Some(i); }
                        });
                    }
                    if let Some(i) = to_remove { self.excluded.remove(i); }
                    if self.excluded.is_empty() {
                        ui.label(egui::RichText::new("No exclusions — all paths will be checked.").small().weak());
                    }
                });

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
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(format!(
                                "{} orphaned entr{} found:",
                                self.orphans.len(),
                                if self.orphans.len() == 1 { "y" } else { "ies" }
                            )).strong());
                            let all = self.selected.iter().all(|&s| s);
                            if ui.small_button(if all { "Deselect all" } else { "Select all" }).clicked() {
                                let v = !all;
                                for s in &mut self.selected { *s = v; }
                            }
                        });
                        ui.add_space(4.0);

                        egui::ScrollArea::vertical()
                            .id_source("orphan_list_scroll")
                            .max_height(220.0)
                            .show(ui, |ui| {
                                for (i, (_, dir, fname)) in self.orphans.iter().enumerate() {
                                    if let Some(sel) = self.selected.get_mut(i) {
                                        ui.horizontal(|ui| {
                                            ui.checkbox(sel, "");
                                            ui.add(egui::Label::new(
                                                egui::RichText::new(
                                                    format!("{}/{}", dir, fname)
                                                ).monospace().small()
                                            ).truncate());
                                        });
                                    }
                                }
                            });

                        ui.add_space(6.0);
                        let sel_count = self.selected.iter().filter(|&&s| s).count();
                        let prune_btn = egui::Button::new(
                            egui::RichText::new(format!(
                                "Prune {} selected entr{}…",
                                sel_count,
                                if sel_count == 1 { "y" } else { "ies" }
                            )).color(egui::Color32::from_rgb(220, 80, 80))
                        );
                        ui.add_enabled_ui(sel_count > 0, |ui| {
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
            let sel_count = self.selected.iter().filter(|&&s| s).count();
            let mut do_prune = false;
            let mut cancel   = false;

            egui::Window::new("Confirm Pruning")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(egui::RichText::new(format!(
                        "Remove {} orphaned database entr{}?",
                        sel_count,
                        if sel_count == 1 { "y" } else { "ies" }
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
