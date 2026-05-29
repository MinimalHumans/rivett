# <img src="resources/icon.png" width="48" height="48" valign="middle"> Rivett

Rivett is a fast, keyboard-driven image viewer for sorting and vetting large collections. Built for photographers and digital artists who need to move quickly through images without the interface getting in the way.

## Features

- **Performance:** Built with Rust and OpenGL for smooth panning and zooming, even with large files.
- **32-bit / HDR Imaging:** Full 32-bit float pipeline with hardware-accelerated gamma, exposure, and black/white point controls. Histogram remap handles and clipping indicators are available for HDR images (EXR, RAW).
- **Format Support:** Standard web formats, OpenEXR, SVG, and professional Camera RAW formats (.CR3, .ARW, .NEF, .RAF, .DNG, and more) via LibRaw.
- **AI Metadata:** Prompts from ComfyUI, Stable Diffusion (A1111), and MidJourney are surfaced at the top of the metadata panel rather than buried in raw JSON.
- **Minimalist UI:** Optional info panel for metadata, EXIF, ratings, and histogram. All controls are documented in the in-app help menu (`?`).

## Controls

All hotkeys and mouse interactions are listed in the in-app help menu. Open it with `?` or via the right-click context menu.

## Supported Formats

- **Standard:** PNG, JPEG, WebP, BMP, GIF, TIFF
- **Production:** OpenEXR (.exr), SVG
- **Camera RAW:** Canon (.CR2, .CR3), Sony (.ARW), Nikon (.NEF, .NRW), Fujifilm (.RAF), Olympus (.ORF), Panasonic (.RW2), Adobe Digital Negative (.DNG), and others via LibRaw

## License

MIT
