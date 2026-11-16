# tilescope

A fast, WizTree-style treemap viewer for Linux that lets you zoom from a whole directory tree down to readable source code.

Built with Rust, egui and wgpu.

## Features

- **Squarified treemap** of any directory. Tiles can be sized by bytes, by lines or all equal.
- **Semantic zoom**: colored tiles, then a minimap-style line silhouette, then readable code with line numbers.
- **Previews** for images, PDFs (first page), EPUB covers, video thumbnails, SVG and rendered Markdown.
- **Folder navigation**: click a folder bar to show only that folder. You also get breadcrumbs, back/forward history and an up button.
- **Git coloring** by last-modified date or by churn (commit count).
- **Search**: filter by path, or run full-text content search with matching lines highlighted and hit-to-hit jumping.
- **Open files** in your editor at the line under the cursor, open them with the default app, or show them in the file manager.
- **Minimap**, extension statistics and filter-by-extension.
- **Responsive on large trees**: files are loaded lazily in the background, thumbnails are cached on disk and drawing goes through the GPU.

## Install

Prebuilt binary (x86_64, glibc ≥ 2.39):

```bash
chmod +x tilescope-linux-x86_64
./tilescope-linux-x86_64 ~/src/project
```

Build from source (Rust 1.88+):

```bash
cargo build --release
./target/release/tilescope ~/src/project
```

### Optional tools

| Tool | Used for |
|---|---|
| `pdftoppm` (poppler-utils) | PDF previews |
| `ffmpeg` | Video thumbnails |
| `git` | Git coloring |
| `xdg-open` | Opening files and folders |

Each feature is simply skipped when its tool is missing.

## Usage

```
tilescope [DIR] [options]

--metric bytes|lines|equal   Tile size (default: bytes). Shortcuts: --lines, --equal
--color ext|age|churn        Tile color: extension, git last modified, git commit count
--grep TEXT                  Start with a content search
--ext py,rs,ts               Only include these extensions
--exclude PATTERN            Exclude names matching a glob (repeatable)
--hidden                     Include dotfiles
--no-default-excludes        Don't skip .git, node_modules, target, build, ...
--max-kb N                   Max text file size to load (default: 1024)
--max-image-mb N             Max image size to decode (default: 64)
--git-max-commits N          Git history depth (default: 50000)
```

## Controls

| Input | Action |
|---|---|
| Mouse wheel / drag | Zoom / pan |
| Double-click | Zoom to tile |
| Click folder bar | Show only that folder |
| `Backspace` / `Alt+↑` / ⬆ | Up one level |
| `Alt+←/→` / mouse back/forward / ◀ ▶ | History back / forward |
| `Esc` | Zoom out one step |
| `0` / `Home` | Fit current folder |
| Right-click | Context menu (open, editor, file manager, copy path, show folder) |
| `o` | Open hovered file in editor at hovered line |
| `n` / `Enter` | Next search hit |
| `m` | Toggle minimap |

The editor is taken from `$TILESCOPE_EDITOR`, then `$VISUAL`, then `$EDITOR`. Line jumps are supported for VS Code, Zed, Sublime, Kate, vim, nano, emacs, helix and others. Terminal editors open in a terminal emulator.

## Cache

Thumbnails for PDFs, EPUBs, videos, SVGs and large images are stored in `~/.cache/tilescope/`. You can safely delete this folder at any time.
