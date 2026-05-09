use anyhow::Result;
use clap::Parser;
use std::sync::mpsc;
use winit::{event_loop::ControlFlow, event_loop::EventLoop};

mod app;
mod cli;
mod constants;
mod gpu;
mod icon;
mod pdf;
mod platform;
mod presenter;
mod render;

use app::App;
use cli::Args;
use platform::install_macos_swipe_monitor;

fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();
    let event_loop = EventLoop::new()?;
    let proxy = event_loop.create_proxy();
    let (mac_nav_tx, mac_nav_rx) = mpsc::channel();
    let _mac_swipe_monitor = install_macos_swipe_monitor(mac_nav_tx, proxy.clone());
    let app = App::new(args, proxy, mac_nav_rx)?;
    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop.run_app(app)?;
    Ok(())
}
