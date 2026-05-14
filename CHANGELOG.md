# Changelog

All notable changes to Rivett are recorded here.

---

## [v0.3.0] — 2026-05-14

This release introduces significant upgrades to the viewer's core capabilities, specifically targeting high-dynamic-range (HDR) workflows and UI efficiency.

### High Dynamic Range & Imaging

- **Shader-Based Adjustments:** Full 32-bit float support with hardware-accelerated gamma and exposure controls.
- **HDR-Specific Tools:** Added Min/Max range settings and out-of-range warnings in the histogram (active for 32-bit images only).
- **Dynamic UI:** Histogram remap handles and labels now contextually hide when viewing 8-bit images to keep the interface clean.
- **Diffusion Metadata:** Improved handling of AI diffusion prompts, raising them out of general metadata for better visibility.

### User Interface & Navigation

- **Help Overlay:** Added a new dedicated help overlay and a comprehensive list of hotkeys and special clicks for quicker onboarding.
- **Refined Adjustments:** Image adjustment sliders now feature tooltips, a cleaner grid layout, and **double-click to reset** functionality.
- **Navigation:** Added parent directory navigation and improved "jump" behaviour — `PgUp`/`PgDn` and `Arrow Up`/`Arrow Down` now skip 10 items at a time.
- **Toasts:** Implemented contextual toast notifications for non-intrusive feedback on background actions.

### Hotkey & Control Polish

- `F` is now the exclusive toggle for zoom (removing `Ctrl+0`).
- `Ctrl+Left-drag` is the standardised trigger for drag-out actions.
- Removed `M` hotkey for metadata stripping; this is now a one-shot button in the context menu.
- Hotkeys are now globally disabled while an input field has focus to prevent accidental triggers.

### Bug Fixes

- Fixed zoom/scale inconsistencies and capped default zoom to 100%.
- Corrected padding for drag-thumbnails.
- Standardised "Delete" behaviour to use the system Recycle Bin.
- Fixed UI rendering issues with black/white point handles.

---

## [v0.2.3] — 2026-05-06

- Better histogram
- Organised metadata display
- Revised `F` hotkey toggle to cycle between current zoom, fit, and 100%
- DNG/TIFF loading is smoother
- `Home`/`End` keys supported
- Delete and Hide preserve browsing position

---

## [v0.2.2] — 2026-04-23

---

## [v0.2.1] — 2026-04-23

---

## [v0.2.0] — 2026-04-23
