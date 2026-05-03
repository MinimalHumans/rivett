//! Image metadata extraction.
//!
//! Currently reads PNG `tEXt` and `iTXt` chunks, which is where ComfyUI,
//! Automatic1111, and InvokeAI embed their workflow/prompt data.
//! EXIF (JPEG/TIFF/WebP/RAW) support is implemented via the `exif` crate.

use std::path::Path;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};

/// A single key/value metadata entry.
#[derive(Debug, Clone)]
pub struct MetaEntry {
    pub key:   String,
    /// Raw value string, potentially very long (ComfyUI JSON can be MBs).
    pub value: String,
}

/// Extract all readable metadata from a file.
/// Returns an empty vec for unsupported formats or unreadable files.
pub fn read_metadata(path: &Path) -> Vec<MetaEntry> {
    let Ok(file) = File::open(path) else { return vec![] };
    let reader = BufReader::new(file);
    let img_reader = image::ImageReader::new(reader).with_guessed_format();
    
    let mut entries = if let Ok(reader) = img_reader {
        match reader.format() {
            Some(image::ImageFormat::Png)  => read_png(path),
            Some(image::ImageFormat::Jpeg) => read_exif(path),
            Some(image::ImageFormat::Tiff) => read_exif(path),
            Some(image::ImageFormat::WebP) => read_exif(path),
            _                              => read_exif_with_fallback(path),
        }
    } else {
        read_exif_with_fallback(path)
    };

    post_process_metadata(&mut entries);

    entries
}

fn post_process_metadata(entries: &mut Vec<MetaEntry>) {
    let mut make = None;
    let mut model = None;
    let mut to_remove = std::collections::HashSet::new();

    for (i, entry) in entries.iter().enumerate() {
        match entry.key.as_str() {
            "Make" => make = Some(entry.value.clone()),
            "Model" => model = Some(entry.value.clone()),
            "ImageWidth" | "ImageLength" | "PixelXDimension" | "PixelYDimension" => {
                to_remove.insert(i);
            }
            _ => {}
        }
    }

    if make.is_some() || model.is_some() {
        let combined = match (make, model) {
            (Some(mk), Some(md)) => {
                if md.to_lowercase().contains(&mk.to_lowercase()) {
                    md
                } else {
                    format!("{} {}", mk, md)
                }
            }
            (Some(mk), None) => mk,
            (None, Some(md)) => md,
            _ => unreachable!(),
        };

        // Remove old make/model entries
        for (i, entry) in entries.iter().enumerate() {
            if entry.key == "Make" || entry.key == "Model" {
                to_remove.insert(i);
            }
        }

        entries.push(MetaEntry {
            key: "Device".to_string(),
            value: combined,
        });
    }

    // Process Orientation and JSON
    for entry in entries.iter_mut() {
        if entry.key == "Orientation" {
            entry.value = match entry.value.as_str() {
                "Horizontal (normal)" => "0°".to_string(),
                "Rotate 90 CW" => "90° CW".to_string(),
                "Rotate 180" => "180°".to_string(),
                "Rotate 270 CW" => "270° CW".to_string(),
                _ => entry.value.clone(),
            };
        }

        // 1. Try JSON pretty-print (ComfyUI workflow/prompt, InvokeAI metadata)
        if entry.value.trim().starts_with('{') || entry.value.trim().starts_with('[') {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&entry.value) {
                if let Ok(pretty) = serde_json::to_string_pretty(&val) {
                    entry.value = pretty;
                }
            }
        }
    }

    let mut i = 0;
    entries.retain(|_| {
        let keep = !to_remove.contains(&i);
        i += 1;
        keep
    });
}

fn read_exif_with_fallback(path: &Path) -> Vec<MetaEntry> {
    if is_raw_extension(path) {
        read_exif(path)
    } else {
        vec![]
    }
}

fn is_raw_extension(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|s| s.to_str()) else { return false };
    matches!(
        ext.to_lowercase().as_str(),
        "arw" | "cr2" | "cr3" | "nef" | "nrw" | "orf" | "raf" | "rw2" | "dng"
    )
}

