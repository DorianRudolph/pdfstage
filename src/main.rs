use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, bail};
use bytemuck::{Pod, Zeroable};
use clap::Parser;
use mupdf::{Colorspace, Device as MupdfDevice, Document, IRect, Matrix, Pixmap};
use wgpu::util::DeviceExt;
use winit::{
    application::ApplicationHandler,
    dpi::{PhysicalPosition, PhysicalSize},
    event::{ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    icon::{Icon, RgbaIcon},
    keyboard::{Key, ModifiersState, NamedKey},
    monitor::Fullscreen,
    window::{Window, WindowAttributes, WindowId},
};

#[cfg(target_os = "macos")]
use block2::RcBlock;
#[cfg(target_os = "macos")]
use objc2::{AnyThread, MainThreadMarker, rc::Retained, runtime::AnyObject};
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSApplication, NSEvent, NSEventMask, NSEventType, NSImage};
#[cfg(target_os = "macos")]
use objc2_foundation::NSData;
#[cfg(target_os = "macos")]
use std::ptr::NonNull;

const LASER_POINTS: usize = 64;
const FLAG_LASER: u32 = 1;
const FLAG_HIGHLIGHT: u32 = 2;
const FLAG_MAGNIFY: u32 = 4;
const APP_ICON_SIZE: u32 = 256;
const APP_ICON_RGBA: &[u8] = include_bytes!("../assets/app-icon.rgba");
const MIN_ZOOM: f64 = 0.25;
const MAX_ZOOM: f64 = 8.0;
const ZOOM_EPSILON: f64 = 0.001;
const WHEEL_ZOOM_STEP: f64 = 1.15;
const PIXEL_ZOOM_SPEED: f64 = 0.0025;
const ZOOM_RENDER_DEBOUNCE: Duration = Duration::from_millis(300);
const SOURCE_RECT_SCALE: f64 = 1_000_000.0;
#[cfg(target_os = "macos")]
const APP_ICON_PNG: &[u8] = include_bytes!("../assets/app-icon.png");
#[cfg(target_os = "macos")]
const APP_ICON_SVG: &[u8] = include_bytes!("../assets/app-icon.svg");

fn app_window_icon() -> Option<Icon> {
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
fn set_macos_app_icon() {
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
fn set_macos_app_icon() {}

#[derive(Parser, Debug)]
#[command(version, about = "Minimal MuPDF/WGPU PDF presentation viewer")]
struct Args {
    pdf: PathBuf,

    #[arg(long, help = "Open a second mirror window for screensharing")]
    mirror: bool,

    #[arg(long, help = "Poll the PDF and reload it when it changes")]
    hot_reload: bool,

    #[arg(
        long,
        default_value_t = 1024,
        help = "Per-window GPU page cache budget in MiB"
    )]
    cache_mib: u64,

    #[arg(
        long,
        default_value_t = 3,
        help = "Pages to render ahead of the current page"
    )]
    ahead: i32,
}

