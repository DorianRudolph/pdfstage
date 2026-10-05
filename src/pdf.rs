use std::{fs, path::PathBuf, sync::Arc, time::SystemTime};

use anyhow::{Context, Result};
use mupdf::Document;
use winit::dpi::PhysicalSize;

const ASPECT_CORRECTION_PIXEL_TOLERANCE: u32 = 1;

#[derive(Clone)]
pub(crate) struct PdfSource {
    pub(crate) bytes: Arc<Vec<u8>>,
    pub(crate) generation: u64,
    pub(crate) modified: Option<SystemTime>,
}

impl PdfSource {
    pub(crate) fn load(path: &PathBuf, generation: u64) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let modified = fs::metadata(path).ok().and_then(|m| m.modified().ok());
        Ok(Self { bytes: Arc::new(bytes), generation, modified })
    }

    pub(crate) fn info(&self) -> Result<PdfInfo> {
        let document = Document::from_bytes(&self.bytes, "pdf")?;
        let page_count = document.page_count()?.max(0) as usize;
        let page_points = if page_count == 0 {
            [16.0, 9.0]
        } else {
            let page = document.load_page(0)?;
            let bounds = page.bounds()?;
            [bounds.width().max(1.0), bounds.height().max(1.0)]
        };
        Ok(PdfInfo { page_count, page_points })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PdfInfo {
    pub(crate) page_count: usize,
    pub(crate) page_points: [f32; 2],
}

pub(crate) fn window_size_for_page(page_points: [f32; 2]) -> PhysicalSize<u32> {
    let aspect = (page_points[0] / page_points[1]).clamp(0.25, 4.0);
    let max_width = 1280.0;
    let max_height = 900.0;
    let (width, height) = if max_width / aspect <= max_height {
        (max_width, max_width / aspect)
    } else {
        (max_height * aspect, max_height)
    };
    PhysicalSize::new(width.round() as u32, height.round() as u32)
}

pub(crate) fn aspect_corrected_size(
    size: PhysicalSize<u32>,
    page_points: [f32; 2],
) -> Option<PhysicalSize<u32>> {
    if size.width == 0 || size.height == 0 {
        return None;
    }

    let aspect = (page_points[0] / page_points[1]).clamp(0.25, 4.0);
    let height_from_width = ((size.width as f32 / aspect).round() as u32).max(1);
    let width_from_height = ((size.height as f32 * aspect).round() as u32).max(1);
    let keep_width = PhysicalSize::new(size.width, height_from_width);
    let keep_height = PhysicalSize::new(width_from_height, size.height);

    let keep_width_delta = keep_width.height.abs_diff(size.height);
    let keep_height_delta = keep_height.width.abs_diff(size.width);
    let corrected = if keep_width_delta <= keep_height_delta { keep_width } else { keep_height };
    let corrected_delta = keep_width_delta.min(keep_height_delta);

    // Some window systems round requested surface sizes to nearby physical pixels.
    // Chasing a one-pixel correction can feed back into another resize event.
    (size != corrected && corrected_delta > ASPECT_CORRECTION_PIXEL_TOLERANCE).then_some(corrected)
}

#[cfg(target_os = "macos")]
pub(crate) fn swipe_navigation_delta(x: f64, y: f64) -> Option<i32> {
    if x.abs() >= y.abs() && x.abs() >= 0.1 {
        Some(if x > 0.0 { -1 } else { 1 })
    } else if y.abs() >= 0.1 {
        Some(if y < 0.0 { 1 } else { -1 })
    } else {
        None
    }
}

pub(crate) fn scroll_navigation_delta(x: f64, y: f64) -> Option<i32> {
    if x.abs() > y.abs() && x.abs() > 0.0 {
        Some(if x < 0.0 { 1 } else { -1 })
    } else if y.abs() > 0.0 {
        Some(if y < 0.0 { 1 } else { -1 })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_pdf_with_unembedded_standard_font() {
        let document =
            Document::from_bytes(include_bytes!("../tests/fixtures/base14.pdf"), "pdf").unwrap();
        assert_eq!(document.page_count().unwrap(), 1);
        let page = document.load_page(0).unwrap();
        let pixmap = page
            .to_pixmap(&mupdf::Matrix::IDENTITY, &mupdf::Colorspace::device_rgb(), false, true)
            .unwrap();
        assert_eq!((pixmap.width(), pixmap.height()), (200, 100));
        assert!(pixmap.samples().iter().any(|&sample| sample < 128), "text must be visible");
    }

    #[test]
    fn aspect_corrected_size_accepts_single_pixel_rounding() {
        assert_eq!(aspect_corrected_size(PhysicalSize::new(2024, 1140), [16.0, 9.0]), None);
    }

    #[test]
    fn aspect_corrected_size_corrects_larger_offsets() {
        assert_eq!(
            aspect_corrected_size(PhysicalSize::new(2024, 1200), [16.0, 9.0]),
            Some(PhysicalSize::new(2024, 1139))
        );
    }
}
