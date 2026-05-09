use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use bytemuck::Zeroable;
use winit::{
    dpi::{PhysicalPosition, PhysicalSize},
    event::{ElementState, MouseButton},
    event_loop::{ActiveEventLoop, EventLoopProxy},
    keyboard::ModifiersState,
    monitor::Fullscreen,
    window::{Window, WindowAttributes, WindowId},
};

use crate::{
    constants::{
        FLAG_HIGHLIGHT, FLAG_LASER, FLAG_MAGNIFY, LASER_POINTS, MAX_ZOOM, MIN_ZOOM, ZOOM_EPSILON,
        ZOOM_RENDER_DEBOUNCE,
    },
    gpu::{Gpu, LaserPoint, Uniforms, create_bind_group},
    icon::app_window_icon,
    pdf::aspect_corrected_size,
    platform::set_window_decorations,
    render::{RenderRequest, RenderResult, RenderWorker, RenderedPage, SourceRectKey},
};

#[derive(Clone, Copy)]
pub(crate) struct ResizeDrag {
    start_position: PhysicalPosition<f64>,
    start_size: PhysicalSize<u32>,
}

pub(crate) struct PresenterWindow {
    pub(crate) worker: RenderWorker,
    pub(crate) surface: wgpu::Surface<'static>,
    pub(crate) window: Box<dyn Window>,
    pub(crate) config: wgpu::SurfaceConfiguration,
    pub(crate) current: Option<RenderedPage>,
    pub(crate) full_page: Option<RenderedPage>,
    pub(crate) full_page_bind_group: Option<wgpu::BindGroup>,
    pub(crate) bind_group: wgpu::BindGroup,
    pub(crate) wanted_request_id: u64,
    pub(crate) displayed_request_id: u64,
    pub(crate) surface_size: PhysicalSize<u32>,
    pub(crate) fullscreen_transition_until: Option<Instant>,
    pub(crate) pending_aspect_size: Option<PhysicalSize<u32>>,
    pub(crate) mouse: PhysicalPosition<f64>,
    pub(crate) mouse_down: Option<MouseButton>,
    pub(crate) highlight_start: Option<PhysicalPosition<f64>>,
    pub(crate) laser: VecDeque<PhysicalPosition<f64>>,
    pub(crate) resize_drag: Option<ResizeDrag>,
    pub(crate) mirror: bool,
    pub(crate) decorated: bool,
    pub(crate) zoom: f64,
    pub(crate) pan: [f64; 2],
    pub(crate) pending_zoom_render_at: Option<Instant>,
}

impl PresenterWindow {
    pub(crate) fn from_surface(
        surface: wgpu::Surface<'static>,
        window: Box<dyn Window>,
        config: wgpu::SurfaceConfiguration,
        gpu: &Gpu,
        proxy: EventLoopProxy,
        cache_limit: u64,
    ) -> Self {
        let size = window.surface_size();
        let worker = RenderWorker::spawn(gpu.device.clone(), gpu.queue.clone(), cache_limit, proxy);
        let bind_group = create_bind_group(gpu, &gpu.placeholder.view);
        Self {
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
        }
    }

    pub(crate) fn new(
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
        Ok(Self::from_surface(
            surface,
            window,
            config,
            gpu,
            proxy,
            cache_limit,
        ))
    }

    pub(crate) fn id(&self) -> WindowId {
        self.window.id()
    }