#[cfg(target_os = "macos")]
struct MacSwipeMonitor {
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
struct MacSwipeMonitor;

#[cfg(target_os = "macos")]
fn install_macos_swipe_monitor(tx: Sender<i32>, proxy: EventLoopProxy) -> Option<MacSwipeMonitor> {
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
fn install_macos_swipe_monitor(
    _tx: Sender<i32>,
    _proxy: EventLoopProxy,
) -> Option<MacSwipeMonitor> {
    None
}

#[derive(Clone)]
struct PdfSource {
    bytes: Arc<Vec<u8>>,
    generation: u64,
    modified: Option<SystemTime>,
}

impl PdfSource {
    fn load(path: &PathBuf, generation: u64) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let modified = fs::metadata(path).ok().and_then(|m| m.modified().ok());
        Ok(Self {
            bytes: Arc::new(bytes),
            generation,
            modified,
        })
    }

    fn info(&self) -> Result<PdfInfo> {
        let document = Document::from_bytes(&self.bytes, "pdf")?;
        let page_count = document.page_count()?.max(0) as usize;
        let page_points = if page_count == 0 {
            [16.0, 9.0]
        } else {
            let page = document.load_page(0)?;
            let bounds = page.bounds()?;
            [bounds.width().max(1.0), bounds.height().max(1.0)]
        };
        Ok(PdfInfo {
            page_count,
            page_points,
        })
    }
}

#[derive(Clone, Copy)]
struct PdfInfo {
    page_count: usize,
    page_points: [f32; 2],
}

fn window_size_for_page(page_points: [f32; 2]) -> PhysicalSize<u32> {
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

fn aspect_corrected_size(
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
    let corrected = if keep_width_delta <= keep_height_delta {
        keep_width
    } else {
        keep_height
    };

    (size != corrected).then_some(corrected)
}

fn swipe_navigation_delta(x: f64, y: f64) -> Option<i32> {
    if x.abs() >= y.abs() && x.abs() >= 0.1 {
        Some(if x > 0.0 { -1 } else { 1 })
    } else if y.abs() >= 0.1 {
        Some(if y < 0.0 { 1 } else { -1 })
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct RenderKey {
    generation: u64,
    page: usize,
    width: u32,
    height: u32,
    source: SourceRectKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct SourceRectKey {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

impl SourceRectKey {
    const FULL: Self = Self {
        x0: 0,
        y0: 0,
        x1: SOURCE_RECT_SCALE as u32,
        y1: SOURCE_RECT_SCALE as u32,
    };

    fn from_unit_rect(rect: [f64; 4]) -> Self {
        let x0 = rect[0].clamp(0.0, 1.0);
        let y0 = rect[1].clamp(0.0, 1.0);
        let x1 = rect[2].clamp(x0, 1.0);
        let y1 = rect[3].clamp(y0, 1.0);
        Self {
            x0: (x0 * SOURCE_RECT_SCALE).floor() as u32,
            y0: (y0 * SOURCE_RECT_SCALE).floor() as u32,
            x1: (x1 * SOURCE_RECT_SCALE).ceil() as u32,
            y1: (y1 * SOURCE_RECT_SCALE).ceil() as u32,
        }
    }

    fn as_unit_rect(self) -> [f32; 4] {
        [
            self.x0 as f32 / SOURCE_RECT_SCALE as f32,
            self.y0 as f32 / SOURCE_RECT_SCALE as f32,
            self.x1 as f32 / SOURCE_RECT_SCALE as f32,
            self.y1 as f32 / SOURCE_RECT_SCALE as f32,
        ]
    }

    fn is_full(self) -> bool {
        self == Self::FULL
    }
}

#[derive(Clone)]
struct RenderedPage {
    key: RenderKey,
    _texture: Arc<wgpu::Texture>,
    view: Arc<wgpu::TextureView>,
    bytes: u64,
    source_rect: [f32; 4],
    page_points: [f32; 2],
}

#[derive(Clone)]
struct RenderRequest {
    source: PdfSource,
    current_page: usize,
    page_count: usize,
    size: PhysicalSize<u32>,
    source_rect: SourceRectKey,
    direction: i32,
    ahead: i32,
    request_id: u64,
}

enum RenderMessage {
    Request(RenderRequest),
    Stop,
}

enum RenderResult {
    Ready {
        page: RenderedPage,
        request_id: u64,
        primary: bool,
    },
    PageSize {
        generation: u64,
        page_points: [f32; 2],
        page_count: usize,
    },
    Error(String),
}

struct RenderWorker {
    tx: Sender<RenderMessage>,
    rx: Receiver<RenderResult>,
}

impl RenderWorker {
    fn spawn(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        cache_limit: u64,
        proxy: EventLoopProxy,
    ) -> Self {
        let (request_tx, request_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        thread::Builder::new()
            .name("mupdf-render-worker".to_string())
            .spawn(move || {
                render_worker_loop(device, queue, cache_limit, request_rx, result_tx, proxy)
            })
            .expect("spawn render worker");
        Self {
            tx: request_tx,
            rx: result_rx,
        }
    }

    fn request(&self, request: RenderRequest) {
        let _ = self.tx.send(RenderMessage::Request(request));
    }
}

impl Drop for RenderWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(RenderMessage::Stop);
    }
}

struct TextureCache {
    map: HashMap<RenderKey, RenderedPage>,
    lru: VecDeque<RenderKey>,
    bytes: u64,
    limit: u64,
}

impl TextureCache {
    fn new(limit: u64) -> Self {
        Self {
            map: HashMap::new(),
            lru: VecDeque::new(),
            bytes: 0,
            limit,
        }
    }

    fn get(&mut self, key: &RenderKey) -> Option<RenderedPage> {
        let item = self.map.get(key).cloned();
        if item.is_some() {
            self.touch(*key);
        }
        item
    }

    fn insert(&mut self, page: RenderedPage) {
        if let Some(old) = self.map.remove(&page.key) {
            self.bytes = self.bytes.saturating_sub(old.bytes);
        }
        self.bytes = self.bytes.saturating_add(page.bytes);
        self.lru.push_back(page.key);
        self.map.insert(page.key, page);
        self.evict();
    }

    fn touch(&mut self, key: RenderKey) {
        self.lru.retain(|k| *k != key);
        self.lru.push_back(key);
    }

    fn evict(&mut self) {
        while self.bytes > self.limit {
            let Some(key) = self.lru.pop_front() else {
                break;
            };
            if let Some(old) = self.map.remove(&key) {
                self.bytes = self.bytes.saturating_sub(old.bytes);
            }
        }
    }
}

fn render_worker_loop(
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    cache_limit: u64,
    rx: Receiver<RenderMessage>,
    tx: Sender<RenderResult>,
    proxy: EventLoopProxy,
) {
    let mut cache = TextureCache::new(cache_limit);
    let mut latest: Option<RenderRequest> = None;
    let mut document_generation = u64::MAX;
    let mut document: Option<Document> = None;

    loop {
        let request = match latest.take() {
            Some(request) => request,
            None => match rx.recv() {
                Ok(RenderMessage::Request(request)) => request,
                Ok(RenderMessage::Stop) | Err(_) => return,
            },
        };

        if !drain_render_messages(&rx, &mut latest) {
            return;
        }
        if latest.is_some() {
            continue;
        }

        if document_generation != request.source.generation {
            document = match Document::from_bytes(&request.source.bytes, "pdf") {
                Ok(doc) => Some(doc),
                Err(err) => {
                    send_render_result(
                        &tx,
                        &proxy,
                        RenderResult::Error(format!("MuPDF open failed: {err}")),
                    );
                    None
                }
            };
            document_generation = request.source.generation;
            cache
                .map
                .retain(|key, _| key.generation == document_generation);
            cache
                .lru
                .retain(|key| key.generation == document_generation);
            cache.bytes = cache.map.values().map(|p| p.bytes).sum();
        }

        let Some(document) = document.as_ref() else {
            continue;
        };
        if request.page_count == 0 || request.size.width == 0 || request.size.height == 0 {
            continue;
        }

        if let Ok(page) = document.load_page(request.current_page as i32) {
            if let Ok(bounds) = page.bounds() {
                send_render_result(
                    &tx,
                    &proxy,
                    RenderResult::PageSize {
                        generation: request.source.generation,
                        page_points: [bounds.width().max(1.0), bounds.height().max(1.0)],
                        page_count: request.page_count,
                    },
                );
            }
        }

        let ahead = if request.source_rect.is_full() {
            request.ahead
        } else {
            0
        };
        for page_index in prefetch_order(
            request.current_page,
            request.page_count,
            request.direction,
            ahead,
        ) {
            if !drain_render_messages(&rx, &mut latest) {
                return;
            }
            if latest.is_some() {
                break;
            }

            let key = render_key_for_page(
                document,
                request.source.generation,
                page_index,
                request.size,
                request.source_rect,
            );
            let Ok(Some(key)) = key else {
                continue;
            };
            let primary = page_index == request.current_page;
            if let Some(page) = cache.get(&key) {
                send_render_result(
                    &tx,
                    &proxy,
                    RenderResult::Ready {
                        page,
                        request_id: request.request_id,
                        primary,
                    },
                );
                continue;
            }

            if !drain_render_messages(&rx, &mut latest) {
                return;
            }
            if latest.is_some() {
                break;
            }

            match render_page_to_texture(document, &device, &queue, key) {
                Ok(page) => {
                    let page = RenderedPage {
                        bytes: (key.width as u64) * (key.height as u64) * 4,
                        ..page
                    };
                    cache.insert(page.clone());
                    send_render_result(
                        &tx,
                        &proxy,
                        RenderResult::Ready {
                            page,
                            request_id: request.request_id,
                            primary,
                        },
                    );
                }
                Err(err) => {
                    send_render_result(
                        &tx,
                        &proxy,
                        RenderResult::Error(format!(
                            "render page {} failed: {err}",
                            page_index + 1
                        )),
                    );
                }
            }
        }
    }
}

fn send_render_result(tx: &Sender<RenderResult>, proxy: &EventLoopProxy, result: RenderResult) {
    if tx.send(result).is_ok() {
        proxy.wake_up();
    }
}

fn drain_render_messages(rx: &Receiver<RenderMessage>, latest: &mut Option<RenderRequest>) -> bool {
    loop {
        match rx.try_recv() {
            Ok(RenderMessage::Request(next)) => *latest = Some(next),
            Ok(RenderMessage::Stop) | Err(mpsc::TryRecvError::Disconnected) => return false,
            Err(mpsc::TryRecvError::Empty) => return true,
        }
    }
}

fn prefetch_order(current: usize, page_count: usize, direction: i32, ahead: i32) -> Vec<usize> {
    let mut pages = vec![current];
    let ahead = ahead.max(0) as usize;
    if direction < 0 {
        for step in 1..=ahead {
            if current >= step {
                pages.push(current - step);
            }
        }
        for step in 1..=ahead {
            let next = current + step;
            if next < page_count {
                pages.push(next);
            }
        }
    } else {
        for step in 1..=ahead {
            let next = current + step;
            if next < page_count {
                pages.push(next);
            }
        }
        for step in 1..=ahead {
            if current >= step {
                pages.push(current - step);
            }
        }
    }
    pages
}

fn render_key_for_page(
    document: &Document,
    generation: u64,
    page: usize,
    surface_size: PhysicalSize<u32>,
    source_rect: SourceRectKey,
) -> Result<Option<RenderKey>> {
    let loaded_page = document.load_page(page as i32)?;
    let bounds = loaded_page.bounds()?;
    let page_width = bounds.width().max(1.0);
    let page_height = bounds.height().max(1.0);
    let source = source_rect.as_unit_rect();
    let source_width = ((source[2] - source[0]) * page_width).max(1.0);
    let source_height = ((source[3] - source[1]) * page_height).max(1.0);
    let scale =
        (surface_size.width as f32 / source_width).min(surface_size.height as f32 / source_height);
    let width = (source_width * scale).ceil().max(1.0) as u32;
    let height = (source_height * scale).ceil().max(1.0) as u32;
    Ok(Some(RenderKey {
        generation,
        page,
        width,
        height,
        source: source_rect,
    }))
}

fn render_page_to_texture(
    document: &Document,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    key: RenderKey,
) -> Result<RenderedPage> {
    let page = document.load_page(key.page as i32)?;
    let bounds = page.bounds()?;
    let source = key.source.as_unit_rect();
    let source_x0 = bounds.x0 + source[0] * bounds.width().max(1.0);
    let source_y0 = bounds.y0 + source[1] * bounds.height().max(1.0);
    let source_width = ((source[2] - source[0]) * bounds.width().max(1.0)).max(1.0);
    let source_height = ((source[3] - source[1]) * bounds.height().max(1.0)).max(1.0);
    let scale = (key.width as f32 / source_width).min(key.height as f32 / source_height);
    let pixmap = if key.source.is_full() {
        let matrix = Matrix::new_scale(scale, scale);
        page.to_pixmap(&matrix, &Colorspace::device_rgb(), false, true)?
    } else {
        let mut pixmap = Pixmap::new_with_w_h(
            &Colorspace::device_rgb(),
            key.width as i32,
            key.height as i32,
            false,
        )?;
        pixmap.clear_with(255)?;
        let matrix = Matrix::new(
            scale,
            0.0,
            0.0,
            scale,
            -source_x0 * scale,
            -source_y0 * scale,
        );
        let clip = IRect::new(0, 0, key.width as i32, key.height as i32);
        let draw_device = MupdfDevice::from_pixmap_with_clip(&pixmap, clip)?;
        page.run(&draw_device, &matrix)?;
        pixmap
    };
    let width = pixmap.width();
    let height = pixmap.height();
    let n = pixmap.n() as usize;
    if n < 3 {
        bail!("unexpected MuPDF pixmap component count: {n}");
    }

    let mut rgba = vec![255_u8; (width * height * 4) as usize];
    for (src, dst) in pixmap
        .samples()
        .chunks_exact(n)
        .zip(rgba.chunks_exact_mut(4))
    {
        dst[0] = src[0];
        dst[1] = src[1];
        dst[2] = src[2];
        dst[3] = if n >= 4 { src[3] } else { 255 };
    }

    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("pdf-page-texture"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 4),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Ok(RenderedPage {
        key,
        _texture: Arc::new(texture),
        view: Arc::new(view),
        bytes: rgba.len() as u64,
        source_rect: key.source.as_unit_rect(),
        page_points: [bounds.width().max(1.0), bounds.height().max(1.0)],
    })
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    surface_image: [f32; 4],
    mouse_flags: [f32; 4],
    highlight: [f32; 4],
    zoom_pan: [f32; 4],
    source_rect: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct LaserPoint {
    point: [f32; 4],
}

struct Gpu {
    instance: wgpu::Instance,
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform_buffer: Arc<wgpu::Buffer>,
    laser_buffer: Arc<wgpu::Buffer>,
    placeholder: RenderedPage,
    surface_format: wgpu::TextureFormat,
    max_texture_dimension_2d: u32,
}

impl Gpu {
    fn new(window: &Box<dyn Window>) -> Result<(Self, wgpu::Surface<'static>)> {
        let instance = wgpu::Instance::default();
        let surface = unsafe {
            instance.create_surface_unsafe(
                wgpu::SurfaceTargetUnsafe::from_display_and_window(window, window)
                    .context("creating raw surface target")?,
            )
        }
        .context("creating WGPU surface")?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .context("requesting WGPU adapter")?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("pdfpresenter-device"),
            ..Default::default()
        }))
        .context("requesting WGPU device")?;
        let device = Arc::new(device);
        let queue = Arc::new(queue);
        let max_texture_dimension_2d = device.limits().max_texture_dimension_2d;
        let size = window.surface_size();
        let config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .context("surface has no default config")?;
        let surface_format = config.format;
        surface.configure(&device, &config);

        let uniform_buffer = Arc::new(device.create_buffer_init(
            &wgpu::util::BufferInitDescriptor {
                label: Some("uniform-buffer"),
                contents: bytemuck::bytes_of(&Uniforms::zeroed()),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            },
        ));
        let laser_buffer = Arc::new(
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("laser-buffer"),
                contents: bytemuck::cast_slice(&[LaserPoint::zeroed(); LASER_POINTS]),
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            }),
        );
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("pdf-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bind-group-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pdfpresenter-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("render-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let placeholder = make_placeholder_texture(&device, &queue);
        Ok((
            Self {
                instance,
                device,
                queue,
                pipeline,
                bind_group_layout,
                sampler,
                uniform_buffer,
                laser_buffer,
                placeholder,
                surface_format,
                max_texture_dimension_2d,
            },
            surface,
        ))
    }
}

fn make_placeholder_texture(device: &wgpu::Device, queue: &wgpu::Queue) -> RenderedPage {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("placeholder-texture"),
        size: wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        texture.as_image_copy(),
        &[18, 18, 18, 255],
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4),
            rows_per_image: Some(1),
        },
        wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    RenderedPage {
        key: RenderKey {
            generation: 0,
            page: 0,
            width: 1,
            height: 1,
            source: SourceRectKey::FULL,
        },
        _texture: Arc::new(texture),
        view: Arc::new(view),
        bytes: 4,
        source_rect: SourceRectKey::FULL.as_unit_rect(),
        page_points: [1.0, 1.0],
    }
}

