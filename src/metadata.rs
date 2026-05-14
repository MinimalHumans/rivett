//! Image metadata extraction.
//!
//! Currently reads PNG `tEXt` and `iTXt` chunks, which is where ComfyUI,
//! Automatic1111, and InvokeAI embed their workflow/prompt data.
//! EXIF (JPEG/TIFF/WebP/RAW) support is implemented via the `exif` crate.

use std::path::Path;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::collections::HashMap;
use exif::{Tag, In, Value};

/// A single key/value metadata entry.
#[derive(Debug, Clone)]
pub struct MetaEntry {
    pub key:   String,
    /// Raw value string, potentially very long (ComfyUI JSON can be MBs).
    pub value: String,
    /// If true, this entry acts as a category header.
    pub is_header: bool,
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
    inject_ai_prompts(&mut entries);

    entries
}

fn post_process_metadata(entries: &mut Vec<MetaEntry>) {
    // For now, only handle JSON pretty-printing for entries that didn't go through the EXIF refactor.
    // EXIF entries are now pre-processed in read_exif.
    for entry in entries.iter_mut() {
        if !entry.is_header && (entry.value.trim().starts_with('{') || entry.value.trim().starts_with('[')) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&entry.value) {
                if let Ok(pretty) = serde_json::to_string_pretty(&val) {
                    entry.value = pretty;
                }
            }
        }
    }
}

/// Scan entries for known AI-generation metadata patterns and prepend
/// top-level "Prompt" / "Negative Prompt" entries so they appear first
/// in the panel.  The original entries are left in place so no detail
/// is lost.
fn inject_ai_prompts(entries: &mut Vec<MetaEntry>) {
    let Some((positive, negative)) = extract_ai_prompt(entries) else { return };

    let mut inserts: Vec<MetaEntry> = Vec::new();
    inserts.push(MetaEntry { key: "Prompt".to_string(),          value: positive, is_header: false });
    if let Some(neg) = negative {
        inserts.push(MetaEntry { key: "Negative Prompt".to_string(), value: neg,      is_header: false });
    }
    // Prepend so they appear before all other metadata.
    for (i, entry) in inserts.into_iter().enumerate() {
        entries.insert(i, entry);
    }
}

/// Try to find a human-readable prompt in the metadata entries.
/// Returns `(positive, Option<negative>)` on success.
fn extract_ai_prompt(entries: &[MetaEntry]) -> Option<(String, Option<String>)> {
    // A1111 / AUTOMATIC1111: plain-text "parameters" chunk.
    if let Some(e) = entries.iter().find(|e| e.key == "parameters") {
        if let Some(result) = extract_a1111_prompt(&e.value) {
            return Some(result);
        }
    }

    // ComfyUI: "prompt" chunk that contains a JSON node-graph.
    if let Some(e) = entries.iter().find(|e| e.key == "prompt") {
        if e.value.trim().starts_with('{') {
            if let Some(result) = extract_comfyui_prompt(&e.value) {
                return Some(result);
            }
        }
    }

    // MidJourney / generic: a "Description" entry that was surfaced from
    // the ImageDescription EXIF tag — just reuse it directly.
    if let Some(e) = entries.iter().find(|e| e.key == "Description") {
        if !e.value.is_empty() {
            return Some((e.value.clone(), None));
        }
    }

    None
}

/// Extract positive (and optional negative) prompt from an A1111-style
/// `parameters` text block.
fn extract_a1111_prompt(params: &str) -> Option<(String, Option<String>)> {
    let neg_marker = "Negative prompt:";

    if let Some(neg_pos) = params.find(neg_marker) {
        let positive = params[..neg_pos].trim().to_string();
        let after_neg = &params[neg_pos + neg_marker.len()..];
        // Negative prompt ends at the first settings line ("Steps:", "Model:", etc.)
        let neg_end = after_neg
            .lines()
            .position(|l| {
                let t = l.trim_start();
                t.starts_with("Steps:") || t.starts_with("Model:") || t.starts_with("Sampler:")
            })
            .map(|i| after_neg.lines().take(i).map(|l| l.len() + 1).sum::<usize>())
            .unwrap_or(after_neg.len());
        let negative = after_neg[..neg_end].trim().to_string();
        if positive.is_empty() { return None; }
        Some((positive, if negative.is_empty() { None } else { Some(negative) }))
    } else {
        // No negative prompt — find where settings begin and take everything before.
        let settings_pos = params
            .lines()
            .position(|l| {
                let t = l.trim_start();
                t.starts_with("Steps:") || t.starts_with("Model:") || t.starts_with("Sampler:")
            })
            .map(|i| params.lines().take(i).map(|l| l.len() + 1).sum::<usize>())
            .unwrap_or(params.len());
        let positive = params[..settings_pos].trim().to_string();
        if positive.is_empty() { None } else { Some((positive, None)) }
    }
}

