# pdfpresenter

Minimal PDF presentation viewer for Beamer-style talks.

## Features

- Fast slide changes with background rendering and render-ahead.
- Laser pointer with tail, right-drag highlight, and middle-click magnifier.
- Optional mirror window for screensharing.
- Hot reload for regenerated PDFs.
- Fullscreen toggle and macOS decoration toggle.
- Mouse back/forward buttons, wheel and touchpad zoom, keyboard navigation, and macOS swipe navigation.

## Usage

```sh
cargo install --git https://github.com/dorianrudolph/pdfpresenter
```

```sh
pdfpresenter talk.pdf
```

Options:

```sh
pdfpresenter [OPTIONS] <PDF>

--mirror                 Open a second mirror window
--hot-reload             Reload when the PDF file changes
--cache-mib <MiB>        Per-window GPU page cache budget [default: 1024]
--ahead <N>              Pages to render ahead [default: 3]
```

## Controls

- `Right`, `PageDown`, `Enter`, `Space`, mouse forward, `Shift` + scroll down: next slide
- `Left`, `PageUp`, `Backspace`, mouse back, `Shift` + scroll up: previous slide
- `Home` / `End`: first / last slide
- `0`: reset zoom
- `f` or `F11`: toggle fullscreen
- `d`: toggle window decorations
- `r`: reload PDF
- `Esc`: exit fullscreen
- `Ctrl` + `Q`: quit
- Mouse wheel: zoom in / out
- Touchpad pinch: zoom in / out
- Two-finger pan or middle mouse drag while zoomed: pan
- Left mouse hold: laser pointer
- Right mouse drag: highlight
- Middle mouse hold while not zoomed: magnifier
- `Ctrl` + left drag: move window
- `Ctrl` + right drag: resize window

## License

AGPL-3.0
