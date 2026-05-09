use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
};

use anyhow::{Result, bail};
use mupdf::{Colorspace, Device as MupdfDevice, Document, IRect, Matrix, Pixmap};
use winit::{dpi::PhysicalSize, event_loop::EventLoopProxy};

use crate::{constants::SOURCE_RECT_SCALE, pdf::PdfSource};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RenderKey {
    pub(crate) generation: u64,
    pub(crate) page: usize,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) source: SourceRectKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SourceRectKey {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

impl SourceRectKey {
    pub(crate) const FULL: Self = Self {
        x0: 0,
        y0: 0,
        x1: SOURCE_RECT_SCALE as u32,
        y1: SOURCE_RECT_SCALE as u32,
    };

    pub(crate) fn from_unit_rect(rect: [f64; 4]) -> Self {
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

    pub(crate) fn as_unit_rect(self) -> [f32; 4] {
        [
            self.x0 as f32 / SOURCE_RECT_SCALE as f32,
            self.y0 as f32 / SOURCE_RECT_SCALE as f32,
            self.x1 as f32 / SOURCE_RECT_SCALE as f32,
            self.y1 as f32 / SOURCE_RECT_SCALE as f32,
        ]
    }

    pub(crate) fn is_full(self) -> bool {
        self == Self::FULL
    }
}

#[derive(Clone)]
pub(crate) struct RenderedPage {
    pub(crate) key: RenderKey,
    pub(crate) _texture: Arc<wgpu::Texture>,
    pub(crate) view: Arc<wgpu::TextureView>,
    pub(crate) bytes: u64,
    pub(crate) source_rect: [f32; 4],
    pub(crate) page_points: [f32; 2],
}

#[derive(Clone)]
pub(crate) struct RenderRequest {
    pub(crate) source: PdfSource,
    pub(crate) current_page: usize,
    pub(crate) page_count: usize,
    pub(crate) size: PhysicalSize<u32>,
    pub(crate) source_rect: SourceRectKey,
    pub(crate) direction: i32,
    pub(crate) ahead: i32,
    pub(crate) request_id: u64,
}

enum RenderMessage {
    Request(RenderRequest),
    Stop,
}

pub(crate) enum RenderResult {
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

pub(crate) struct RenderWorker {
    tx: Sender<RenderMessage>,
    pub(crate) rx: Receiver<RenderResult>,
    thread: Option<JoinHandle<()>>,
}

impl RenderWorker {
    pub(crate) fn spawn(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        cache_limit: u64,
        proxy: EventLoopProxy,
    ) -> Self {
        let (request_tx, request_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("mupdf-render-worker".to_string())
            .spawn(move || {
                render_worker_loop(device, queue, cache_limit, request_rx, result_tx, proxy)
            })
            .expect("spawn render worker");
        Self {
            tx: request_tx,
            rx: result_rx,
            thread: Some(thread),
        }
    }

    pub(crate) fn request(&self, request: RenderRequest) {
        let _ = self.tx.send(RenderMessage::Request(request));
    }
}

impl Drop for RenderWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(RenderMessage::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
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
