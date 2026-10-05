use std::{
    collections::HashMap,
    fs,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::Receiver,
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalPosition,
    event::{ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy},
    keyboard::{Key, ModifiersState, NamedKey},
    window::{WindowAttributes, WindowId},
};

use crate::{
    cli::Args,
    constants::{PIXEL_ZOOM_SPEED, WHEEL_ZOOM_STEP},
    gpu::Gpu,
    icon::{app_window_icon, set_macos_app_icon},
    pdf::{PdfSource, aspect_corrected_size, scroll_navigation_delta, window_size_for_page},
    platform::set_window_decorations,
    presenter::PresenterWindow,
    render::RenderRequest,
};

fn keyboard_navigation_delta<Str: AsRef<str>>(key: &Key<Str>) -> Option<i32> {
    match key {
        Key::Named(
            NamedKey::ArrowRight | NamedKey::PageDown | NamedKey::Enter | NamedKey::BrowserForward,
        ) => Some(1),
        Key::Named(
            NamedKey::ArrowLeft
            | NamedKey::PageUp
            | NamedKey::Backspace
            | NamedKey::BrowserBack
            | NamedKey::GoBack,
        ) => Some(-1),
        Key::Character(ch) if ch.as_ref() == " " => Some(1),
        _ => None,
    }
}

fn mouse_navigation_delta(button: MouseButton, state: ElementState) -> Option<i32> {
    if !state.is_pressed() {
        return None;
    }
    match button {
        MouseButton::Back => Some(-1),
        MouseButton::Forward => Some(1),
        _ => None,
    }
}

fn is_mouse_navigation_button(button: MouseButton) -> bool {
    matches!(button, MouseButton::Back | MouseButton::Forward)
}

pub(crate) struct App {
    args: Args,
    source: PdfSource,
    generation_counter: Arc<AtomicU64>,
    page_count: usize,
    page_points: [f32; 2],
    current_page: usize,
    direction: i32,
    request_counter: u64,
    windows: HashMap<WindowId, PresenterWindow>,
    gpu: Option<Gpu>,
    proxy: EventLoopProxy,
    mac_nav_rx: Receiver<i32>,
    last_reload_check: Instant,
    modifiers: ModifiersState,
    quitting: bool,
}

