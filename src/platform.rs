use std::sync::mpsc::Sender;

use winit::{event_loop::EventLoopProxy, window::Window};

pub(crate) struct Navigation {
    pub(crate) delta: i32,
    pub(crate) history: bool,
}

pub(crate) fn shift_key_pressed(reported: bool) -> bool {
    #[cfg(target_os = "macos")]
    {
        // Mouse drivers can synthesize navigation events without copying held modifiers.
        reported || NSEvent::modifierFlags_class().contains(NSEventModifierFlags::Shift)
    }
    #[cfg(not(target_os = "macos"))]
    {
        reported
    }
}

pub(crate) fn open_uri(uri: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "windows")]
    let program = "explorer.exe";
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let program = "xdg-open";

    let mut child = std::process::Command::new(program).arg(uri).spawn()?;
    std::thread::spawn(move || match child.wait() {
        Ok(status) if status.success() => {}
        result => eprintln!("opening PDF link failed: {result:?}"),
    });
    Ok(())
}

#[cfg(target_os = "macos")]
use block2::RcBlock;
#[cfg(target_os = "macos")]
use objc2::{rc::Retained, runtime::AnyObject};
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags, NSEventType};
#[cfg(target_os = "macos")]
use std::ptr::NonNull;

#[cfg(target_os = "macos")]
use crate::pdf::swipe_navigation_delta;

#[cfg(target_os = "macos")]
pub(crate) struct MacSwipeMonitor {
    monitor: Retained<AnyObject>,
    _block: RcBlock<dyn Fn(NonNull<NSEvent>) -> *mut NSEvent>,
}

#[cfg(target_os = "macos")]
impl Drop for MacSwipeMonitor {
    fn drop(&mut self) {
        unsafe {
            NSEvent::removeMonitor(&self.monitor);
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) struct MacSwipeMonitor;

#[cfg(target_os = "macos")]
pub(crate) fn install_macos_swipe_monitor(
    tx: Sender<Navigation>,
    proxy: EventLoopProxy,
) -> Option<MacSwipeMonitor> {
    let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        let ns_event = unsafe { event.as_ref() };
        if ns_event.r#type() == NSEventType::Swipe {
            if let Some(delta) =
                swipe_navigation_delta(ns_event.deltaX() as f64, ns_event.deltaY() as f64)
            {
                let reported_shift = ns_event.modifierFlags().contains(NSEventModifierFlags::Shift);
                let history = shift_key_pressed(reported_shift);
                log::debug!(target: "pdfstage::input", "macOS swipe delta={delta} event_shift={reported_shift} effective_shift={history}");
                let _ = tx.send(Navigation { delta, history });
                proxy.wake_up();
            }
        }
        event.as_ptr()
    });

    let monitor = unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::Swipe, &block)
    };
    monitor.map(|monitor| MacSwipeMonitor { monitor, _block: block })
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn install_macos_swipe_monitor(
    _tx: Sender<Navigation>,
    _proxy: EventLoopProxy,
) -> Option<MacSwipeMonitor> {
    None
}

#[cfg(target_os = "macos")]
pub(crate) fn set_window_decorations(window: &dyn Window, decorated: bool) {
    window.set_decorations(decorated);
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn set_window_decorations(window: &dyn Window, decorated: bool) {
    window.set_decorations(decorated);
}
