# Project Layout

`pdfstage` is a small Rust application for presenting PDFs with MuPDF, WGPU, and winit.

- `src/main.rs`: startup only. Parses CLI args, creates the event loop, installs platform hooks, and runs `App`.
- `src/app.rs`: top-level application state, window collection, hot reload, navigation, scheduling, and winit event handling.
- `src/presenter.rs`: per-window state and behavior, including zoom, pan, pointer tools, fullscreen/decorations, render requests, and drawing.
- `src/render.rs`: PDF render worker, render request/result types, source-rect keys, page texture creation, prefetching, and texture cache.
- `src/gpu.rs`: WGPU initialization, pipeline/bind-group setup, GPU uniform types, placeholder texture, and bind group creation.
- `src/pdf.rs`: PDF source loading, document metadata, page/window sizing helpers, and input navigation helpers.
- `src/cli.rs`: command-line arguments.
- `src/icon.rs`: window/app icon loading, including macOS app icon setup.
- `src/platform.rs`: platform-specific hooks such as macOS swipe monitoring and window decoration calls.
- `src/constants.rs`: shared tuning constants and shader flags.
- `src/shader.wgsl`: WGSL shader used by `gpu.rs`.

Keep changes near the responsibility they affect. Avoid moving code just to reduce file size; prefer module boundaries that match runtime ownership: app orchestration, window behavior, rendering, GPU setup, PDF data, and platform glue.