#[derive(Clone, Copy)]
struct ResizeDrag {
    start_position: PhysicalPosition<f64>,
    start_size: PhysicalSize<u32>,
}

struct PresenterWindow {
    surface: wgpu::Surface<'static>,
    window: Box<dyn Window>,
    config: wgpu::SurfaceConfiguration,
    worker: RenderWorker,
    current: Option<RenderedPage>,
    full_page: Option<RenderedPage>,
    full_page_bind_group: Option<wgpu::BindGroup>,
    bind_group: wgpu::BindGroup,
    wanted_request_id: u64,
    displayed_request_id: u64,
    surface_size: PhysicalSize<u32>,
    fullscreen_transition_until: Option<Instant>,
    pending_aspect_size: Option<PhysicalSize<u32>>,
    mouse: PhysicalPosition<f64>,
    mouse_down: Option<MouseButton>,
    highlight_start: Option<PhysicalPosition<f64>>,
    laser: VecDeque<PhysicalPosition<f64>>,
    resize_drag: Option<ResizeDrag>,
    mirror: bool,
    decorated: bool,
    zoom: f64,
    pan: [f64; 2],
    pending_zoom_render_at: Option<Instant>,
}

impl PresenterWindow {
    fn new(
        event_loop: &dyn ActiveEventLoop,
        gpu: &Gpu,
        proxy: EventLoopProxy,
        title: &str,
        cache_limit: u64,
        initial_size: PhysicalSize<u32>,
    ) -> Result<Self> {
        let window = event_loop
            .create_window(
                WindowAttributes::default()
                    .with_title(title)
                    .with_visible(true)
                    .with_window_icon(app_window_icon())
                    .with_surface_size(initial_size),
            )
            .context("creating window")?;

        let surface = unsafe {
            gpu.instance.create_surface_unsafe(
                wgpu::SurfaceTargetUnsafe::from_display_and_window(&window, &window)
                    .context("creating raw mirror surface target")?,
            )
        }
        .context("creating mirror surface")?;
        let size = window.surface_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: gpu.surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&gpu.device, &config);
        let worker = RenderWorker::spawn(gpu.device.clone(), gpu.queue.clone(), cache_limit, proxy);
        let bind_group = create_bind_group(gpu, &gpu.placeholder.view);
        Ok(Self {
            surface,
            window,
            config,
            worker,
            current: None,
            full_page: None,
            full_page_bind_group: None,
            bind_group,
            wanted_request_id: 0,
            displayed_request_id: 0,
            surface_size: size,
            fullscreen_transition_until: None,
            pending_aspect_size: None,
            mouse: PhysicalPosition::new(0.0, 0.0),
            mouse_down: None,
            highlight_start: None,
            laser: VecDeque::new(),
            resize_drag: None,
            mirror: false,
            decorated: true,
            zoom: 1.0,
            pan: [0.0, 0.0],
            pending_zoom_render_at: None,
        })
    }

    fn id(&self) -> WindowId {
        self.window.id()
    }

    fn resize(&mut self, gpu: &Gpu, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.surface_size = size;
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&gpu.device, &self.config);
    }

    fn sync_surface_size(&mut self, gpu: &Gpu) -> bool {
        let size = self.window.surface_size();
        if size.width == 0 || size.height == 0 {
            return false;
        }
        if size == self.surface_size {
            return false;
        }
        self.pending_aspect_size = None;
        self.resize(gpu, size);
        true
    }

    fn page_image_size(&self, page_points: [f32; 2]) -> [f64; 2] {
        let (page_width, page_height) = self
            .current
            .as_ref()
            .map(|page| (page.page_points[0] as f64, page.page_points[1] as f64))
            .unwrap_or((page_points[0] as f64, page_points[1] as f64));
        let page_width = page_width.max(1.0);
        let page_height = page_height.max(1.0);
        let scale = (self.surface_size.width as f64 / page_width)
            .min(self.surface_size.height as f64 / page_height);
        [
            (page_width * scale).ceil().max(1.0),
            (page_height * scale).ceil().max(1.0),
        ]
    }

    fn draw_image_size(&self, page_points: [f32; 2]) -> [f64; 2] {
        let base = self.page_image_size(page_points);
        [base[0] * self.zoom, base[1] * self.zoom]
    }

    fn render_request_geometry(
        &self,
        max_texture_dimension_2d: u32,
        page_points: [f32; 2],
    ) -> (PhysicalSize<u32>, SourceRectKey) {
        let max_dimension = max_texture_dimension_2d.max(1);
        if self.zoom <= 1.0 + ZOOM_EPSILON {
            let image = self.draw_image_size(page_points);
            return (
                PhysicalSize::new(
                    (image[0].ceil().max(1.0) as u32).min(max_dimension),
                    (image[1].ceil().max(1.0) as u32).min(max_dimension),
                ),
                SourceRectKey::FULL,
            );
        }

        let rect = self.page_image_rect(page_points);
        let visible_x0 = rect[0].max(0.0);
        let visible_y0 = rect[1].max(0.0);
        let visible_x1 = (rect[0] + rect[2]).min(self.surface_size.width as f64);
        let visible_y1 = (rect[1] + rect[3]).min(self.surface_size.height as f64);
        if visible_x0 >= visible_x1 || visible_y0 >= visible_y1 {
            return (
                PhysicalSize::new(
                    self.surface_size.width.max(1).min(max_dimension),
                    self.surface_size.height.max(1).min(max_dimension),
                ),
                SourceRectKey::FULL,
            );
        }

        let source_rect = SourceRectKey::from_unit_rect([
            (visible_x0 - rect[0]) / rect[2],
            (visible_y0 - rect[1]) / rect[3],
            (visible_x1 - rect[0]) / rect[2],
            (visible_y1 - rect[1]) / rect[3],
        ]);
        (
            PhysicalSize::new(
                ((visible_x1 - visible_x0).ceil().max(1.0) as u32).min(max_dimension),
                ((visible_y1 - visible_y0).ceil().max(1.0) as u32).min(max_dimension),
            ),
            source_rect,
        )
    }

    fn is_zoomed(&self) -> bool {
        (self.zoom - 1.0).abs() > ZOOM_EPSILON || self.pan[0].abs() > 0.5 || self.pan[1].abs() > 0.5
    }

    fn page_image_rect(&self, page_points: [f32; 2]) -> [f64; 4] {
        let image = self.draw_image_size(page_points);
        [
            ((self.surface_size.width as f64 - image[0]) * 0.5 + self.pan[0]).floor(),
            ((self.surface_size.height as f64 - image[1]) * 0.5 + self.pan[1]).floor(),
            image[0],
            image[1],
        ]
    }

    fn page_unit_at(
        &self,
        position: PhysicalPosition<f64>,
        page_points: [f32; 2],
    ) -> PhysicalPosition<f64> {
        let rect = self.page_image_rect(page_points);
        PhysicalPosition::new(
            (position.x - rect[0]) / rect[2],
            (position.y - rect[1]) / rect[3],
        )
    }

    fn position_for_page_unit(
        &self,
        position: PhysicalPosition<f64>,
        page_points: [f32; 2],
    ) -> PhysicalPosition<f64> {
        let rect = self.page_image_rect(page_points);
        PhysicalPosition::new(
            rect[0] + position.x * rect[2],
            rect[1] + position.y * rect[3],
        )
    }

    fn clamp_pan(&mut self, page_points: [f32; 2]) {
        let image = self.draw_image_size(page_points);
        let max_x = ((image[0] - self.surface_size.width as f64) * 0.5).max(0.0);
        let max_y = ((image[1] - self.surface_size.height as f64) * 0.5).max(0.0);
        self.pan[0] = self.pan[0].clamp(-max_x, max_x);
        self.pan[1] = self.pan[1].clamp(-max_y, max_y);
    }

    fn zoom_about(
        &mut self,
        factor: f64,
        anchor: PhysicalPosition<f64>,
        page_points: [f32; 2],
    ) -> bool {
        if !factor.is_finite() || factor <= 0.0 {
            return false;
        }
        let old_zoom = self.zoom;
        let new_zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        if (new_zoom - old_zoom).abs() <= ZOOM_EPSILON {
            return false;
        }

        let old_image = self.draw_image_size(page_points);
        let old_origin = [
            (self.surface_size.width as f64 - old_image[0]) * 0.5 + self.pan[0],
            (self.surface_size.height as f64 - old_image[1]) * 0.5 + self.pan[1],
        ];
        let unit = [
            ((anchor.x - old_origin[0]) / old_image[0]).clamp(0.0, 1.0),
            ((anchor.y - old_origin[1]) / old_image[1]).clamp(0.0, 1.0),
        ];

        self.zoom = new_zoom;
        let new_image = self.draw_image_size(page_points);
        self.pan[0] = anchor.x
            - (self.surface_size.width as f64 - new_image[0]) * 0.5
            - unit[0] * new_image[0];
        self.pan[1] = anchor.y
            - (self.surface_size.height as f64 - new_image[1]) * 0.5
            - unit[1] * new_image[1];
        self.clamp_pan(page_points);
        self.window.request_redraw();
        true
    }

    fn pan_by(&mut self, delta: [f64; 2], page_points: [f32; 2]) -> bool {
        if delta[0].abs() < f64::EPSILON && delta[1].abs() < f64::EPSILON {
            return false;
        }
        let before = self.pan;
        self.pan[0] += delta[0];
        self.pan[1] += delta[1];
        self.clamp_pan(page_points);
        if self.pan != before {
            self.window.request_redraw();
            return true;
        }
        false
    }

    fn reset_zoom(&mut self, page_points: [f32; 2]) -> bool {
        let changed = self.is_zoomed();
        self.zoom = 1.0;
        self.pan = [0.0, 0.0];
        self.clamp_pan(page_points);
        if changed {
            self.window.request_redraw();
        }
        changed
    }

    fn reset_zoom_for_slide_change(&mut self) {
        self.zoom = 1.0;
        self.pan = [0.0, 0.0];
        self.pending_zoom_render_at = None;
        self.full_page = None;
        self.full_page_bind_group = None;
        self.window.request_redraw();
    }

    fn use_full_page_if_available(&mut self) {
        let Some(full_page) = self.full_page.clone() else {
            return;
        };
        let Some(full_bind_group) = self.full_page_bind_group.as_ref() else {
            return;
        };
        let same_page = self
            .current
            .as_ref()
            .map(|current| {
                current.key.generation == full_page.key.generation
                    && current.key.page == full_page.key.page
            })
            .unwrap_or(true);
        if same_page
            && !self
                .current
                .as_ref()
                .is_some_and(|page| page.key.source.is_full())
        {
            self.bind_group = full_bind_group.clone();
            self.current = Some(full_page);
            self.window.request_redraw();
        }
    }

    fn defer_zoom_render(&mut self) {
        self.pending_zoom_render_at = Some(Instant::now() + ZOOM_RENDER_DEBOUNCE);
    }

    fn finish_zoom_render(&mut self) {
        self.pending_zoom_render_at = Some(Instant::now());
    }

    fn set_page(&mut self, request: RenderRequest) {
        self.wanted_request_id = request.request_id;
        self.worker.request(request);
    }

    fn poll_worker(
        &mut self,
        gpu: &Gpu,
        generation: u64,
        current_page: usize,
        page_count: usize,
        direction: i32,
    ) {
        while let Ok(message) = self.worker.rx.try_recv() {
            match message {
                RenderResult::Ready {
                    page,
                    request_id,
                    primary,
                } => {
                    let requested_current_page = primary
                        && request_id > self.displayed_request_id
                        && request_id <= self.wanted_request_id
                        && self.is_page_between_displayed_and_current(
                            page.key.page,
                            current_page,
                            direction,
                        );
                    let latest_current_page =
                        request_id == self.wanted_request_id && page.key.page == current_page;
                    if page.key.generation == generation
                        && (requested_current_page || latest_current_page)
                    {
                        let bind_group = create_bind_group(gpu, &page.view);
                        if page.key.source.is_full() {
                            self.full_page = Some(page.clone());
                            self.full_page_bind_group = Some(bind_group.clone());
                        }
                        self.bind_group = bind_group;
                        self.current = Some(page);
                        self.displayed_request_id = request_id;
                        self.window.request_redraw();
                    }
                }
                RenderResult::PageSize {
                    generation: msg_generation,
                    page_points,
                    page_count: msg_page_count,
                } => {
                    if msg_generation == generation && msg_page_count == page_count {
                        let _ = page_points;
                    }
                }
                RenderResult::Error(err) => eprintln!("{err}"),
            }
        }
    }

    fn is_page_between_displayed_and_current(
        &self,
        page: usize,
        current_page: usize,
        direction: i32,
    ) -> bool {
        let Some(displayed_page) = self.current.as_ref().map(|displayed| displayed.key.page) else {
            return page == current_page;
        };
        if direction < 0 {
            page <= displayed_page && page >= current_page
        } else {
            page >= displayed_page && page <= current_page
        }
    }

    fn draw(&mut self, gpu: &Gpu, page_points: [f32; 2]) -> Result<()> {
        let image_size = self
            .current
            .as_ref()
            .map(|_| {
                let image = self.draw_image_size(page_points);
                [image[0] as f32, image[1] as f32]
            })
            .unwrap_or([1.0, 1.0]);
        let source_rect = self
            .current
            .as_ref()
            .map(|page| page.source_rect)
            .unwrap_or_else(|| SourceRectKey::FULL.as_unit_rect());
        let flags = self.flags();
        let highlight_end = [self.mouse.x as f32, self.mouse.y as f32];
        let highlight_start = self.highlight_start.unwrap_or(self.mouse);
        let uniforms = Uniforms {
            surface_image: [
                self.surface_size.width as f32,
                self.surface_size.height as f32,
                image_size[0],
                image_size[1],
            ],
            mouse_flags: [
                self.mouse.x as f32,
                self.mouse.y as f32,
                flags as f32,
                self.laser.len() as f32,
            ],
            highlight: [
                highlight_start.x as f32,
                highlight_start.y as f32,
                highlight_end[0],
                highlight_end[1],
            ],
            zoom_pan: [self.pan[0] as f32, self.pan[1] as f32, 0.0, 0.0],
            source_rect,
        };
        gpu.queue
            .write_buffer(&gpu.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

        let mut points = [LaserPoint::zeroed(); LASER_POINTS];
        for (dst, point) in points.iter_mut().zip(self.laser.iter()) {
            dst.point = [point.x as f32, point.y as f32, 0.0, 0.0];
        }
        gpu.queue
            .write_buffer(&gpu.laser_buffer, 0, bytemuck::cast_slice(&points));

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&gpu.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Validation => bail!("surface validation error"),
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("render-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("render-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&gpu.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.window.pre_present_notify();
        gpu.queue.submit(Some(encoder.finish()));
        frame.present();
        Ok(())
    }

    fn flags(&self) -> u32 {
        let mut flags = 0;
        match self.mouse_down {
            Some(MouseButton::Left) => flags |= FLAG_LASER,
            Some(MouseButton::Right) => flags |= FLAG_HIGHLIGHT,
            Some(MouseButton::Middle) if !self.is_zoomed() => flags |= FLAG_MAGNIFY,
            _ => {}
        }
        flags
    }

    fn pointer_moved(&mut self, position: PhysicalPosition<f64>, page_points: [f32; 2]) -> bool {
        let previous = self.mouse;
        self.mouse = position;
        let mut panned = false;
        if self.mouse_down == Some(MouseButton::Left) {
            self.laser.push_back(position);
            while self.laser.len() > LASER_POINTS {
                self.laser.pop_front();
            }
        }
        if self.mouse_down == Some(MouseButton::Middle) && self.is_zoomed() {
            panned = self.pan_by(
                [position.x - previous.x, position.y - previous.y],
                page_points,
            );
        }
        self.window.request_redraw();
        panned
    }

    fn pointer_button(&mut self, button: MouseButton, state: ElementState) {
        match state {
            ElementState::Pressed => {
                self.mouse_down = Some(button);
                if button == MouseButton::Right {
                    self.highlight_start = Some(self.mouse);
                }
                if button == MouseButton::Left {
                    self.laser.clear();
                    self.laser.push_back(self.mouse);
                }
            }
            ElementState::Released => {
                if self.mouse_down == Some(button) {
                    self.mouse_down = None;
                    self.highlight_start = None;
                    self.laser.clear();
                }
            }
        }
        self.window.request_redraw();
    }

    fn toggle_fullscreen(&mut self) {
        let fullscreen = self.window.fullscreen().is_none();
        self.fullscreen_transition_until = Some(Instant::now() + Duration::from_millis(1200));
        self.pending_aspect_size = None;
        self.window.set_fullscreen(if fullscreen {
            Some(Fullscreen::Borderless(None))
        } else {
            None
        });
        self.window.request_redraw();
    }

    fn exit_fullscreen(&mut self) {
        if self.window.fullscreen().is_none() {
            return;
        }
        self.fullscreen_transition_until = Some(Instant::now() + Duration::from_millis(1200));
        self.pending_aspect_size = None;
        self.window.set_fullscreen(None);
        self.window.request_redraw();
    }

    fn in_fullscreen_transition(&mut self) -> bool {
        if let Some(until) = self.fullscreen_transition_until {
            if Instant::now() < until {
                return true;
            }
            self.fullscreen_transition_until = None;
        }
        false
    }

    fn toggle_decorations(&mut self) {
        self.decorated = !self.decorated;
        set_window_decorations(self.window.as_ref(), self.decorated);
    }

    fn start_modified_window_drag(
        &self,
        button: MouseButton,
        state: ElementState,
        modifiers: ModifiersState,
    ) -> bool {
        if button != MouseButton::Left || state != ElementState::Pressed {
            return false;
        }
        if self.window.fullscreen().is_some() || self.fullscreen_transition_until.is_some() {
            return false;
        }
        if !(modifiers.control_key()) {
            return false;
        }
        if let Err(err) = self.window.drag_window() {
            eprintln!("window drag failed: {err:?}");
        }
        true
    }

    fn start_modified_window_resize(
        &mut self,
        button: MouseButton,
        state: ElementState,
        position: PhysicalPosition<f64>,
        modifiers: ModifiersState,
    ) -> bool {
        if button != MouseButton::Right
            || state != ElementState::Pressed
            || !modifiers.control_key()
        {
            return false;
        }
        if self.window.fullscreen().is_some() || self.fullscreen_transition_until.is_some() {
            return false;
        }
        self.resize_drag = Some(ResizeDrag {
            start_position: position,
            start_size: self.surface_size,
        });
        self.mouse_down = None;
        self.highlight_start = None;
        self.laser.clear();
        true
    }

    fn update_modified_window_resize(
        &mut self,
        position: PhysicalPosition<f64>,
        page_points: [f32; 2],
    ) -> bool {
        let Some(drag) = self.resize_drag else {
            return false;
        };
        let width = (drag.start_size.width as f64 + position.x - drag.start_position.x)
            .round()
            .max(64.0) as u32;
        let height = (drag.start_size.height as f64 + position.y - drag.start_position.y)
            .round()
            .max(64.0) as u32;
        let size = PhysicalSize::new(width, height);
        let requested = aspect_corrected_size(size, page_points).unwrap_or(size);
        self.pending_aspect_size = Some(requested);
        let _ = self.window.request_surface_size(requested.into());
        true
    }

    fn finish_modified_window_resize(&mut self, button: MouseButton, state: ElementState) -> bool {
        if button == MouseButton::Right
            && state == ElementState::Released
            && self.resize_drag.is_some()
        {
            self.resize_drag = None;
            return true;
        }
        false
    }
}

fn create_bind_group(gpu: &Gpu, view: &wgpu::TextureView) -> wgpu::BindGroup {
    gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("bind-group"),
        layout: &gpu.bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: gpu.uniform_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(&gpu.sampler),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: gpu.laser_buffer.as_entire_binding(),
            },
        ],
    })
}

