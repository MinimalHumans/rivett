# Keyboard & Mouse Interactions

## Keyboard Shortcuts

> Hotkeys are disabled while any text input / widget has keyboard focus.

### Navigation

| Key | Action |
|-----|--------|
| `→` | Next image |
| `←` | Previous image |
| `↓` / `Page Down` | Jump forward 10 images |
| `↑` / `Page Up` | Jump back 10 images |
| `Alt+↑` | Navigate to parent directory (opens first image there) |
| `Home` | First image |
| `End` | Last image |
| `Shift` + navigate | Preserve current zoom level while navigating |

### View

| Key | Action |
|-----|--------|
| `F` | Toggle fit-to-window / actual size (100%) |
| `I` | Toggle info panel |

### Rating

| Key | Action |
|-----|--------|
| `1`–`5` | Set star rating |
| `0` | Clear rating |

### Image Adjustments

| Key | Action |
|-----|--------|
| `[` | Rotate counter-clockwise |
| `]` | Rotate clockwise |

### File Management

| Key | Action |
|-----|--------|
| `H` | Hide / ignore current image |
| `Delete` (first press) | Arm delete — opens a 4-second confirm window |
| `Delete` (second press, within 4 s) | Move to trash |
| `Escape` | Cancel armed delete |

### Save & Refresh

| Key | Action |
|-----|--------|
| `Ctrl+S` | Save rotation / crop / metadata changes |
| `Ctrl+Shift+S` | Save As (choose output path) |
| `Ctrl+R` | Soft refresh — reload directory, preserve position |
| `Ctrl+Shift+R` | Hard refresh — reset session state, reload directory |

---

## Canvas Mouse Interactions

| Interaction | Action |
|-------------|--------|
| **Left-drag** | Pan image |
| **Ctrl + Left-drag** | Drag-out: export current file to OS (Windows only, includes thumbnail) |
| **Scroll wheel** | Zoom in/out (1.1× per tick, centered on cursor) |
| **Pinch gesture** (trackpad) | Smooth zoom centered on cursor |
| **Double-click** | Open file picker |
| **Double-click** (on load error) | Copy error message to clipboard |
| **Single-click** (on load error) | Copy error message to clipboard |
| **Right-click** | Context menu (all actions with shortcut hints, includes "Strip metadata" action) |
| **Drop file onto window** | Open dropped image |

---

## Histogram & Adjustment Controls

### Histogram handles (black / white point)

| Interaction | Action |
|-------------|--------|
| **Drag min handle** (left) | Adjust black point |
| **Drag max handle** (right) | Adjust white point |
| **Drag histogram background** (between handles) | Shift both min and max together |
| **Double-click min handle** | Reset black point to 0.0 |
| **Double-click max handle** | Reset white point to 1.0 |
| Hover on either handle | Handle brightens to white (visual feedback) |

### Black / white point numeric inputs (DragValue)

| Interaction | Action |
|-------------|--------|
| **Double-click min DragValue** | Reset black point to 0.0 |
| **Double-click max DragValue** | Reset white point to 1.0 |

### Exposure

| Interaction | Action |
|-------------|--------|
| **Drag DragValue or slider** | Adjust exposure |
| **Shift + drag DragValue** | Coarse adjustment — 10× faster, snaps to 0.5 EV steps |
| **Double-click slider** | Reset exposure to 0.0 |

### Gamma

| Interaction | Action |
|-------------|--------|
| **Drag DragValue or slider** | Adjust gamma |
| **Double-click slider** | Reset gamma to 1.0 |
