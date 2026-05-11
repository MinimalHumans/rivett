//! Ephemeral per-session state.
//!
//! Nothing in this module is ever written to disk. `Ctrl+Shift+R` calls
//! [`SessionState::flush`] to wipe it all, subject to a save-or-discard
//! prompt if [`SessionState::has_pending_changes`] is true.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::settings::SortOrder;

// ---------------------------------------------------------------------------
// Rotation
// ---------------------------------------------------------------------------

/// Net clockwise rotation applied to an image for the current session only.
/// Stored as a cumulative quarter-turn; pixel data is never touched until save.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    #[default]
    None,
    Cw90,
    Cw180,
    Cw270,
}

impl Rotation {
    pub fn rotate_cw(self) -> Self {
        match self {
            Self::None  => Self::Cw90,
            Self::Cw90  => Self::Cw180,
            Self::Cw180 => Self::Cw270,
            Self::Cw270 => Self::None,
        }
    }

    pub fn rotate_ccw(self) -> Self {
        match self {
            Self::None  => Self::Cw270,
            Self::Cw90  => Self::None,
            Self::Cw180 => Self::Cw90,
            Self::Cw270 => Self::Cw180,
        }
    }

    pub fn is_identity(self) -> bool {
        self == Self::None
    }

    /// Net clockwise degrees: 0, 90, 180, or 270.
    pub fn degrees(self) -> u32 {
        match self {
            Self::None  => 0,
            Self::Cw90  => 90,
            Self::Cw180 => 180,
            Self::Cw270 => 270,
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Self::None  => 0,
            Self::Cw90  => 1,
            Self::Cw180 => 2,
            Self::Cw270 => 3,
        }
    }

    pub fn from_u8(val: u8) -> Self {
        match val % 4 {
            0 => Self::None,
            1 => Self::Cw90,
            2 => Self::Cw180,
            3 => Self::Cw270,
            _ => unreachable!(),
        }
    }
}

// ---------------------------------------------------------------------------
// Crop
// ---------------------------------------------------------------------------

/// Pixel-space crop rectangle applied to an image during this session.
/// Not written to disk until the user explicitly saves (destructive path).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CropRect {
    pub x:      f32,
    pub y:      f32,
    pub width:  f32,
    pub height: f32,
}

impl CropRect {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self { x, y, width, height }
    }

    pub fn is_valid(self) -> bool {
        self.width > 0.0 && self.height > 0.0
    }
}

// ---------------------------------------------------------------------------
// Rating filter
// ---------------------------------------------------------------------------

/// Comparison operator for a rating predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RatingFilterOp {
    AtLeast,
    AtMost,
    Exactly,
}

/// A rating predicate used to filter the directory listing for this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RatingFilter {
    pub op:          RatingFilterOp,
    pub value:       u8,
    pub path_prefix: Option<PathBuf>,
}