#[cfg(target_os = "macos")]
fn set_window_decorations(window: &dyn Window, decorated: bool) {
    window.set_decorations(decorated);
}

#[cfg(not(target_os = "macos"))]
fn set_window_decorations(window: &dyn Window, decorated: bool) {
    window.set_decorations(decorated);
}

struct App {
    args: Args,
    source: PdfSource,
    generation_counter: Arc<AtomicU64>,
    page_count: usize,
    page_points: [f32; 2],
    current_page: usize,
    direction: i32,
    request_counter: u64,
    gpu: Option<Gpu>,
    windows: HashMap<WindowId, PresenterWindow>,
    proxy: EventLoopProxy,
    mac_nav_rx: Receiver<i32>,
    last_reload_check: Instant,
    modifiers: ModifiersState,
}

impl App {
    fn new(args: Args, proxy: EventLoopProxy, mac_nav_rx: Receiver<i32>) -> Result<Self> {
        let source = PdfSource::load(&args.pdf, 1)?;
        let info = source.info()?;
        if info.page_count == 0 {
            bail!("document has no pages");
        }
        Ok(Self {
            args,
            source,
            generation_counter: Arc::new(AtomicU64::new(1)),
            page_count: info.page_count,
            page_points: info.page_points,
            current_page: 0,
            direction: 1,
            request_counter: 0,
            gpu: None,
            windows: HashMap::new(),
            proxy,
            mac_nav_rx,
            last_reload_check: Instant::now(),
            modifiers: ModifiersState::empty(),
        })
    }