/// Returns the EXIF orientation tag (1-8) if present.
pub fn get_orientation(path: &Path) -> Option<u32> {
    let Ok(file) = File::open(path) else { return None };
    let reader = BufReader::new(file);
    let img_reader = image::ImageReader::new(reader).with_guessed_format();
    
    if let Ok(reader) = img_reader {
        match reader.format() {
            Some(image::ImageFormat::Jpeg) | Some(image::ImageFormat::Tiff) | Some(image::ImageFormat::WebP) => {
                let file = File::open(path).ok()?;
                let mut reader = BufReader::new(file);
                let exifreader = exif::Reader::new();
                let exif = exifreader.read_from_container(&mut reader).ok()?;
                return exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?
                    .value.get_uint(0);
            }
            _ => {}
        }
    }

    // Fallback: Deep scan for RAW files (like .CR3) where TIFF headers are buried.
    if let Ok(offset) = find_tiff_header(path) {
        let mut file = File::open(path).ok()?;
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut reader = BufReader::new(file);
        let exifreader = exif::Reader::new();
        let exif = exifreader.read_from_container(&mut reader).ok()?;
        return exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?
            .value.get_uint(0);
    }

    None
}

/// Scans the first 1MB of a file for TIFF magic bytes and returns the offset.
fn find_tiff_header(path: &Path) -> Result<u64, ()> {
    let mut file = File::open(path).map_err(|_| ())?;
    let mut buffer = vec![0u8; 1024 * 1024]; // Metadata can be deep in RAW files
    let bytes_read = file.read(&mut buffer).map_err(|_| ())?;
    let data = &buffer[..bytes_read];

    let headers = [
        [0x49, 0x49, 0x2A, 0x00], // Little-Endian TIFF
        [0x4D, 0x4D, 0x00, 0x2A], // Big-Endian TIFF
    ];

    for header in headers {
        if let Some(pos) = data.windows(4).position(|w| w == header) {
            return Ok(pos as u64);
        }
    }

    Err(())
}

/// Returns the EXIF orientation tag from a byte buffer.
pub fn get_orientation_from_bytes(data: &[u8]) -> Option<u32> {
    let mut reader = std::io::Cursor::new(data);
    let exifreader = exif::Reader::new();
    let exif = exifreader.read_from_container(&mut reader).ok()?;
    exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?
        .value.get_uint(0)
}

// ---------------------------------------------------------------------------
// PNG — tEXt and iTXt chunks
// ---------------------------------------------------------------------------

fn read_png(path: &Path) -> Vec<MetaEntry> {
    let Ok(file) = File::open(path) else { return vec![] };
    let decoder = png::Decoder::new(file);
    let Ok(reader) = decoder.read_info() else { return vec![] };
    let info = reader.info();
    let mut entries = Vec::new();

    for chunk in &info.uncompressed_latin1_text {
        entries.push(MetaEntry {
            key:   chunk.keyword.clone(),
            value: chunk.text.clone(),
        });
    }

    for chunk in &info.utf8_text {
        let value = chunk.get_text().unwrap_or_default();
        if !value.is_empty() {
            entries.push(MetaEntry {
                key:   chunk.keyword.clone(),
                value,
            });
        }
    }

    entries
}

// ---------------------------------------------------------------------------
// JPEG/TIFF/WebP/RAW — EXIF
// ---------------------------------------------------------------------------

fn read_exif(path: &Path) -> Vec<MetaEntry> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };

    // Use our tiff header finder to support RAW files
    let offset = find_tiff_header(path).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(offset));

    let mut reader = BufReader::new(file);
    let exifreader = exif::Reader::new();
    let exif = match exifreader.read_from_container(&mut reader) {
        Ok(e) => e,
        Err(_) => return vec![],
    };
    
    let mut entries = Vec::new();
    for field in exif.fields() {
        entries.push(MetaEntry {
            key:   field.tag.to_string(),
            value: field.display_value().with_unit(&exif).to_string(),
        });
    }
    entries
}
