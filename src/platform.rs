use std::sync::mpsc::Sender;

use winit::{event_loop::EventLoopProxy, window::Window};

#[cfg(target_os = "macos")]
use block2::RcBlock;
#[cfg(target_os = "macos")]
use objc2::{rc::Retained, runtime::AnyObject};
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSEvent, NSEventMask, NSEventType};
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
    tx: Sender<i32>,
    proxy: EventLoopProxy,
) -> Option<MacSwipeMonitor> {
    let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        let ns_event = unsafe { event.as_ref() };
        if ns_event.r#type() == NSEventType::Swipe {
            if let Some(delta) =
                swipe_navigation_delta(ns_event.deltaX() as f64, ns_event.deltaY() as f64)
            {
                let _ = tx.send(delta);
                proxy.wake_up();
            }
        }
        event.as_ptr()
    });

    let monitor = unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::Swipe, &block)
    };
    monitor.map(|monitor| MacSwipeMonitor {
        monitor,
        _block: block,
    })
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn install_macos_swipe_monitor(
    _tx: Sender<i32>,
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