impl RatingFilter {
    /// Returns `true` if `rating` satisfies this predicate.
    pub fn matches(&self, path: &Path, rating: Option<u8>) -> bool {
        if let Some(prefix) = &self.path_prefix {
            if !path.starts_with(prefix) {
                return false;
            }
        }

        match rating {
            None    => false,
            Some(r) => match self.op {
                RatingFilterOp::AtLeast => r >= self.value,
                RatingFilterOp::AtMost  => r <= self.value,
                RatingFilterOp::Exactly => r == self.value,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// SessionState
// ---------------------------------------------------------------------------

/// All state that lives for exactly one run of the application.
#[derive(Debug, Default)]
pub struct SessionState {
    pub pending_rotations: HashMap<PathBuf, Rotation>,
    pub pending_crops:     HashMap<PathBuf, CropRect>,
    pub pending_metadata_strips: HashSet<PathBuf>,
    pub ignored_images:    HashSet<PathBuf>,
    pub rating_filter:     Option<RatingFilter>,
    pub sort_order:        SortOrder,
}

impl SessionState {
    pub fn new(default_sort: SortOrder) -> Self {
        Self {
            sort_order: default_sort,
            ..Default::default()
        }
    }

    // ── Pending-change tracking ───────────────────────────────────────────

    /// `true` if any save-or-discard prompt should be shown before closing.
    pub fn has_pending_changes(&self) -> bool {
        !self.pending_crops.is_empty() || !self.pending_rotations.is_empty() || !self.pending_metadata_strips.is_empty()
    }

    /// Clear all session state (hard refresh / `Ctrl+Shift+R`).
    pub fn flush(&mut self) {
        self.pending_rotations.clear();
        self.pending_crops.clear();
        self.pending_metadata_strips.clear();
        self.ignored_images.clear();
        self.rating_filter = None;
    }

    // ── Metadata Strip ────────────────────────────────────────────────────

    pub fn toggle_metadata_strip(&mut self, path: PathBuf) {
        if self.pending_metadata_strips.contains(&path) {
            self.pending_metadata_strips.remove(&path);
        } else {
            self.pending_metadata_strips.insert(path);
        }
    }

    pub fn is_metadata_stripped(&self, path: &PathBuf) -> bool {
        self.pending_metadata_strips.contains(path)
    }

    // ── Rotation ──────────────────────────────────────────────────────────

    pub fn rotate_cw(&mut self, path: PathBuf) -> Rotation {
        let next = self.rotation_for(&path).rotate_cw();
        self.set_rotation(path, next);
        next
    }

    pub fn rotate_ccw(&mut self, path: PathBuf) -> Rotation {
        let next = self.rotation_for(&path).rotate_ccw();
        self.set_rotation(path, next);
        next
    }

    pub fn set_rotation(&mut self, path: PathBuf, rotation: Rotation) {
        if rotation.is_identity() {
            self.pending_rotations.remove(&path);
        } else {
            self.pending_rotations.insert(path, rotation);
        }
    }

    pub fn rotation_for(&self, path: &PathBuf) -> Rotation {
        self.pending_rotations.get(path).copied().unwrap_or_default()
    }

    // ── Crop ──────────────────────────────────────────────────────────────

    pub fn set_crop(&mut self, path: PathBuf, crop: CropRect) {
        self.pending_crops.insert(path, crop);
    }

    pub fn clear_crop(&mut self, path: &PathBuf) {
        self.pending_crops.remove(path);
    }

    pub fn crop_for(&self, path: &PathBuf) -> Option<CropRect> {
        self.pending_crops.get(path).copied()
    }

    // ── Ignore ────────────────────────────────────────────────────────────

    pub fn ignore_image(&mut self, path: PathBuf) {
        self.ignored_images.insert(path);
    }

    pub fn is_ignored(&self, path: &PathBuf) -> bool {
        self.ignored_images.contains(path)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf { PathBuf::from(s) }

    // ── Rotation ──────────────────────────────────────────────────────────

    #[test]
    fn rotation_cw_cycles_through_all_steps() {
        assert_eq!(Rotation::None.rotate_cw(),  Rotation::Cw90);
        assert_eq!(Rotation::Cw90.rotate_cw(),  Rotation::Cw180);
        assert_eq!(Rotation::Cw180.rotate_cw(), Rotation::Cw270);
        assert_eq!(Rotation::Cw270.rotate_cw(), Rotation::None);
    }

    #[test]
    fn rotation_ccw_cycles_through_all_steps() {
        assert_eq!(Rotation::None.rotate_ccw(),  Rotation::Cw270);
        assert_eq!(Rotation::Cw90.rotate_ccw(),  Rotation::None);
        assert_eq!(Rotation::Cw180.rotate_ccw(), Rotation::Cw90);
        assert_eq!(Rotation::Cw270.rotate_ccw(), Rotation::Cw180);
    }

    #[test]
    fn four_cw_steps_return_to_identity() {
        let mut r = Rotation::None;
        for _ in 0..4 { r = r.rotate_cw(); }
        assert!(r.is_identity());
    }

    #[test]
    fn cw_and_ccw_cancel() {
        assert_eq!(Rotation::None.rotate_cw().rotate_ccw(), Rotation::None);
        assert_eq!(Rotation::Cw90.rotate_ccw().rotate_cw(), Rotation::Cw90);
    }

    #[test]
    fn degrees_are_correct() {
        assert_eq!(Rotation::None.degrees(),  0);
        assert_eq!(Rotation::Cw90.degrees(),  90);
        assert_eq!(Rotation::Cw180.degrees(), 180);
        assert_eq!(Rotation::Cw270.degrees(), 270);
    }

    #[test]
    fn rotation_u8_roundtrip() {
        for i in 0..4 {
            assert_eq!(Rotation::from_u8(i).as_u8(), i);
        }
    }

    // ── SessionState ─────────────────────────────────────────────────────

    #[test]
    fn new_session_has_no_pending_changes() {
        assert!(!SessionState::new(SortOrder::Name).has_pending_changes());
    }

    #[test]
    fn crop_creates_pending_change_and_clears() {
        let mut s = SessionState::new(SortOrder::Name);
        let path = p("/img.jpg");
        s.set_crop(path.clone(), CropRect::new(0.0, 0.0, 100.0, 100.0));
        assert!(s.has_pending_changes());
        s.clear_crop(&path);
        assert!(!s.has_pending_changes());
    }

    #[test]
    fn flush_clears_all_state() {
        let mut s = SessionState::new(SortOrder::Name);
        s.rotate_cw(p("/img.jpg"));
        s.ignore_image(p("/other.jpg"));
        s.rating_filter = Some(RatingFilter { op: RatingFilterOp::AtLeast, value: 3, path_prefix: None });
        s.flush();
        assert!(!s.has_pending_changes());
        assert!(s.ignored_images.is_empty());
        assert!(s.rating_filter.is_none());
    }

    // ── RatingFilter ─────────────────────────────────────────────────────

    #[test]
    fn filter_at_least() {
        let f = RatingFilter { op: RatingFilterOp::AtLeast, value: 3, path_prefix: None };
        let path = PathBuf::from("dummy.jpg");
        assert!(!f.matches(&path, None));
        assert!(!f.matches(&path, Some(2)));
        assert!( f.matches(&path, Some(3)));
        assert!( f.matches(&path, Some(5)));
    }

    #[test]
    fn filter_at_most() {
        let f = RatingFilter { op: RatingFilterOp::AtMost, value: 3, path_prefix: None };
        let path = PathBuf::from("dummy.jpg");
        assert!( f.matches(&path, Some(1)));
        assert!( f.matches(&path, Some(3)));
        assert!(!f.matches(&path, Some(4)));
        assert!(!f.matches(&path, None));
    }

    #[test]
    fn filter_exactly() {
        let f = RatingFilter { op: RatingFilterOp::Exactly, value: 3, path_prefix: None };
        let path = PathBuf::from("dummy.jpg");
        assert!(!f.matches(&path, Some(2)));
        assert!( f.matches(&path, Some(3)));
        assert!(!f.matches(&path, Some(4)));
        assert!(!f.matches(&path, None));
    }

    #[test]
    fn filter_with_path_prefix() {
        let prefix = PathBuf::from("/foo/bar");
        let f = RatingFilter { op: RatingFilterOp::AtLeast, value: 3, path_prefix: Some(prefix) };
        
        assert!( f.matches(&PathBuf::from("/foo/bar/img.jpg"), Some(3)));
        assert!( f.matches(&PathBuf::from("/foo/bar/baz/img.jpg"), Some(3)));
        assert!(!f.matches(&PathBuf::from("/foo/img.jpg"), Some(3)));
        assert!(!f.matches(&PathBuf::from("/other/img.jpg"), Some(3)));
    }

    // ── CropRect ─────────────────────────────────────────────────────────

    #[test]
    fn valid_crop_rect() {
        assert!( CropRect::new(10.0, 10.0, 100.0, 200.0).is_valid());
        assert!(!CropRect::new(10.0, 10.0,   0.0, 200.0).is_valid());
        assert!(!CropRect::new(10.0, 10.0, -10.0, 200.0).is_valid());
    }
}