impl App {
    pub(crate) fn new(
        args: Args,
        proxy: EventLoopProxy,
        mac_nav_rx: Receiver<i32>,
    ) -> Result<Self> {
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
            windows: HashMap::new(),
            gpu: None,
            proxy,
            mac_nav_rx,
            last_reload_check: Instant::now(),
            modifiers: ModifiersState::empty(),
            quitting: false,
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
        let config =
            surface.get_configuration().context("configured initial surface missing config")?;
        let presenter = PresenterWindow::from_surface(
            surface,
            first,
            config,
            &gpu,
            self.proxy.clone(),
            self.args.cache_mib.saturating_mul(1024 * 1024),
        );
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
        let filename =
            self.args.pdf.file_name().and_then(|name| name.to_str()).unwrap_or("document");
        let title = format!("{filename} – {}/{}", self.current_page + 1, self.page_count);
        if mirror { format!("[mirror] {title}") } else { title }
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
        self.gpu.as_ref().map(|gpu| gpu.max_texture_dimension_2d).unwrap_or(u32::MAX)
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
                window.pending_zoom_render_at.filter(|deadline| *deadline <= now).map(|_| *id)
            })
            .collect::<Vec<_>>();
        for id in ids {
            self.schedule_window(id);
        }
        self.windows.values().filter_map(|window| window.pending_zoom_render_at).min()
    }

    fn poll_workers(&mut self) {
        let Some(gpu) = self.gpu.as_ref() else {
            return;
        };
        let generation = self.source.generation;
        let direction = self.direction;
        for window in self.windows.values_mut() {
            window.poll_worker(gpu, generation, self.current_page, self.page_count, direction);
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
        let aspect_changed =
            self.page_points[0] / self.page_points[1] != info.page_points[0] / info.page_points[1];
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
        if aspect_changed && !self.args.free_aspect {
            for window in self.windows.values_mut() {
                // Keep the fullscreen surface at the size chosen by the compositor.
                if window.window.fullscreen().is_some() || window.in_fullscreen_transition() {
                    continue;
                }
                if let Some(corrected) =
                    aspect_corrected_size(window.window.surface_size(), self.page_points)
                {
                    let _ = window.request_surface_size(corrected, "reload-aspect-correction");
                }
            }
        }
        self.update_window_titles();
        self.schedule_all();
        Ok(())
    }

    fn check_hot_reload(&mut self) {
        if !self.args.hot_reload || self.last_reload_check.elapsed() < Self::HOT_RELOAD_INTERVAL {
            return;
        }
        self.last_reload_check = Instant::now();
        let modified = fs::metadata(&self.args.pdf).ok().and_then(|m| m.modified().ok());
        if modified.is_some() && modified != self.source.modified {
            if let Err(err) = self.reload() {
                eprintln!("hot reload failed: {err:?}");
            }
        }
    }

    const HOT_RELOAD_INTERVAL: Duration = Duration::from_millis(500);

    fn next_hot_reload_check(&self) -> Option<Instant> {
        self.args.hot_reload.then_some(self.last_reload_check + Self::HOT_RELOAD_INTERVAL)
    }

    fn set_idle_control_flow(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        zoom_deadline: Option<Instant>,
    ) {
        let deadline = [zoom_deadline, self.next_hot_reload_check()].into_iter().flatten().min();
        if let Some(deadline) = deadline {
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }

    fn request_quit(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.quitting = true;
        event_loop.set_control_flow(ControlFlow::Poll);
        self.proxy.wake_up();
    }

    fn finish_quit(&mut self, event_loop: &dyn ActiveEventLoop) -> bool {
        if !self.quitting {
            return false;
        }
        self.windows.clear();
        self.gpu = None;
        event_loop.exit();
        true
    }
}

impl ApplicationHandler for App {
    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        if let Err(err) = self.create_windows(event_loop) {
            eprintln!("{err:?}");
            self.quitting = true;
            self.finish_quit(event_loop);
        }
    }

    fn window_event(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if self.quitting {
            return;
        }

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
                    self.request_quit(event_loop);
                }
            }
            WindowEvent::SurfaceResized(size) => {
                let fullscreen_or_transition =
                    window.window.fullscreen().is_some() || window.in_fullscreen_transition();
                log::debug!(
                    "[resize] window={window_id:?} SurfaceResized size={size:?} configured={:?} pending={:?} free_aspect={} fullscreen_or_transition={fullscreen_or_transition}",
                    window.surface_size,
                    window.pending_aspect_size,
                    self.args.free_aspect
                );
                if !fullscreen_or_transition && !self.args.free_aspect {
                    if let Some(pending) = window.pending_aspect_size.take() {
                        if size == pending {
                            log::debug!(
                                "[resize] window={window_id:?} pending aspect size accepted: {size:?}"
                            );
                            window.last_aspect_request = None;
                            window.resize(gpu, size);
                            self.schedule_all();
                            return;
                        }
                        log::debug!(
                            "[resize] window={window_id:?} pending aspect size {pending:?} did not match event size {size:?}"
                        );
                    }
                    if let Some(corrected) = aspect_corrected_size(size, self.page_points) {
                        let repeated_request =
                            window.last_aspect_request == Some((size, corrected));
                        window.resize(gpu, size);
                        if repeated_request {
                            log::debug!(
                                "[resize] window={window_id:?} suppressing repeated aspect request for size={size:?} corrected={corrected:?}"
                            );
                        } else {
                            match window.request_surface_size(corrected, "aspect-correction") {
                                Some(applied) => {
                                    window.pending_aspect_size = None;
                                    window.last_aspect_request = Some((size, corrected));
                                    log::debug!(
                                        "[resize] window={window_id:?} aspect request returned {applied:?}; keeping SurfaceResized size {size:?} until compositor reports another size"
                                    );
                                }
                                None => {
                                    window.pending_aspect_size = Some(corrected);
                                    window.last_aspect_request = None;
                                }
                            }
                        }
                        self.schedule_all();
                        return;
                    }
                } else {
                    window.pending_aspect_size = None;
                }
                window.last_aspect_request = None;
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
                if let Some(resized) = window.update_modified_window_resize(
                    gpu,
                    position,
                    self.page_points,
                    !self.args.free_aspect,
                ) {
                    if resized {
                        self.schedule_all();
                    }
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
            WindowEvent::PointerButton { button, state, position, .. } => {
                if let Some(button) = button.mouse_button() {
                    if let Some(delta) = mouse_navigation_delta(button, state) {
                        self.go(delta);
                        return;
                    }
                    if is_mouse_navigation_button(button) {
                        return;
                    }
                    if window.start_modified_window_drag(button, state, self.modifiers) {
                        return;
                    }
                    if window.start_modified_window_resize(button, state, position, self.modifiers)
                    {
                        return;
                    }
                    if window.finish_modified_window_resize(button, state) {
                        return;
                    }
                    let _ = self.mirror_pointer_moved(window_id, position);
                    self.mirror_pointer_button(window_id, button, state);
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.modifiers.shift_key() {
                    let delta = match delta {
                        MouseScrollDelta::LineDelta(x, y) => {
                            scroll_navigation_delta(x as f64, y as f64)
                        }
                        MouseScrollDelta::PixelDelta(delta) => {
                            scroll_navigation_delta(delta.x, delta.y)
                        }
                        _ => None,
                    };
                    if let Some(delta) = delta {
                        self.go(delta);
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
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. }
                if event.state.is_pressed() =>
            {
                if let Some(delta) = keyboard_navigation_delta(&event.logical_key) {
                    self.go(delta);
                    return;
                }
                match &event.logical_key {
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
                        self.request_quit(event_loop);
                    }
                    Key::Named(NamedKey::F11) => window.toggle_fullscreen(),
                    Key::Named(NamedKey::Escape) => window.exit_fullscreen(),
                    Key::Character(ch) if ch == "0" => {
                        if window.reset_zoom(self.page_points) {
                            window.use_full_page_if_available();
                            window.finish_zoom_render();
                            self.proxy.wake_up();
                        }
                    }
                    Key::Character(ch) if ch.eq_ignore_ascii_case("f") => {
                        window.toggle_fullscreen()
                    }
                    Key::Character(ch) if ch.eq_ignore_ascii_case("d") => {
                        window.toggle_decorations()
                    }
                    Key::Character(ch) if ch.eq_ignore_ascii_case("r") => {
                        if let Err(err) = self.reload() {
                            eprintln!("reload failed: {err:?}");
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn proxy_wake_up(&mut self, event_loop: &dyn ActiveEventLoop) {
        if self.finish_quit(event_loop) {
            return;
        }
        self.drain_macos_navigation();
        self.poll_workers();
        let zoom_deadline = self.schedule_due_zoom_renders();
        self.set_idle_control_flow(event_loop, zoom_deadline);
    }

    fn about_to_wait(&mut self, event_loop: &dyn ActiveEventLoop) {
        if self.finish_quit(event_loop) {
            return;
        }
        self.drain_macos_navigation();
        self.check_hot_reload();
        self.poll_workers();
        let zoom_deadline = self.schedule_due_zoom_renders();
        self.set_idle_control_flow(event_loop, zoom_deadline);
    }
}