    fn create_windows(&mut self, event_loop: &dyn ActiveEventLoop) -> Result<()> {
        set_macos_app_icon();

        let initial_size = window_size_for_page(self.page_points);
        let initial_title = self.window_title(false);
        let first = event_loop
            .create_window(
                WindowAttributes::default()
                    .with_title(&initial_title)
                    .with_visible(true)
                    .with_window_icon(app_window_icon())
                    .with_surface_size(initial_size),
            )
            .context("creating initial window")?;
        let (gpu, surface) = Gpu::new(&first)?;
        let size = first.surface_size();
        let config = surface
            .get_configuration()
            .context("configured initial surface missing config")?;
        let worker = RenderWorker::spawn(
            gpu.device.clone(),
            gpu.queue.clone(),
            self.args.cache_mib.saturating_mul(1024 * 1024),
            self.proxy.clone(),
        );
        let bind_group = create_bind_group(&gpu, &gpu.placeholder.view);
        let presenter = PresenterWindow {
            surface,
            window: first,
            config,
            worker,
            current: None,
            full_page: None,
            full_page_bind_group: None,
            bind_group,
            wanted_request_id: 0,
            displayed_request_id: 0,
            surface_size: size,
            fullscreen_transition_until: None,
            pending_aspect_size: None,
            mouse: PhysicalPosition::new(0.0, 0.0),
            mouse_down: None,
            highlight_start: None,
            laser: VecDeque::new(),
            resize_drag: None,
            mirror: false,
            decorated: true,
            zoom: 1.0,
            pan: [0.0, 0.0],
            pending_zoom_render_at: None,
        };
        let id = presenter.id();
        self.gpu = Some(gpu);
        self.windows.insert(id, presenter);

        if self.args.mirror {
            let gpu = self.gpu.as_ref().expect("gpu initialized");
            let mirror_title = self.window_title(true);
            let mirror = PresenterWindow::new(
                event_loop,
                gpu,
                self.proxy.clone(),
                &mirror_title,
                self.args.cache_mib.saturating_mul(1024 * 1024),
                initial_size,
            )?;
            let mut mirror = mirror;
            mirror.mirror = true;
            mirror.decorated = false;
            set_window_decorations(mirror.window.as_ref(), false);
            self.windows.insert(mirror.id(), mirror);
        }
        self.schedule_all();
        Ok(())
    }