/// Extract positive (and optional negative) prompt from a ComfyUI
/// `prompt` JSON node-graph.  Looks for `CLIPTextEncode` nodes and
/// uses the `_meta.title` hint to distinguish positive from negative.
fn extract_comfyui_prompt(json_str: &str) -> Option<(String, Option<String>)> {
    let val: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let obj = val.as_object()?;

    let mut positives: Vec<String> = Vec::new();
    let mut negatives: Vec<String> = Vec::new();

    for (_id, node) in obj {
        if node.get("class_type").and_then(|v| v.as_str()) != Some("CLIPTextEncode") {
            continue;
        }
        let text = node
            .get("inputs")
            .and_then(|i| i.get("text"))
            .and_then(|t| t.as_str())
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        if text.is_empty() { continue; }

        let title = node
            .get("_meta")
            .and_then(|m| m.get("title"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_lowercase();

        if title.contains("negative") {
            negatives.push(text);
        } else {
            positives.push(text);
        }
    }

    if positives.is_empty() { return None; }

    let positive = positives.join("\n\n");
    let negative = if negatives.is_empty() { None } else { Some(negatives.join("\n\n")) };
    Some((positive, negative))
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
            is_header: false,
        });
    }

    for chunk in &info.utf8_text {
        let value = chunk.get_text().unwrap_or_default();
        if !value.is_empty() {
            entries.push(MetaEntry {
                key:   chunk.keyword.clone(),
                value,
                is_header: false,
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

    let offset = find_tiff_header(path).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(offset));

    let mut reader = BufReader::new(file);
    let exifreader = exif::Reader::new();
    let exif = match exifreader.read_from_container(&mut reader) {
        Ok(e) => e,
        Err(_) => return vec![],
    };
    
    let mut map: HashMap<Tag, &exif::Field> = HashMap::new();
    
    // 1. IFD Filtering: Target IFD0 (PRIMARY) specifically.
    for field in exif.fields() {
        if field.ifd_num == In::PRIMARY {
             map.entry(field.tag).or_insert(field);
        }
    }

    let mut main_entries = Vec::new();

    // Noise Blocklist
    let blocklist = [
        Tag::ComponentsConfiguration,
        Tag::YCbCrPositioning,
        Tag::ExifVersion,
        Tag::FlashpixVersion,
        Tag::InteroperabilityIndex,
        Tag::ImageWidth,
        Tag::ImageLength,
        Tag::PixelXDimension,
        Tag::PixelYDimension,
    ];

    // Curated Main Metadata

    // Description / prompt (ImageDescription = tag 0x010E).
    // MidJourney stores the full prompt here; other tools use it as a caption.
    // Surfacing it early means inject_ai_prompts() can pick it up as "Description"
    // and promote it to a top-level "Prompt" entry automatically.
    if let Some(f) = map.get(&Tag::ImageDescription) {
        let val = f.display_value().to_string();
        if !val.is_empty() {
            main_entries.push(MetaEntry { key: "Description".to_string(), value: val, is_header: false });
        }
    }

    // Identity
    let make = map.get(&Tag::Make).map(|f| f.display_value().to_string());
    let model = map.get(&Tag::Model).map(|f| f.display_value().to_string());
    if let Some(dev) = combine_make_model(make, model) {
        main_entries.push(MetaEntry { key: "Device".to_string(), value: dev, is_header: false });
    }

    let date_time = map.get(&Tag::DateTimeOriginal).map(|f| f.display_value().to_string());
    let offset = map.get(&Tag::OffsetTimeOriginal).map(|f| f.display_value().to_string());
    let subsec = map.get(&Tag::SubSecTimeOriginal).map(|f| f.display_value().to_string());
    if let Some(ts) = format_iso_timestamp(date_time, offset, subsec) {
        main_entries.push(MetaEntry { key: "Timestamp".to_string(), value: ts, is_header: false });
    }

    if let Some(f) = map.get(&Tag::Software) {
        main_entries.push(MetaEntry { key: "Software".to_string(), value: f.display_value().to_string(), is_header: false });
    }

    // Optics
    if let Some(f) = map.get(&Tag::FNumber) {
        main_entries.push(MetaEntry { key: "Aperture".to_string(), value: format!("f/{}", f.display_value()), is_header: false });
    }
    if let Some(f) = map.get(&Tag::ExposureTime) {
        main_entries.push(MetaEntry { key: "Exposure".to_string(), value: format_exposure_time(f), is_header: false });
    }
    if let Some(f) = map.get(&Tag::PhotographicSensitivity) {
        main_entries.push(MetaEntry { key: "ISO".to_string(), value: f.display_value().to_string(), is_header: false });
    }
    if let Some(f) = map.get(&Tag::FocalLength) {
        main_entries.push(MetaEntry { key: "Focal Length".to_string(), value: format!("{} mm", f.display_value()), is_header: false });
    }

    // GPS
    let lat = map.get(&Tag::GPSLatitude);
    let lat_ref = map.get(&Tag::GPSLatitudeRef);
    if let Some(val) = format_gps_decimal(lat, lat_ref) {
        main_entries.push(MetaEntry { key: "Latitude".to_string(), value: format!("{:.6}°", val), is_header: false });
    }

    let lon = map.get(&Tag::GPSLongitude);
    let lon_ref = map.get(&Tag::GPSLongitudeRef);
    if let Some(val) = format_gps_decimal(lon, lon_ref) {
        main_entries.push(MetaEntry { key: "Longitude".to_string(), value: format!("{:.6}°", val), is_header: false });
    }

    let alt = map.get(&Tag::GPSAltitude);
    let alt_ref = map.get(&Tag::GPSAltitudeRef);
    if let Some(val) = format_gps_altitude(alt, alt_ref) {
        main_entries.push(MetaEntry { key: "Altitude".to_string(), value: val, is_header: false });
    }

    if let Some(f) = map.get(&Tag::GPSImgDirection) {
        main_entries.push(MetaEntry { key: "Direction".to_string(), value: format!("{}°", f.display_value()), is_header: false });
    }

    // Technical
    if let Some(f) = map.get(&Tag::Orientation) {
        let val = match f.value.get_uint(0) {
            Some(1) => "0°".to_string(),
            Some(6) => "90° CW".to_string(),
            Some(3) => "180°".to_string(),
            Some(8) => "270° CW".to_string(),
            _ => f.display_value().to_string(),
        };
        main_entries.push(MetaEntry { key: "Orientation".to_string(), value: val, is_header: false });
    }

    let tech_tags = [
        (Tag::ColorSpace, "Color Space"),
        (Tag::MeteringMode, "Metering"),
        (Tag::Flash, "Flash"),
        (Tag::WhiteBalance, "White Balance"),
    ];

    for (tag, label) in tech_tags {
        if let Some(f) = map.get(&tag) {
            main_entries.push(MetaEntry { key: label.to_string(), value: f.display_value().to_string(), is_header: false });
        }
    }

    // Remaining non-blocklisted tags
    let handled_tags: Vec<Tag> = vec![
        Tag::ImageDescription,
        Tag::Make, Tag::Model, Tag::DateTimeOriginal, Tag::OffsetTimeOriginal, Tag::SubSecTimeOriginal,
        Tag::Software, Tag::FNumber, Tag::ExposureTime, Tag::PhotographicSensitivity, Tag::FocalLength,
        Tag::GPSLatitude, Tag::GPSLatitudeRef, Tag::GPSLongitude, Tag::GPSLongitudeRef,
        Tag::GPSAltitude, Tag::GPSAltitudeRef, Tag::GPSImgDirection, Tag::Orientation,
        Tag::ColorSpace, Tag::MeteringMode, Tag::Flash, Tag::WhiteBalance,
    ];

    let mut other_entries = Vec::new();
    for field in exif.fields() {
        if field.ifd_num == In::PRIMARY {
            if !handled_tags.contains(&field.tag) && !blocklist.contains(&field.tag) {
                other_entries.push(MetaEntry {
                    key: field.tag.to_string(),
                    value: field.display_value().with_unit(&exif).to_string(),
                    is_header: false,
                });
            }
        }
    }

    let mut final_entries = Vec::new();
    final_entries.extend(main_entries);

    if !other_entries.is_empty() {
        final_entries.push(MetaEntry { key: "Other Metadata".to_string(), value: String::new(), is_header: true });
        final_entries.extend(other_entries);
    }

    final_entries
}

fn combine_make_model(make: Option<String>, model: Option<String>) -> Option<String> {
    match (make, model) {
        (Some(mk), Some(md)) => {
            let mk_clean = mk.trim().to_string();
            let md_clean = md.trim().to_string();
            if md_clean.to_lowercase().contains(&mk_clean.to_lowercase()) {
                Some(md_clean)
            } else {
                Some(format!("{} {}", mk_clean, md_clean))
            }
        }
        (Some(mk), None) => Some(mk.trim().to_string()),
        (None, Some(md)) => Some(md.trim().to_string()),
        _ => None,
    }
}

fn format_iso_timestamp(dt: Option<String>, offset: Option<String>, subsec: Option<String>) -> Option<String> {
    let dt = dt?; 
    let parts: Vec<&str> = dt.split_whitespace().collect();
    if parts.len() != 2 { return Some(dt); }

    let date = parts[0].replace(':', "-");
    let time = parts[1];

    let mut result = format!("{}T{}", date, time);
    if let Some(ss) = subsec {
        result.push_str(&format!(".{}", ss.trim()));
    }
    if let Some(off) = offset {
        let off = off.trim();
        if off.starts_with('+') || off.starts_with('-') {
            result.push_str(off);
        } else {
            result.push_str(&format!("+{}", off));
        }
    }
    Some(result)
}

fn format_exposure_time(field: &exif::Field) -> String {
    if let Value::Rational(ref v) = field.value {
        if !v.is_empty() {
            let num = v[0].num;
            let den = v[0].denom;
            if num == 1 {
                return format!("1/{}", den);
            } else if num > den && den != 0 {
                return format!("{:.1}s", num as f32 / den as f32);
            }
        }
    }
    field.display_value().to_string()
}

fn format_gps_decimal(field: Option<&&exif::Field>, ref_field: Option<&&exif::Field>) -> Option<f64> {
    let field = field?;
    if let Value::Rational(ref v) = field.value {
        if v.len() >= 3 {
            let d = v[0].num as f64 / v[0].denom as f64;
            let m = v[1].num as f64 / v[1].denom as f64;
            let s = v[2].num as f64 / v[2].denom as f64;
            let mut decimal = d + (m / 60.0) + (s / 3600.0);
            
            if let Some(rf) = ref_field {
                let r = rf.display_value().to_string();
                if r.contains('S') || r.contains('W') {
                    decimal = -decimal;
                }
            }
            return Some(decimal);
        }
    }
    None
}

fn format_gps_altitude(field: Option<&&exif::Field>, ref_field: Option<&&exif::Field>) -> Option<String> {
    let field = field?;
    if let Value::Rational(ref v) = field.value {
        if !v.is_empty() {
            let mut alt = v[0].num as f64 / v[0].denom as f64;
            if let Some(rf) = ref_field {
                let r = rf.display_value().to_string();
                if r.contains('1') || r.to_lowercase().contains("below") {
                    alt = -alt;
                }
            }
            return Some(format!("{:.1} m", alt));
        }
    }
    None
}
