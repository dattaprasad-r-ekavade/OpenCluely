//! Screen capture. The answer window is created with `contentProtected`,
//! which on Windows sets WDA_EXCLUDEFROMCAPTURE, so it is never part of
//! the screenshot that goes to the AI (and never visible to screen share).

use std::io::Cursor;

use anyhow::{Context, Result};
use image::{codecs::jpeg::JpegEncoder, imageops::FilterType, DynamicImage};
use xcap::Monitor;

/// Longest edge sent to the AI. Large enough to read code and small text,
/// small enough to keep the upload and the token cost low.
const MAX_EDGE: u32 = 2048;
const JPEG_QUALITY: u8 = 85;

pub struct Screenshot {
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl Screenshot {
    pub const MIME: &'static str = "image/jpeg";
}

/// Capture the monitor that contains the physical point (x, y).
/// Falls back to the primary monitor.
pub fn capture_at(x: i32, y: i32) -> Result<Screenshot> {
    let monitor = match Monitor::from_point(x, y) {
        Ok(m) => m,
        Err(_) => primary_monitor()?,
    };
    let rgba = monitor.capture_image().context("screen capture failed")?;

    let mut img = DynamicImage::ImageRgba8(rgba);
    let (w, h) = (img.width(), img.height());
    if w.max(h) > MAX_EDGE {
        let scale = MAX_EDGE as f32 / w.max(h) as f32;
        let (nw, nh) = ((w as f32 * scale) as u32, (h as f32 * scale) as u32);
        img = img.resize_exact(nw, nh, FilterType::Triangle);
    }
    let rgb = img.to_rgb8();

    let mut jpeg = Vec::with_capacity(512 * 1024);
    JpegEncoder::new_with_quality(Cursor::new(&mut jpeg), JPEG_QUALITY)
        .encode_image(&rgb)
        .context("JPEG encode failed")?;

    Ok(Screenshot { width: rgb.width(), height: rgb.height(), jpeg })
}

fn primary_monitor() -> Result<Monitor> {
    let all = Monitor::all().context("no monitors found")?;
    let primary = all.iter().position(|m| m.is_primary().unwrap_or(false)).unwrap_or(0);
    all.into_iter().nth(primary).context("no monitors found")
}
