use std::time::Duration;

pub(crate) const LASER_POINTS: usize = 64;
pub(crate) const FLAG_LASER: u32 = 1;
pub(crate) const FLAG_HIGHLIGHT: u32 = 2;
pub(crate) const FLAG_MAGNIFY: u32 = 4;
pub(crate) const MIN_ZOOM: f64 = 0.25;
pub(crate) const MAX_ZOOM: f64 = 8.0;
pub(crate) const ZOOM_EPSILON: f64 = 0.001;
pub(crate) const WHEEL_ZOOM_STEP: f64 = 1.15;
pub(crate) const PIXEL_ZOOM_SPEED: f64 = 0.0025;
pub(crate) const ZOOM_RENDER_DEBOUNCE: Duration = Duration::from_millis(300);
pub(crate) const SOURCE_RECT_SCALE: f64 = 1_000_000.0;
