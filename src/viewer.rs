//! Viewer canvas: zoom, pan, rotation application, and texture management.
//!
//! [`ViewerState`] is purely logical state; the actual egui painting happens
//! in `app.rs`. GPU textures are owned here via [`egui::TextureHandle`].

use egui::{Context, Rect, TextureHandle, TextureOptions, Vec2};
use crate::image_loader::DecodedImage;
use crate::session::Rotation;

// ---------------------------------------------------------------------------
// ViewerMode
// ---------------------------------------------------------------------------

/// The current interaction mode of the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewerMode {
    /// Normal pan/zoom navigation.
    #[default]
    Navigate,
    /// User is drawing a rectangular crop/selection (`S` key).
    Selection,
    /// R/G/B/A channel inspection overlay (`K` key).
    ChannelView,
}

// ---------------------------------------------------------------------------
// ViewerState
// ---------------------------------------------------------------------------

/// All state required to render and interact with the image canvas.
pub struct ViewerState {
    /// The GPU texture for the currently displayed image (with rotation and gamma).
    pub texture: Option<TextureHandle>,
    /// Current zoom level (1.0 = 100 %).
    pub zoom: f32,
    /// Canvas pan offset in logical pixels.
    pub pan: Vec2,
    /// Native (post-rotation) pixel size of the current image.
    pub image_size: Vec2,
    /// When `true`, zoom is recalculated every frame to fit the canvas.
    pub fit_to_window: bool,
    /// Zoom saved before entering fit-to-window mode, for toggling back.
    pub saved_zoom: Option<f32>,
    /// Current interaction mode.
    pub mode: ViewerMode,
    /// In-progress selection rectangle in canvas coordinates.
    pub selection: Option<Rect>,
    /// Whether the window is currently in fullscreen mode.
    pub fullscreen: bool,
    /// Last error message if loading failed.
    pub load_error: Option<String>,
    /// Viewing gamma (default 1.0).
    pub gamma: f32,
    
    // Internal cache for re-applying gamma/rotation without disk access.
    last_image:    Option<DecodedImage>,
    last_rotation: Rotation,
}

impl Default for ViewerState {
    fn default() -> Self {
        Self {
            texture:        None,
            zoom:           1.0,
            pan:            Vec2::ZERO,
            image_size:     Vec2::ZERO,
            fit_to_window:  true,
            saved_zoom:     None,
            mode:           ViewerMode::default(),
            selection:      None,
            fullscreen:     false,
            load_error:     None,
            gamma:          1.0,
            last_image:     None,
            last_rotation:  Rotation::None,
        }
    }
}

impl ViewerState {
    pub fn new() -> Self { Self::default() }

    /// Load a decoded image into egui.
    pub fn load_image(&mut self, ctx: &Context, img: &DecodedImage, rotation: Rotation, preserve_zoom: bool) {
        self.load_error   = None;
        self.last_image   = Some(img.clone());
        self.last_rotation = rotation;
        
        self.refresh_texture(ctx);

        if !preserve_zoom {
            self.fit_to_window = true;
            self.pan = Vec2::ZERO;
        }
    }

    /// Re-calculate the texture using current gamma and rotation settings.
    pub fn refresh_texture(&mut self, ctx: &Context) {
        let Some(ref img) = self.last_image else { return };
        
        // 1. Apply rotation
        let (rgba, w, h) = apply_rotation(img, self.last_rotation);
        
        // 2. Apply viewing gamma if not 1.0
        let final_rgba = if (self.gamma - 1.0).abs() > 0.001 {
            let inv_gamma = 1.0 / self.gamma;
            rgba.iter().enumerate().map(|(i, &v)| {
                if i % 4 == 3 { v } // Skip alpha
                else {
                    let f = v as f32 / 255.0;
                    (f.powf(inv_gamma) * 255.0).clamp(0.0, 255.0) as u8
                }
            }).collect()
        } else {
            rgba
        };

        let color_image = egui::ColorImage::from_rgba_unmultiplied([w, h], &final_rgba);
        self.image_size = Vec2::new(w as f32, h as f32);
        self.texture    = Some(ctx.load_texture(
            "current_image",
            color_image,
            TextureOptions::default(),
        ));
    }

    pub fn set_gamma(&mut self, gamma: f32, ctx: &Context) {
        if (self.gamma - gamma).abs() > 0.001 {
            self.gamma = gamma;
            self.refresh_texture(ctx);
        }
    }

    /// Clear the canvas.
    pub fn clear(&mut self) {
        self.texture    = None;
        self.image_size = Vec2::ZERO;
        self.pan        = Vec2::ZERO;
        self.last_image = None;
        self.gamma      = 1.0;
    }

    pub fn set_error(&mut self, err: String) {
        self.clear();
        self.load_error = Some(err);
    }

    /// Reset zoom to 100% (1:1 pixel mapping).
    pub fn zoom_actual_size(&mut self) {
        self.fit_to_window = false;
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;
    }

    /// Cycle between "Fit to window", "100%", and "Custom zoom".
    pub fn toggle_fit(&mut self, canvas_size: Vec2) {
        if self.fit_to_window {
            // From Fit -> 100%
            self.fit_to_window = false;
            self.zoom = 1.0;
            self.pan = Vec2::ZERO;
        } else if (self.zoom - 1.0).abs() < 1e-3 {
            // From 100% -> Custom (if exists) or Fit
            if let Some(saved) = self.saved_zoom.take() {
                if (saved - 1.0).abs() > 1e-3 {
                    self.zoom = saved;
                    return;
                }
            }
            self.fit_to_window = true;
            self.pan = Vec2::ZERO;
            self.recalc_fit(canvas_size);
        } else {
            // From Custom -> Fit
            self.saved_zoom = Some(self.zoom);
            self.fit_to_window = true;
            self.pan = Vec2::ZERO;
            self.recalc_fit(canvas_size);
        }
    }

