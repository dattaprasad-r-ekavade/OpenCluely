//! Screen capture. The answer window is created with `contentProtected`,
//! which on Windows sets WDA_EXCLUDEFROMCAPTURE, so it is never part of
//! the screenshot that goes to the AI (and never visible to screen share).

use std::io::Cursor;

use anyhow::{Context, Result};
use image::{codecs::jpeg::JpegEncoder, imageops::FilterType, DynamicImage};
use xcap::Monitor;

pub struct Screenshot {
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl Screenshot {
    pub const MIME: &'static str = "image/jpeg";
}

pub struct CaptureOptions {
    /// Capture the monitor that contains this physical point.
    /// `None` = the primary monitor.
    pub point: Option<(i32, i32)>,
    /// Longest image edge in px. 0 = full resolution. A smaller image is a
    /// faster upload and fewer tokens; a larger one keeps small text readable.
    pub max_edge: u32,
    pub jpeg_quality: u8,
}

/// Capture one monitor and encode it as JPEG.
pub fn capture(opts: &CaptureOptions) -> Result<Screenshot> {
    let monitor = match opts.point {
        Some((x, y)) => Monitor::from_point(x, y).or_else(|_| primary_monitor())?,
        None => primary_monitor()?,
    };
    let rgba = monitor.capture_image().context("screen capture failed")?;

    let mut img = DynamicImage::ImageRgba8(rgba);
    let (w, h) = (img.width(), img.height());
    let max_edge = opts.max_edge;
    if max_edge > 0 && w.max(h) > max_edge {
        let scale = max_edge as f32 / w.max(h) as f32;
        let (nw, nh) = ((w as f32 * scale) as u32, (h as f32 * scale) as u32);
        img = img.resize_exact(nw, nh, FilterType::Triangle);
    }
    let rgb = img.to_rgb8();

    let mut jpeg = Vec::with_capacity(512 * 1024);
    JpegEncoder::new_with_quality(Cursor::new(&mut jpeg), opts.jpeg_quality)
        .encode_image(&rgb)
        .context("JPEG encode failed")?;

    Ok(Screenshot { width: rgb.width(), height: rgb.height(), jpeg })
}

fn primary_monitor() -> Result<Monitor> {
    let all = Monitor::all().context("no monitors found")?;
    let primary = all.iter().position(|m| m.is_primary().unwrap_or(false)).unwrap_or(0);
    all.into_iter().nth(primary).context("no monitors found")
}
