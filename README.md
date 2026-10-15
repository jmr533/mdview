# mdview

A fast, lightweight Markdown viewer that renders like GitHub. Built for Linux on **ppc64le** (and any other architecture), including **RHEL 8**.

mdview draws Markdown natively with GTK3. There's no browser engine, so it starts quickly, uses little memory, and scrolls smoothly over X11 forwarding and VNC.

- GitHub Flavored Markdown: tables, task lists, footnotes, autolinks, strikethrough, `> [!NOTE]` alerts, `:emoji:` shortcodes, front matter, and common inline HTML (`<img>`, `<br>`, `<kbd>`, `<details>`, `align="center"`)
- Syntax highlighting with GitHub's light and dark color palettes
- Light, dark, or follow-the-system theme
- **Live reload**: the view updates when the file changes on disk and keeps your place
- Outline sidebar, find-in-page, copy buttons on code blocks, zoom, back/forward
- Relative links between documents (including `#anchors`), local images, folder browsing (shows the README)
- Drag and drop files onto the window

## How it works

Markdown is parsed with [comrak](https://github.com/kivikakk/comrak) and highlighted with [syntect](https://github.com/trishume/syntect), both pure Rust. The result goes straight into a GTK text view as styled text. Code-block backgrounds, quote bars, table grids, checkboxes and alert icons are painted with cairo underneath the text. Over a remote display, GTK sends compact drawing commands rather than whole page images.

It needs only GTK 3.22+, which RHEL 8 ships. There is no WebKit dependency. The binary is about 4 MB.

## Install on RHEL 8 (ppc64le) without sudo

Download the prebuilt binary straight onto the POWER machine. Everything goes in your home directory:

```bash
mkdir -p ~/.local/bin ~/.local/share/applications
curl -fL -o ~/.local/bin/mdview https://github.com/jmr533/mdview/releases/latest/download/mdview-linux-ppc64le
chmod +x ~/.local/bin/mdview
curl -fL -o ~/.local/share/applications/mdview.desktop https://raw.githubusercontent.com/jmr533/mdview/main/packaging/mdview.desktop
mdview README.md        # ~/.local/bin is on PATH by default on RHEL 8
```

(`wget -O <file> <url>` works the same way if `curl` isn't available.)

### Building that binary yourself

On any machine (macOS or Linux, any CPU) with Rust, [zig](https://ziglang.org) and `cargo install cargo-zigbuild`:

```bash
rustup target add powerpc64le-unknown-linux-gnu
packaging/el8-cross/build.sh
```

The result is `target/powerpc64le-unknown-linux-gnu/release/mdview`. The script downloads RHEL 8–compatible GTK libraries for linking (from AlmaLinux 8) and targets glibc 2.28.

## Build on the machine itself

If you have root, or `gtk3-devel` is already installed (check with `pkg-config --modversion gtk+-3.0`):

```bash
sudo dnf install -y gcc pkgconf-pkg-config gtk3-devel   # skip if already installed
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # Rust 1.89+, installs in $HOME
cargo build --release
```

The same works on RHEL 9, Fedora, Debian and Ubuntu (`libgtk-3-dev`).

## Usage

```bash
mdview README.md          # open a file
mdview docs/              # browse a folder (shows its README)
mdview --theme dark FILE  # auto | light | dark (remembered)
mdview                    # welcome screen; press Ctrl+O or drop a file
```

| Action | Keys |
|---|---|
| Open file | <kbd>Ctrl</kbd> <kbd>O</kbd> |
| Find | <kbd>Ctrl</kbd> <kbd>F</kbd> or <kbd>/</kbd>, then <kbd>Enter</kbd> / <kbd>Shift</kbd> <kbd>Enter</kbd> |
| Outline | <kbd>F9</kbd> or <kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>O</kbd> |
| Theme | <kbd>Ctrl</kbd> <kbd>T</kbd> |
| Zoom | <kbd>Ctrl</kbd> <kbd>+</kbd> / <kbd>-</kbd> / <kbd>0</kbd> |
| Back / forward | <kbd>Alt</kbd> <kbd>←</kbd> / <kbd>→</kbd>, or <kbd>Backspace</kbd> |
| Reload / quit | <kbd>Ctrl</kbd> <kbd>R</kbd> / <kbd>Ctrl</kbd> <kbd>Q</kbd> |

Settings (theme, zoom, outline, window size) are stored in `~/.config/mdview/settings`.

Set `MDVIEW_TIMING=1` to print how long each document takes to render.

## Limitations

- Remote images (`https://…`, such as badges) are shown as their alt text; mdview never touches the network.
- Wide tables scroll sideways instead of wrapping. `<details>` sections are always expanded.
- Math is shown as code, and Mermaid diagrams as their source.
- Links to websites open in your default browser.