    fn window_title(&self, mirror: bool) -> String {
        let filename = self
            .args
            .pdf
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("document");
        let title = format!("{filename} – {}/{}", self.current_page + 1, self.page_count);
        if mirror {
            format!("[mirror] {title}")
        } else {
            title
        }
    }

    fn update_window_titles(&mut self) {
        let titles = self
            .windows
            .values()
            .map(|window| (window.id(), self.window_title(window.mirror)))
            .collect::<Vec<_>>();
        for (id, title) in titles {
            if let Some(window) = self.windows.get(&id) {
                window.window.set_title(&title);
            }
        }
    }

    fn max_texture_dimension_2d(&self) -> u32 {
        self.gpu
            .as_ref()
            .map(|gpu| gpu.max_texture_dimension_2d)
            .unwrap_or(u32::MAX)
    }

    fn schedule_all(&mut self) {
        self.request_counter = self.request_counter.wrapping_add(1);
        let request_id = self.request_counter;
        let max_texture_dimension_2d = self.max_texture_dimension_2d();
        for window in self.windows.values_mut() {
            let (size, source_rect) =
                window.render_request_geometry(max_texture_dimension_2d, self.page_points);
            window.set_page(RenderRequest {
                source: self.source.clone(),
                current_page: self.current_page,
                page_count: self.page_count,
                size,
                source_rect,
                direction: self.direction,
                ahead: self.args.ahead,
                request_id,
            });
        }
        self.proxy.wake_up();
    }

    fn schedule_window(&mut self, window_id: WindowId) {
        let max_texture_dimension_2d = self.max_texture_dimension_2d();
        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        self.request_counter = self.request_counter.wrapping_add(1);
        let request_id = self.request_counter;
        let (size, source_rect) =
            window.render_request_geometry(max_texture_dimension_2d, self.page_points);
        window.pending_zoom_render_at = None;
        window.set_page(RenderRequest {
            source: self.source.clone(),
            current_page: self.current_page,
            page_count: self.page_count,
            size,
            source_rect,
            direction: self.direction,
            ahead: self.args.ahead,
            request_id,
        });
        self.proxy.wake_up();
    }

    fn schedule_due_zoom_renders(&mut self) -> Option<Instant> {
        let now = Instant::now();
        let ids = self
            .windows
            .iter()
            .filter_map(|(id, window)| {
                window
                    .pending_zoom_render_at
                    .filter(|deadline| *deadline <= now)
                    .map(|_| *id)
            })
            .collect::<Vec<_>>();
        for id in ids {
            self.schedule_window(id);
        }
        self.windows
            .values()
            .filter_map(|window| window.pending_zoom_render_at)
            .min()
    }

    fn poll_workers(&mut self) {
        let Some(gpu) = self.gpu.as_ref() else {
            return;
        };
        let generation = self.source.generation;
        let direction = self.direction;
        for window in self.windows.values_mut() {
            window.poll_worker(
                gpu,
                generation,
                self.current_page,
                self.page_count,
                direction,
            );
        }
    }

