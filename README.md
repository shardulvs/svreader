# svreader

> A brave command-line utility that refuses to leave the terminal,
> defying a world obsessed with Electron apps.

Terminal PDF reader with vim keybindings. Renders pages with **sixel**,
so it works in any modern terminal (WezTerm, foot, Ghostty, Konsole,
xterm with sixel, iTerm2, mintty, Windows Terminal, mlterm) — and
cleanly through tmux.

Reading state — last page, zoom, scroll offset, night mode, rotation —
is persisted to a `<file>.sdr/metadata.pdf.lua` sidecar next to each
document, so your place in a book survives across runs.

## Features

- Full vim navigation: `j k h l`, `<C-d>/<C-u>`, `<C-f>/<C-b>`,
  `gg`/`G`/`[N]G`, `H M L`, count prefixes, `:` command palette with
  completion, `?` help overlay.
- Zoom modes: fit-width / fit-height / fit-page / custom; rotation
  0/90/180/270; night mode with RGB inversion.
- Vim-style **tabs** (`:tabnew`, `gt`/`gT`, `Ctrl-PageUp/Down`) and
  **splits** (`:split`, `:vsplit`, `<C-w>h/j/k/l`, `Alt-<arrow>`,
  `Shift-Alt-<arrow>` to resize). Two windows can share one buffer
  (vim semantics) or hold different PDFs.
- **Netrw-style file explorer** (`:Ex`, `:Sex`, `:Vex`). Argless
  `svreader` lands in an explorer at `$PWD`; `svreader some/dir/`
  lands in an explorer there.
- In-process **LRU page cache** with background prefetch. Runtime
  knobs: `:cache on/off/toggle`, `:cache-size N`, `:prefetch N`.
- **Quality / DPI levers** (`:quality N%`, `:dpi N|auto`) to trade
  sharpness for encode+transmit speed. Status bar shows a per-stage
  frame-time breakdown so it's obvious which lever to pull.
- **Per-document state persistence:** last page, zoom, rotation,
  scroll offsets, night mode, render DPI/quality, cache enabled.

## Requirements

- **A sixel-capable terminal.** If your terminal doesn't advertise
  sixel via `CSI c`, svreader logs a warning at startup and you
  won't see any image.
- **tmux** users: add `set -g allow-passthrough on` to `~/.tmux.conf`
  so sixel DCS sequences reach the outer terminal.
- **Build deps:** Rust toolchain (stable, 1.80+), plus the system
  packages `mupdf-rs` needs to build vendored libmupdf:
  ```
  sudo apt install build-essential clang pkg-config
  ```

## Build

```
make release        # or: cargo build --release --workspace
```

The binary lands at `./target/release/svreader`.

## Run

```
svreader                    # explorer rooted at $PWD
svreader some/dir/          # explorer rooted at some/dir
svreader paper.pdf          # open a PDF directly
svreader --page 42 book.pdf # start on page 42
```

Inside an explorer: `j`/`k` select, `Enter`/`l` descend a directory
or open the PDF in that window, `-`/`h`/`u`/`Backspace` go to the
parent.

Inside a PDF: `?` opens the keybinding cheatsheet; `:` opens the
command palette (Tab/↑↓/`C-n`/`C-p` cycle completions, Enter pastes
the highlight then Enter again to execute).

## Project layout

```
crates/
  svreader-core/   pure backend; no terminal deps.
  svreader-cli/    headless debug harness (render pages to PNG,
                   simulate key sequences, dump outlines, …).
  svreader-tui/    sixel output + ratatui overlays.
src/               top-level binary (delegates to svreader-tui).
```

`svreader-core` is testable without a terminal; `svreader-cli` is the
reproducibility harness — every rendering decision is reproducible
from the CLI as a PNG on disk.

## Status

v1 — PDFs only. The `Document` trait is ready for EPUB / DjVu / CBZ
backends, not implemented yet. No cloud sync, no dictionary lookup,
no stats. If a terminal doesn't speak sixel, svreader doesn't try to
fall back to a different protocol.

## License

AGPL-3.0-or-later.