    pub(crate) fn resize(&mut self, gpu: &Gpu, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.surface_size = size;
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&gpu.device, &self.config);
    }

    pub(crate) fn sync_surface_size(&mut self, gpu: &Gpu) -> bool {
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

    pub(crate) fn page_image_size(&self, page_points: [f32; 2]) -> [f64; 2] {
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

    pub(crate) fn draw_image_size(&self, page_points: [f32; 2]) -> [f64; 2] {
        let base = self.page_image_size(page_points);
        [base[0] * self.zoom, base[1] * self.zoom]
    }

    pub(crate) fn render_request_geometry(
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

    pub(crate) fn is_zoomed(&self) -> bool {
        (self.zoom - 1.0).abs() > ZOOM_EPSILON || self.pan[0].abs() > 0.5 || self.pan[1].abs() > 0.5
    }

    pub(crate) fn page_image_rect(&self, page_points: [f32; 2]) -> [f64; 4] {
        let image = self.draw_image_size(page_points);
        [
            ((self.surface_size.width as f64 - image[0]) * 0.5 + self.pan[0]).floor(),
            ((self.surface_size.height as f64 - image[1]) * 0.5 + self.pan[1]).floor(),
            image[0],
            image[1],
        ]
    }

    pub(crate) fn page_unit_at(
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

    pub(crate) fn position_for_page_unit(
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

    pub(crate) fn clamp_pan(&mut self, page_points: [f32; 2]) {
        let image = self.draw_image_size(page_points);
        let max_x = ((image[0] - self.surface_size.width as f64) * 0.5).max(0.0);
        let max_y = ((image[1] - self.surface_size.height as f64) * 0.5).max(0.0);
        self.pan[0] = self.pan[0].clamp(-max_x, max_x);
        self.pan[1] = self.pan[1].clamp(-max_y, max_y);
    }

    pub(crate) fn zoom_about(
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

    pub(crate) fn pan_by(&mut self, delta: [f64; 2], page_points: [f32; 2]) -> bool {
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

    pub(crate) fn reset_zoom(&mut self, page_points: [f32; 2]) -> bool {
        let changed = self.is_zoomed();
        self.zoom = 1.0;
        self.pan = [0.0, 0.0];
        self.clamp_pan(page_points);
        if changed {
            self.window.request_redraw();
        }
        changed
    }

    pub(crate) fn reset_zoom_for_slide_change(&mut self) {
        self.zoom = 1.0;
        self.pan = [0.0, 0.0];
        self.pending_zoom_render_at = None;
        self.full_page = None;
        self.full_page_bind_group = None;
        self.window.request_redraw();
    }

    pub(crate) fn use_full_page_if_available(&mut self) {
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

    pub(crate) fn defer_zoom_render(&mut self) {
        self.pending_zoom_render_at = Some(Instant::now() + ZOOM_RENDER_DEBOUNCE);
    }

    pub(crate) fn finish_zoom_render(&mut self) {
        self.pending_zoom_render_at = Some(Instant::now());
    }

    pub(crate) fn set_page(&mut self, request: RenderRequest) {
        self.wanted_request_id = request.request_id;
        self.worker.request(request);
    }

    pub(crate) fn poll_worker(
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

    pub(crate) fn is_page_between_displayed_and_current(
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

    pub(crate) fn draw(&mut self, gpu: &Gpu, page_points: [f32; 2]) -> Result<()> {
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

    pub(crate) fn flags(&self) -> u32 {
        let mut flags = 0;
        match self.mouse_down {
            Some(MouseButton::Left) => flags |= FLAG_LASER,
            Some(MouseButton::Right) => flags |= FLAG_HIGHLIGHT,
            Some(MouseButton::Middle) if !self.is_zoomed() => flags |= FLAG_MAGNIFY,
            _ => {}
        }
        flags
    }

    pub(crate) fn pointer_moved(
        &mut self,
        position: PhysicalPosition<f64>,
        page_points: [f32; 2],
    ) -> bool {
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

    pub(crate) fn pointer_button(&mut self, button: MouseButton, state: ElementState) {
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

    pub(crate) fn toggle_fullscreen(&mut self) {
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

    pub(crate) fn exit_fullscreen(&mut self) {
        if self.window.fullscreen().is_none() {
            return;
        }
        self.fullscreen_transition_until = Some(Instant::now() + Duration::from_millis(1200));
        self.pending_aspect_size = None;
        self.window.set_fullscreen(None);
        self.window.request_redraw();
    }

    pub(crate) fn in_fullscreen_transition(&mut self) -> bool {
        if let Some(until) = self.fullscreen_transition_until {
            if Instant::now() < until {
                return true;
            }
            self.fullscreen_transition_until = None;
        }
        false
    }

    pub(crate) fn toggle_decorations(&mut self) {
        self.decorated = !self.decorated;
        set_window_decorations(self.window.as_ref(), self.decorated);
    }

    pub(crate) fn start_modified_window_drag(
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

    pub(crate) fn start_modified_window_resize(
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

    pub(crate) fn update_modified_window_resize(
        &mut self,
        position: PhysicalPosition<f64>,
        page_points: [f32; 2],
        preserve_aspect: bool,
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
        let requested = if preserve_aspect {
            aspect_corrected_size(size, page_points).unwrap_or(size)
        } else {
            size
        };
        self.pending_aspect_size = Some(requested);
        let _ = self.window.request_surface_size(requested.into());
        true
    }

    pub(crate) fn finish_modified_window_resize(
        &mut self,
        button: MouseButton,
        state: ElementState,
    ) -> bool {
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