    fn go(&mut self, delta: i32) {
        let next = (self.current_page as i32 + delta)
            .clamp(0, self.page_count.saturating_sub(1) as i32) as usize;
        if next != self.current_page {
            self.current_page = next;
            self.direction = delta.signum();
            for window in self.windows.values_mut() {
                window.reset_zoom_for_slide_change();
            }
            self.update_window_titles();
            self.schedule_all();
        }
    }

    fn drain_macos_navigation(&mut self) {
        while let Ok(delta) = self.mac_nav_rx.try_recv() {
            self.go(delta);
        }
    }

    fn mirror_pointer_moved(
        &mut self,
        source_id: WindowId,
        position: PhysicalPosition<f64>,
    ) -> bool {
        let Some(page_position) = self
            .windows
            .get(&source_id)
            .map(|window| window.page_unit_at(position, self.page_points))
        else {
            return false;
        };
        let mut panned = false;
        for window in self.windows.values_mut() {
            let position = window.position_for_page_unit(page_position, self.page_points);
            panned |= window.pointer_moved(position, self.page_points);
        }
        panned
    }

    fn mirror_pointer_button(
        &mut self,
        source_id: WindowId,
        button: MouseButton,
        state: ElementState,
    ) {
        let Some(page_position) = self
            .windows
            .get(&source_id)
            .map(|window| window.page_unit_at(window.mouse, self.page_points))
        else {
            return;
        };
        for window in self.windows.values_mut() {
            window.mouse = window.position_for_page_unit(page_position, self.page_points);
            window.pointer_button(button, state);
        }
    }

    fn reload(&mut self) -> Result<()> {
        let generation = self.generation_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let source = PdfSource::load(&self.args.pdf, generation)?;
        let info = source.info()?;
        if info.page_count == 0 {
            bail!("reloaded document has no pages");
        }
        self.source = source;
        self.page_count = info.page_count;
        self.page_points = info.page_points;
        let old_current_page = self.current_page;
        self.current_page = self.current_page.min(self.page_count - 1);
        if self.current_page != old_current_page {
            for window in self.windows.values_mut() {
                window.reset_zoom_for_slide_change();
            }
        }
        let requested_size = window_size_for_page(self.page_points);
        for window in self.windows.values() {
            let _ = window.window.request_surface_size(requested_size.into());
        }
        self.update_window_titles();
        self.schedule_all();
        Ok(())
    }

    fn check_hot_reload(&mut self) {
        if !self.args.hot_reload || self.last_reload_check.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.last_reload_check = Instant::now();
        let modified = fs::metadata(&self.args.pdf)
            .ok()
            .and_then(|m| m.modified().ok());
        if modified.is_some() && modified != self.source.modified {
            if let Err(err) = self.reload() {
                eprintln!("hot reload failed: {err:?}");
            }
        }
    }
}

impl ApplicationHandler for App {
    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        if let Err(err) = self.create_windows(event_loop) {
            eprintln!("{err:?}");
            event_loop.exit();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        self.poll_workers();
        let Some(gpu) = self.gpu.as_ref() else {
            return;
        };
        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };

