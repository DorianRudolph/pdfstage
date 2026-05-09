use winit::icon::{Icon, RgbaIcon};

#[cfg(target_os = "macos")]
use objc2::{AnyThread, MainThreadMarker, rc::Retained};
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSApplication, NSImage};
#[cfg(target_os = "macos")]
use objc2_foundation::NSData;

const APP_ICON_SIZE: u32 = 256;
const APP_ICON_RGBA: &[u8] = include_bytes!("../assets/app-icon.rgba");
#[cfg(target_os = "macos")]
const APP_ICON_PNG: &[u8] = include_bytes!("../assets/app-icon.png");
#[cfg(target_os = "macos")]
const APP_ICON_SVG: &[u8] = include_bytes!("../assets/app-icon.svg");

pub(crate) fn app_window_icon() -> Option<Icon> {
    let expected_len = APP_ICON_SIZE as usize * APP_ICON_SIZE as usize * 4;
    if APP_ICON_RGBA.len() != expected_len {
        eprintln!(
            "invalid app icon RGBA data: expected {expected_len} bytes, got {}",
            APP_ICON_RGBA.len()
        );
        return None;
    }
    RgbaIcon::new(APP_ICON_RGBA.to_vec(), APP_ICON_SIZE, APP_ICON_SIZE)
        .map(Icon::from)
        .map_err(|err| eprintln!("invalid app icon RGBA data: {err}"))
        .ok()
}

#[cfg(target_os = "macos")]
pub(crate) fn set_macos_app_icon() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // Recent macOS releases can initialize NSImage from SVG data. Keep PNG as
    // a fallback for older systems or stricter image loaders.
    let Some(image) =
        ns_image_from_bytes(APP_ICON_SVG).or_else(|| ns_image_from_bytes(APP_ICON_PNG))
    else {
        eprintln!("failed to load app icon with NSImage");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    unsafe {
        app.setApplicationIconImage(Some(&image));
    }
}

#[cfg(target_os = "macos")]
fn ns_image_from_bytes(bytes: &[u8]) -> Option<Retained<NSImage>> {
    let data = NSData::with_bytes(bytes);
    NSImage::initWithData(NSImage::alloc(), &data)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn set_macos_app_icon() {}
