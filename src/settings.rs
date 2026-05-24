//! Persistent application settings, serialised as JSON to the platform config
//! directory (`~/.config/rivett/settings.json` on Linux, etc.).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// How image files within a directory are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    #[default]
    Name,
    DateModified,
    FileSize,
}

/// Which database(s) Rivett reads and writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DbMode {
    /// Single SQLite file at a user-level path.
    #[default]
    Central,
    /// `.rivett.db` file co-located with each image directory.
    Local,
    /// Central and local DBs are both maintained; conflict resolution applies.
    Both,
}

// ---------------------------------------------------------------------------
// Window geometry
// ---------------------------------------------------------------------------

/// Saved window position and size, restored on next launch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub x:      i32,
    pub y:      i32,
    pub width:  u32,
    pub height: u32,
}

impl Default for WindowGeometry {
    fn default() -> Self {
        Self { x: 100, y: 100, width: 1200, height: 800 }
    }
}

// ---------------------------------------------------------------------------
// AppSettings
// ---------------------------------------------------------------------------

/// Top-level persistent settings written to `settings.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    pub default_sort:    SortOrder,
    pub db_mode:         DbMode,
    /// Restored on next launch; `None` means use the hardcoded default size.
    pub window_geometry: Option<WindowGeometry>,
    /// Override for the central DB path; `None` → platform default.
    pub central_db_path: Option<PathBuf>,
    /// Whether the info panel is visible.
    pub show_info_panel: bool,
    /// Default state for "Preserve metadata" in Save As dialog.
    pub preserve_metadata: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            default_sort:    SortOrder::Name,
            db_mode:         DbMode::Central,
            window_geometry: None,
            central_db_path: None,
            show_info_panel: false, // Collapsed by default
            preserve_metadata: true,
        }
    }
}

impl AppSettings {
    /// Platform-specific config directory for Rivett.
    pub fn config_dir() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("rivett"))
    }

    pub fn config_path() -> Option<PathBuf> {
        Self::config_dir().map(|d| d.join("settings.json"))
    }

    /// Load settings from disk; returns defaults on any error.
    pub fn load() -> Self {
        let Some(path) = Self::config_path() else { return Self::default() };
        let Ok(content) = std::fs::read_to_string(&path) else { return Self::default() };
        serde_json::from_str(&content).unwrap_or_default()
    }

    /// Persist settings to disk; creates parent directories as needed.
    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = Self::config_path()
            .ok_or("unable to determine config directory")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Resolved path for the central SQLite database.
    pub fn central_db_resolved(&self) -> Option<PathBuf> {
        if let Some(ref p) = self.central_db_path {
            return Some(p.clone());
        }
        dirs::data_local_dir().map(|d| d.join("rivett").join("ratings.db"))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_are_sane() {
        let s = AppSettings::default();
        assert_eq!(s.default_sort, SortOrder::Name);
        assert_eq!(s.db_mode, DbMode::Central);
        assert!(s.window_geometry.is_none());
        assert!(s.central_db_path.is_none());
        assert!(!s.show_info_panel);
    }

    #[test]
    fn sort_order_serialises_to_snake_case() {
        assert_eq!(serde_json::to_string(&SortOrder::Name).unwrap(),         r#""name""#);
        assert_eq!(serde_json::to_string(&SortOrder::DateModified).unwrap(), r#""date_modified""#);
        assert_eq!(serde_json::to_string(&SortOrder::FileSize).unwrap(),     r#""file_size""#);
    }

    #[test]
    fn sort_order_round_trips() {
        for order in [SortOrder::Name, SortOrder::DateModified, SortOrder::FileSize] {
            let json  = serde_json::to_string(&order).unwrap();
            let back: SortOrder = serde_json::from_str(&json).unwrap();
            assert_eq!(order, back);
        }
    }

    #[test]
    fn db_mode_round_trips() {
        for mode in [DbMode::Central, DbMode::Local, DbMode::Both] {
            let json  = serde_json::to_string(&mode).unwrap();
            let back: DbMode = serde_json::from_str(&json).unwrap();
            assert_eq!(mode, back);
        }
    }

    #[test]
    fn settings_round_trip_through_json() {
        let original = AppSettings {
            default_sort:    SortOrder::FileSize,
            db_mode:         DbMode::Both,
            window_geometry: Some(WindowGeometry { x: 50, y: 50, width: 1920, height: 1080 }),
            central_db_path: Some(PathBuf::from("/tmp/test.db")),
            show_info_panel: true,
        };
        let json     = serde_json::to_string(&original).unwrap();
        let restored: AppSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.default_sort,    original.default_sort);
        assert_eq!(restored.db_mode,         original.db_mode);
        assert_eq!(restored.central_db_path, original.central_db_path);
        assert_eq!(restored.show_info_panel, original.show_info_panel);
        let geom = restored.window_geometry.unwrap();
        assert_eq!(geom.width, 1920);
        assert_eq!(geom.height, 1080);
    }
}