    /// Update `zoom` to fit the current `image_size` inside `canvas_size`.
    pub fn recalc_fit(&mut self, canvas_size: Vec2) {
        if !self.fit_to_window || self.image_size.x <= 0.0 || self.image_size.y <= 0.0 {
            return;
        }
        let ratio_x = canvas_size.x / self.image_size.x;
        let ratio_y = canvas_size.y / self.image_size.y;
        self.zoom = ratio_x.min(ratio_y).min(1.0);
    }

    pub fn apply_zoom_delta(&mut self, delta: f32, cursor: Option<egui::Pos2>, canvas: Rect) {
        self.fit_to_window = false;
        let old_zoom = self.zoom;
        self.zoom = (self.zoom * delta).clamp(0.01, 50.0);

        if let Some(c) = cursor {
            let center    = canvas.center();
            let relative  = c - center - self.pan;
            let pixel_pos = relative / old_zoom;
            self.pan      = c - center - (pixel_pos * self.zoom);
        }
    }

    /// Returns the screen-space rectangle where the image should be painted.
    pub fn image_rect(&self, canvas: Rect) -> Rect {
        let size = self.image_size * self.zoom;
        Rect::from_center_size(canvas.center() + self.pan, size)
    }

    pub fn has_image(&self) -> bool {
        self.texture.is_some()
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn apply_rotation(img: &DecodedImage, rotation: Rotation) -> (Vec<u8>, usize, usize) {
    if rotation.is_identity() {
        return (img.rgba.clone(), img.width as usize, img.height as usize);
    }

    let mut dimg = image::DynamicImage::ImageRgba8(
        image::ImageBuffer::from_raw(img.width, img.height, img.rgba.clone()).unwrap()
    );

    dimg = match rotation {
        Rotation::None  => dimg,
        Rotation::Cw90  => dimg.rotate90(),
        Rotation::Cw180 => dimg.rotate180(),
        Rotation::Cw270 => dimg.rotate270(),
    };

    let rgba = dimg.to_rgba8();
    let (w, h) = rgba.dimensions();
    (rgba.into_raw(), w as usize, h as usize)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_viewer_has_no_image() {
        let v = ViewerState::new();
        assert!(v.texture.is_none());
        assert_eq!(v.image_size, Vec2::ZERO);
    }

    #[test]
    fn zoom_actual_size_resets_to_100_percent() {
        let mut v = ViewerState::new();
        v.zoom = 5.0;
        v.zoom_actual_size();
        assert_eq!(v.zoom, 1.0);
        assert!(!v.fit_to_window);
        assert_eq!(v.pan, Vec2::ZERO);
    }

    #[test]
    fn zoom_is_clamped_at_minimum() {
        let mut v = ViewerState::new();
        v.apply_zoom_delta(0.00001, None, Rect::NOTHING);
        assert!(v.zoom >= 0.01);
    }

    #[test]
    fn zoom_is_clamped_at_maximum() {
        let mut v = ViewerState::new();
        v.zoom = 40.0;
        v.apply_zoom_delta(10.0, None, Rect::NOTHING);
        assert!(v.zoom <= 50.0);
    }

    #[test]
    fn toggle_fit_switches_modes_and_restores_zoom() {
        let mut v = ViewerState::new();
        v.fit_to_window = false;
        v.image_size = Vec2::new(1000.0, 1000.0);
        v.zoom = 0.5;
        
        v.toggle_fit(Vec2::new(100.0, 100.0));
        assert!(v.fit_to_window);
        assert_eq!(v.zoom, 0.1); // fits 1000 into 100
        assert_eq!(v.pan, Vec2::ZERO);
        
        v.toggle_fit(Vec2::new(100.0, 100.0));
        assert!(!v.fit_to_window);
        assert_eq!(v.zoom, 0.5); // restored
    }

    #[test]
    fn fit_zoom_handles_zero_sizes() {
        let mut v = ViewerState::new();
        v.fit_to_window = true;
        v.recalc_fit(Vec2::ZERO);
        assert_eq!(v.zoom, 1.0); // unchanged from default
    }

    #[test]
    fn fit_zoom_respects_shorter_dimension() {
        let mut v = ViewerState::new();
        v.image_size = Vec2::new(1000.0, 1000.0);
        v.fit_to_window = true;
        v.recalc_fit(Vec2::new(500.0, 200.0));
        assert_eq!(v.zoom, 0.2); // constrained by height
    }

    #[test]
    fn fit_zoom_respects_narrower_dimension() {
        let mut v = ViewerState::new();
        v.image_size = Vec2::new(1000.0, 1000.0);
        v.fit_to_window = true;
        v.recalc_fit(Vec2::new(200.0, 500.0));
        assert_eq!(v.zoom, 0.2); // constrained by width
    }

    #[test]
    fn image_rect_is_centred_when_pan_is_zero() {
        let mut v    = ViewerState::new();
        v.image_size = Vec2::new(400.0, 300.0);
        v.zoom       = 1.0;
        let canvas   = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(800.0, 600.0));
        v.pan         = Vec2::ZERO;

        let rect = v.image_rect(canvas);
        assert!((rect.min.x - 200.0).abs() < 1e-4, "min.x = {}", rect.min.x);
        assert!((rect.min.y - 150.0).abs() < 1e-4, "min.y = {}", rect.min.y);
    }
}
