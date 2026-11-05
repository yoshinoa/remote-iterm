# Remote iTerm — desktop app

A native window for the remote-iterm server, built with [GPUI](https://gpui.rs). It
shows your Mac's iTerm tabs and lets you type into them with predictive echo.
Characters appear as soon as you press a key, and are corrected if the Mac
disagrees.

It talks to the same server as the web client, so nothing on the Mac changes.

## Build on Windows

1. Install [rustup](https://rustup.rs). The `rust-toolchain.toml` in this folder
   pins Rust 1.99.1, and rustup installs it on the first build.
2. Install **Visual Studio 2022 Build Tools** with the **Desktop development
   with C++** workload. That includes MSVC and the Windows 10/11 SDK, which
   GPUI needs to compile its shaders.
3. From a terminal in this folder, build it:

   ```powershell
   cargo build --release
   ```

   The app is `target\release\remote-iterm.exe`. It's a single file, with no
   installer, and you can copy it anywhere. Windows SmartScreen may warn the
   first time because the file isn't signed. Click *More info → Run anyway*.

If the build can't find the shader compiler (`fxc.exe`), run it from the
**Developer PowerShell for VS 2022** shortcut, which puts the SDK tools on
`PATH`.

### Installer

To build a setup program instead of a bare exe, install
[cargo-packager](https://github.com/crabnebula-dev/cargo-packager) once, then
package from this folder:

```powershell
cargo install cargo-packager --locked
cargo packager --release
```

This builds the app and writes `target\release\remote-iterm_0.1.0_x64-setup.exe`.
The first run downloads the NSIS installer tools, so it needs the internet.

The installer doesn't need admin rights. It installs to
`%LOCALAPPDATA%\Remote iTerm`, adds a Start menu entry and, if you tick the box
on the last page, a desktop shortcut. To update, run a newer installer over
the old one. To remove the app, use **Settings → Apps**. Your settings in
`%APPDATA%\remote-iterm` are kept either way.

## First run

The app opens on an access-key screen. Paste the server's QR link (the
`…#key=…` URL) or the bare key with **Ctrl+V**, then press **Enter**.

By default it connects to `https://term.jiye.ca:8443`. Settings live in
`%APPDATA%\remote-iterm\desktop.json`:

```json
{
  "server_url": "https://term.jiye.ca:8443",
  "key": "…",
  "window": { "x": 100, "y": 80, "width": 1100, "height": 700, "maximized": false },
  "font_size": 13
}
```

The window's size and position, and the zoom level, are saved there
automatically.

On Windows the tab bar is the title bar. Drag its empty space to move the
window, and double-click it to maximize.

## Keys

| Keys | What they do |
|---|---|
| Any key, including Ctrl+W, Ctrl+R, Ctrl+T | Sent to the terminal |
| Ctrl+1 … Ctrl+8 | Switch to tab N |
| Ctrl+9 | Switch to the last tab |
| Ctrl+V | Paste text. An image, including a screenshot, goes to the Mac clipboard, and Ctrl+V is sent so Claude Code picks it up. With nothing to paste, nothing is sent |
| Ctrl+C | Copy the selected text. With nothing selected it sends ^C |
| Ctrl+Shift+C | Copy the selected text |
| Ctrl+click on a link | Open the link in your browser |
| Ctrl+= and Ctrl+−, or Ctrl+wheel | Make the text bigger or smaller |
| Ctrl+0 | Reset the text size |

In apps that use the mouse, such as Claude Code, clicks, drags and the wheel
go to the app. Everywhere else, dragging selects text and the wheel scrolls,
loading older output as you reach the top.

The **Mac clipboard** button in the tab bar copies whatever is on the Mac's
clipboard (for example after `pbcopy`) to this PC.

## Developing on the Mac

The app builds and runs on macOS too. Two things are specific to this Mac:

- Homebrew's `rustc` (1.85) is earlier on `PATH` than rustup's and is too old,
  so put the pinned toolchain first:

  ```sh
  export PATH="$HOME/.rustup/toolchains/1.99.1-aarch64-apple-darwin/bin:$PATH"
  ```

- GPUI's `runtime_shaders` feature is enabled on macOS, because Xcode's Metal
  Toolchain isn't installed.

To run against the local server without the key screen:

```sh
REMOTE_ITERM_URL=http://localhost:7291 \
REMOTE_ITERM_KEY="$(cat ~/Library/Application\ Support/remote-iterm/access-key)" \
cargo run
```

`cargo test` runs the unit tests and headless UI tests. The live tests against a
running server are marked `#[ignore]`:

- `cargo test connection -- --ignored` checks the connection.
- `cargo test live_mac_clipboard -- --ignored` checks the clipboard button.
  Set `REMOTE_ITERM_E2E_RESTART=1` to also restart the service and check
  reconnecting.
- `live_typing_prediction_and_password_safety` needs a scratch iTerm window,
  passed in as `REMOTE_ITERM_E2E_WINDOW` / `REMOTE_ITERM_E2E_SESSION`. It types
  into that window, so never point it at a real session.