        match event {
            WindowEvent::CloseRequested => {
                self.windows.remove(&window_id);
                if self.windows.is_empty() {
                    event_loop.exit();
                }
            }
            WindowEvent::SurfaceResized(size) => {
                let fullscreen_or_transition =
                    window.window.fullscreen().is_some() || window.in_fullscreen_transition();
                if !fullscreen_or_transition {
                    if let Some(pending) = window.pending_aspect_size.take() {
                        if size == pending {
                            window.resize(gpu, size);
                            self.schedule_all();
                            return;
                        }
                    }
                    if let Some(corrected) = aspect_corrected_size(size, self.page_points) {
                        window.pending_aspect_size = Some(corrected);
                        let _ = window.window.request_surface_size(corrected.into());
                        return;
                    }
                } else {
                    window.pending_aspect_size = None;
                }
                window.resize(gpu, size);
                self.schedule_all();
            }
            WindowEvent::RedrawRequested => {
                let resized = window.sync_surface_size(gpu);
                if let Err(err) = window.draw(gpu, self.page_points) {
                    eprintln!("{err:?}");
                }
                if resized {
                    window.clamp_pan(self.page_points);
                    window.window.request_redraw();
                    self.schedule_all();
                }
            }
            WindowEvent::PointerMoved { position, .. } => {
                if window.update_modified_window_resize(position, self.page_points) {
                    return;
                }
                if self.mirror_pointer_moved(window_id, position) {
                    for window in self.windows.values_mut() {
                        window.use_full_page_if_available();
                        window.defer_zoom_render();
                    }
                    if let Some(deadline) = self
                        .windows
                        .values()
                        .filter_map(|window| window.pending_zoom_render_at)
                        .min()
                    {
                        event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
                    }
                }
            }
            WindowEvent::PointerButton {
                button,
                state,
                position,
                ..
            } => {
                if let Some(button) = button.mouse_button() {
                    match (button, state) {
                        (MouseButton::Back, ElementState::Pressed) => self.go(-1),
                        (MouseButton::Forward, ElementState::Pressed) => self.go(1),
                        (MouseButton::Back | MouseButton::Forward, ElementState::Released) => {}
                        _ => {
                            if window.start_modified_window_drag(button, state, self.modifiers) {
                                return;
                            }
                            if window.start_modified_window_resize(
                                button,
                                state,
                                position,
                                self.modifiers,
                            ) {
                                return;
                            }
                            if window.finish_modified_window_resize(button, state) {
                                return;
                            }
                            let _ = self.mirror_pointer_moved(window_id, position);
                            self.mirror_pointer_button(window_id, button, state);
                        }
                    }
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.modifiers.shift_key() {
                    match delta {
                        MouseScrollDelta::LineDelta(_, y) if y < 0.0 => self.go(1),
                        MouseScrollDelta::LineDelta(_, y) if y > 0.0 => self.go(-1),
                        MouseScrollDelta::PixelDelta(delta) if delta.y < 0.0 => self.go(1),
                        MouseScrollDelta::PixelDelta(delta) if delta.y > 0.0 => self.go(-1),
                        _ => {}
                    }
                } else {
                    match delta {
                        MouseScrollDelta::LineDelta(_, y) if y != 0.0 => {
                            if window.zoom_about(
                                WHEEL_ZOOM_STEP.powf(y as f64),
                                window.mouse,
                                self.page_points,
                            ) {
                                window.use_full_page_if_available();
                                window.defer_zoom_render();
                                if let Some(deadline) = window.pending_zoom_render_at {
                                    event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
                                }
                            }
                        }
                        MouseScrollDelta::PixelDelta(delta)
                            if window.is_zoomed() && delta.x.abs() > delta.y.abs() =>
                        {
                            if window.pan_by([delta.x, delta.y], self.page_points) {
                                window.use_full_page_if_available();
                                window.defer_zoom_render();
                                if let Some(deadline) = window.pending_zoom_render_at {
                                    event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
                                }
                            }
                        }
                        MouseScrollDelta::PixelDelta(delta) if delta.y != 0.0 => {
                            if window.zoom_about(
                                (delta.y * PIXEL_ZOOM_SPEED).exp(),
                                window.mouse,
                                self.page_points,
                            ) {
                                window.use_full_page_if_available();
                                window.defer_zoom_render();
                                if let Some(deadline) = window.pending_zoom_render_at {
                                    event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            WindowEvent::PinchGesture { delta, phase, .. } => {
                let finished = matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled);
                let changed =
                    window.zoom_about((1.0 + delta).max(0.01), window.mouse, self.page_points);
                if changed && !finished {
                    window.use_full_page_if_available();
                    window.defer_zoom_render();
                    if let Some(deadline) = window.pending_zoom_render_at {
                        event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
                    }
                }
                if finished {
                    window.finish_zoom_render();
                    self.proxy.wake_up();
                }
            }
            WindowEvent::PanGesture { delta, .. } => {
                if window.is_zoomed()
                    && window.pan_by([delta.x as f64, delta.y as f64], self.page_points)
                {
                    window.use_full_page_if_available();
                    window.defer_zoom_render();
                    if let Some(deadline) = window.pending_zoom_render_at {
                        event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
                    }
                }
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } if event.state.is_pressed() => match &event.logical_key {
                Key::Named(NamedKey::ArrowRight | NamedKey::PageDown | NamedKey::Enter) => {
                    self.go(1)
                }
                Key::Named(NamedKey::ArrowLeft | NamedKey::PageUp | NamedKey::Backspace) => {
                    self.go(-1)
                }
                Key::Named(NamedKey::Home) => {
                    if self.current_page != 0 {
                        self.current_page = 0;
                        self.direction = -1;
                        for window in self.windows.values_mut() {
                            window.reset_zoom_for_slide_change();
                        }
                        self.update_window_titles();
                        self.schedule_all();
                    }
                }
                Key::Named(NamedKey::End) => {
                    let last = self.page_count.saturating_sub(1);
                    if self.current_page != last {
                        self.current_page = last;
                        self.direction = 1;
                        for window in self.windows.values_mut() {
                            window.reset_zoom_for_slide_change();
                        }
                        self.update_window_titles();
                        self.schedule_all();
                    }
                }
                Key::Character(ch)
                    if ch.eq_ignore_ascii_case("q") && self.modifiers.control_key() =>
                {
                    event_loop.exit();
                }
                Key::Named(NamedKey::F11) => window.toggle_fullscreen(),
                Key::Named(NamedKey::Escape) => window.exit_fullscreen(),
                Key::Character(ch) if ch == " " => self.go(1),
                Key::Character(ch) if ch == "0" => {
                    if window.reset_zoom(self.page_points) {
                        window.use_full_page_if_available();
                        window.finish_zoom_render();
                        self.proxy.wake_up();
                    }
                }
                Key::Character(ch) if ch.eq_ignore_ascii_case("f") => window.toggle_fullscreen(),
                Key::Character(ch) if ch.eq_ignore_ascii_case("d") => window.toggle_decorations(),
                Key::Character(ch) if ch.eq_ignore_ascii_case("r") => {
                    if let Err(err) = self.reload() {
                        eprintln!("reload failed: {err:?}");
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn proxy_wake_up(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.drain_macos_navigation();
        self.poll_workers();
        if let Some(deadline) = self.schedule_due_zoom_renders() {
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }

    fn about_to_wait(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.drain_macos_navigation();
        self.check_hot_reload();
        self.poll_workers();
        if let Some(deadline) = self.schedule_due_zoom_renders() {
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
}

fn main() -> Result<()> {
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

const SHADER: &str = r#"
struct Uniforms {
    surface_image: vec4<f32>,
    mouse_flags: vec4<f32>,
    highlight: vec4<f32>,
    zoom_pan: vec4<f32>,
    source_rect: vec4<f32>,
}

struct LaserPoints {
    points: array<vec4<f32>, 64>,
}

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var pdf_texture: texture_2d<f32>;
@group(0) @binding(2) var pdf_sampler: sampler;
@group(0) @binding(3) var<storage, read> laser: LaserPoints;

struct VertexOut {
    @builtin(position) pos: vec4<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOut {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -3.0),
        vec2<f32>(3.0, 1.0),
        vec2<f32>(-1.0, 1.0)
    );
    var out: VertexOut;
    out.pos = vec4<f32>(positions[vertex_index], 0.0, 1.0);
    return out;
}

fn image_rect() -> vec4<f32> {
    let surface = uniforms.surface_image.xy;
    let image = uniforms.surface_image.zw;
    let origin = floor((surface - image) * 0.5 + uniforms.zoom_pan.xy);
    return vec4<f32>(origin, image);
}

fn slide_scale() -> f32 {
    let rect = image_rect();
    return min(rect.z, rect.w);
}

fn sample_page(pixel: vec2<f32>) -> vec4<f32> {
    let rect = image_rect();
    let page_uv = (pixel - rect.xy) / rect.zw;
    if (page_uv.x < 0.0 || page_uv.y < 0.0 || page_uv.x > 1.0 || page_uv.y > 1.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    let source = uniforms.source_rect;
    if (page_uv.x < source.x || page_uv.y < source.y || page_uv.x > source.z || page_uv.y > source.w) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    let uv = (page_uv - source.xy) / max(source.zw - source.xy, vec2<f32>(0.000001, 0.000001));
    return textureSampleLevel(pdf_texture, pdf_sampler, uv, 0.0);
}

fn dist_to_segment(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>) -> f32 {
    let pa = p - a;
    let ba = b - a;
    let denom = max(dot(ba, ba), 0.0001);
    let h = clamp(dot(pa, ba) / denom, 0.0, 1.0);
    return length(pa - ba * h);
}

fn laser_overlay(pixel: vec2<f32>, color: vec4<f32>) -> vec4<f32> {
    let count = i32(min(uniforms.mouse_flags.w, 64.0));
    if (count <= 0) {
        return color;
    }
    let scale = slide_scale();
    let tail_radius = max(2.0, scale * 0.0167);
    let head_radius = max(3.0, scale * 0.025);
    var alpha = 0.0;
    for (var i = 1; i < 64; i = i + 1) {
        if (i >= count) {
            break;
        }
        let a = laser.points[i - 1].xy;
        let b = laser.points[i].xy;
        let d = dist_to_segment(pixel, a, b);
        let age = f32(i) / max(uniforms.mouse_flags.w, 1.0);
        alpha = max(alpha, smoothstep(tail_radius, 0.0, d) * age);
    }
    let head = uniforms.mouse_flags.xy;
    alpha = max(alpha, smoothstep(head_radius, 0.0, length(pixel - head)));
    let laser_color = vec4<f32>(1.0, 0.02, 0.02, 1.0);
    return mix(color, laser_color, clamp(alpha, 0.0, 0.95));
}

fn highlight_overlay(pixel: vec2<f32>, color: vec4<f32>) -> vec4<f32> {
    let a = min(uniforms.highlight.xy, uniforms.highlight.zw);
    let b = max(uniforms.highlight.xy, uniforms.highlight.zw);
    let inside = pixel.x >= a.x && pixel.x <= b.x && pixel.y >= a.y && pixel.y <= b.y;
    if (!inside) {
        return color;
    }
    return vec4<f32>(
        mix(color.rgb, vec3<f32>(1.0, 0.88, 0.05), 0.35),
        color.a
    );
}

fn magnifier_overlay(pixel: vec2<f32>, color: vec4<f32>) -> vec4<f32> {
    let center = uniforms.mouse_flags.xy;
    let delta = pixel - center;
    let scale = slide_scale();
    let radius = max(32.0, scale * 0.16);
    let shadow_width = max(6.0, scale * 0.033);
    let outline_outer = max(1.0, scale * 0.0035);
    let outline_inner = max(1.0, scale * 0.0042);
    let dist = length(delta);
    let shadow = smoothstep(radius + shadow_width, radius, dist) * 0.25;
    let scale_factor = 1.5;
    var out = vec4<f32>(color.rgb * (1.0 - shadow), color.a);
    if (dist < radius) {
        let zoomed = center + delta / scale_factor;
        out = sample_page(zoomed);
    }
    let outline = smoothstep(radius + outline_outer, radius, dist) - smoothstep(radius, radius - outline_inner, dist);
    out = mix(out, vec4<f32>(0.0, 0.0, 0.0, 1.0), clamp(outline, 0.0, 1.0));
    return out;
}

@fragment
fn fs_main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let pixel = pos.xy;
    let flags = u32(uniforms.mouse_flags.z);
    var color = sample_page(pixel);
    if ((flags & 2u) != 0u) {
        color = highlight_overlay(pixel, color);
    }
    if ((flags & 4u) != 0u) {
        color = magnifier_overlay(pixel, color);
    }
    if ((flags & 1u) != 0u) {
        color = laser_overlay(pixel, color);
    }
    return color;
}
"#;
