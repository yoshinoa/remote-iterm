# Native desktop client (GPUI) with predictive typing — design

Date: 2026-11-04
Status: draft, awaiting approval

## Why

The user reaches their Mac's iTerm from a Windows PC through the web client,
in Firefox at `https://term.jiye.ca:8443`. Caddy routes `/socket.io/*` to the
Python server on :7291 and everything else to Vite on :7292. Most of the time
they're typing into Claude Code.

Next to Windows Terminal, typing feels noticeably slower and characters show
up at uneven times. The goal is a **native Windows app that feels like a
native terminal**. That means its own window, every shortcut reaching the
terminal, Ctrl+1–9 for tabs, and typed characters appearing instantly through
prediction. The user chose a native GPUI rewrite over wrapping the web page in
Tauri, and accepted that it's a rewrite.

Measured on 2026-11-04 by typing into a visible zsh window over the live
server:

| Stage | Before | After the throttle fix (done) |
|---|---|---|
| iTerm + shell echo (send → iTerm reports the change) | ~13 ms (3–20) | same |
| `read_content()` | ~3 ms | same |
| Server waits before sending | 50 ms, every time | 0 ms after a quiet spell |
| End to end on the Mac (no network) | ~74 ms | ~21 ms (9–25) |
| Network to the PC (ping) | ~4.5 ms | same |

The throttle fix is already in `stream_session`, with tests in
`StreamThrottleTest`.

What's left:
- The delay itself, about 25 ms.
- Jitter from iTerm's refresh cadence: 3–20 ms, which can't be fixed on the
  Mac.
- Jitter when something else on screen is changing. Claude Code's spinner can
  add 0–50 ms to a keystroke.
- The browser engine's own input-to-paint time.

Prediction hides the first three. A native renderer removes the last.

Also observed: iTerm updates a tab that isn't selected on the Mac only about
once every 0.8 s. Selecting a tab in the client already activates it on the
Mac (`focus`), so the tab you're viewing is normally live.

## Goals

1. **A native client in Rust with GPUI** (Zed's UI toolkit), in `desktop/`.
   It targets Windows and also runs on macOS for development.
2. **Predictive typing in that client.**
3. **Two small server changes:** keystrokes skip the bundling wait, and the
   content payload carries the session's grid size.

## Version 1 scope (agreed with the user)

**In:**
- **Connecting:** connect with the access key and reconnect on its own.
- **Tabs:** a tab bar of iTerm tabs. Click one or press Ctrl+1–9 to switch.
  Open and close tabs.
- **Panes:** one pane shown at a time, with a pane switcher.
- **Typing:** goes straight to the terminal with prediction. Every Ctrl key
  reaches the terminal, including Ctrl+W.
- **Pasting:** Ctrl+V pastes text. An image is handed to the Mac pasteboard
  the way the web client does it.
- **Mouse:** clicks, drags and the wheel are forwarded to apps that ask for
  the mouse, such as Claude Code. Elsewhere, local text selection and copying.
  Links open.
- **History:** scroll back, loading older pages on demand.
- **Mac clipboard:** a button copies the Mac clipboard to the PC clipboard.
- **Look:** monospace text drawn by the GPU, in the terminal's own colours.

**Out** (the user confirmed): the two-tabs-at-once split screen, broadcast
typing to several tabs, renaming sessions, the "long command finished" alert,
and the ESC / ^C quick-action bar. Also out: the pane map overlay and the
spatial window map.

## Non-goals

- Changing the web client. It stays as it is for phone and browser use, and it
  doesn't get prediction.
- Automatic clipboard sync. The user explicitly declined it.
- Tray icon, notifications, launch at startup, auto-update, an installer, and
  code signing. A plain `.exe` is enough.
- Reproducing iTerm's split layout side by side.
- Changing the content protocol to send diffs instead of snapshots.
- Fixing iTerm's slow updates for unselected tabs.

## Anti-goals

These would make the work a failure even if it technically works:

- A wrong character staying on screen after the real screen has arrived.
- Password characters flashing on screen at a `sudo` or `ssh` prompt.
- Visible flicker in apps that don't echo keys, such as vim in normal mode or
  a TUI's menu keys.
- Prediction delaying a key being sent, or typing feeling worse than in the
  browser in any situation.
- Input going to any session other than the one on screen.

## Part 1 — Server changes

### Keystrokes skip the bundling wait

When `sendKeys` arrives for a session, that session's stream reads and sends
changes without the `STREAM_MIN_INTERVAL` wait for a short window afterwards
(**my call: 100 ms**). A window is used rather than "the next change" because
another change, such as a spinner frame, can land before the echo and would
use up a single bypass. Outside the window, the existing leading-edge throttle
applies.

### Grid size in content

Add the session's `columns` and `rows` to every `content` and
`historyContent` payload, from `session.grid_size`. The web client ignores the
extra fields.

Tests for both go in `server/test_server.py`, in the style of
`StreamThrottleTest`.

## Part 2 — The native client (`desktop/`)

### Stack

- **UI:** Rust with GPUI, pinned to an exact version or commit.
- **Socket.IO v4 client:** for example `rust_socketio` with TLS. If the crate
  is unworkable, write a small Engine.IO v4 WebSocket client. Builder's call.
- **Platforms:** must build and run on macOS, which is where development and
  automated testing happen, and on Windows (MSVC). The Mac currently has Rust
  1.85 through Homebrew, plus rustup and Xcode with Metal. GPUI may need a
  newer Rust.

### Talking to the server

The existing Socket.IO contract (see `docs/ARCHITECTURE.md`) is used
unchanged, apart from Part 1:

- **Connecting:** connect with `auth: {key}`.
- **Events received:** `state`, `content` and `historyContent`.
- **Events sent:**
  - `watch {sessionIds: [shown session]}`
  - `getContent {sessionId}`
  - `getEarlierContent {sessionId, beforeLine}`
  - `sendKeys {sessionId, keys}`
  - `pasteImage {sessionId, data: dataURL}`
  - `clipboardText` (ack)
  - `clipboardCount` and `clipboardSince {count}` (acks)
  - `newTab`
  - `closeTab {tabId}`
  - `focus {windowId, tabIndex}`
  - `ping`
- **Content format:** content lines are runs (`t` text, `f` fg, `g` bg, `b`
  bold, `d` dim, `u` link, `c` cursor marker), with the pane's default `fg`
  and `bg`.
- **Merging updates:** follow the web client's `mergeStyledContent`. A live
  update replaces the tail, and older pages prepend.

### Configuration

- **Server URL:** defaults to `https://term.jiye.ca:8443`.
- **Access key:** the first-run screen accepts either the key or the full QR
  link (`…#key=…`). A rejected key brings the screen back.
- **Where it's stored:** the user's config directory, which is
  `%APPDATA%\remote-iterm\` on Windows. It's a plain file, the same level of
  trust as the browser's localStorage today.

### Following the Mac

- The selected window defaults to the front window.
- Like the web client, the app follows tab and focus changes made on the Mac.
  An explicit pane choice in the app is kept.
- Selecting a tab sends `focus` so that the tab is live on the Mac.
- If there's more than one iTerm window, a simple window picker appears.

### Tab bar

- **Contents:** the selected window's tabs, by title.
- **Clicking a tab:** selects it and sends `focus`.
- **Closing and opening:** each tab has a close button, hidden when it's the
  only tab, which sends `closeTab {tabId}`. A new-tab button sends `newTab`.
- **Ctrl+1–8** select tab N. **Ctrl+9** selects the last tab. These are the
  browser and Windows Terminal conventions, and my reading of the user's
  "ctrl+1 2 3 etc".
- **Mac clipboard button:** copies the Mac clipboard to the PC clipboard,
  showing success, or nothing to copy, the same way as the web client's
  button.
- **Pane switcher:** shown when the tab has more than one pane. Choosing a
  pane changes which session is shown and watched. It doesn't change the Mac's
  focused pane.

### Terminal view

- **Rows:** one terminal row per content line, with **no soft wrap**, so a
  cell on screen maps directly to a terminal cell. That mapping is what
  prediction and mouse forwarding need. If the window is narrower than the
  session's `columns`, overflowing content is handled however the builder
  sees fit.
- **Styling:** runs are drawn with their colours, bold and dim. Links show an
  underline on hover.
- **Cursor:** drawn as a block at the cursor cell.
- **Font:** Cascadia Mono on Windows and Menlo on macOS (my call).
- **Following output:** the view follows the live tail while scrolled to the
  bottom. Otherwise it keeps the reader's position.
- **Typing while scrolled up:** a key that's sent to the terminal jumps the
  view back to the live tail (my call, standard terminal behaviour).
- **Caching:** content is cached per session, so switching is instant.
  `getContent` is sent on every switch.

### Keyboard

Port the web client's `encodeKeyEvent`:
- Ctrl+letter becomes a C0 control code.
- Alt prefixes ESC.
- Arrows, Home/End, PgUp/PgDn, Insert/Delete and F-keys become CSI or SS3
  sequences, with the `;2` Shift modifier.
- Enter is `\r`, Shift+Enter is `\n`, Backspace is `\x7f`, Shift+Tab is
  `ESC[Z`.

App shortcuts take precedence over sending keys:

- **Ctrl+1–9:** tabs, as above.
- **Ctrl+V:**
  - an image on the PC clipboard → `pasteImage`
  - text → `sendKeys` with the web client's `pasteSequence` rules (bracketed
    paste for multi-line or long text)
  - an empty clipboard → send `\x16`
- **Ctrl+C:** copies when there's a local selection, and clears it. Otherwise
  it sends `\x03`. This is Windows Terminal's behaviour.
- **Ctrl+Shift+C:** always copies the selection.

### Mouse

When the content has `mouse: true`, the app has asked for the mouse:

- **Buttons and drags:** left press, drag and release are sent as SGR mouse
  reports at the pointer cell, using the web client's `handleTerminalMouseDown`
  encoding (Shift = 4, Ctrl = 16, motion = 32). The row is relative to
  `screenFirstLine`.
- **Copying from the app:** before the press, ask `clipboardCount`. After the
  release, ask `clipboardSince`. If the app copied text on the Mac, put it on
  the PC clipboard.
- **Wheel:** sends SGR wheel reports (64 up, 65 down) at the pointer cell. The
  coordinates must be real, because TUIs like opencode hit-test them.

Otherwise the mouse is local:

- **Wheel:** scrolls history. Reaching the top requests the previous page.
- **Drag:** selects text.
- **Ctrl+click on a link:** opens it in the default browser.

### Predictive typing

The logic lives in its own module with no UI, and is unit-tested.

**What gets predicted.** Printable characters with no Ctrl, Alt or Meta
(Shift is fine), and Backspace. Backspace erases the cell left of the cursor
and moves the cursor left. If that cell holds a pending predicted character,
it just removes it.

**What resets prediction.** Everything else clears all pending predictions and
starts a new **epoch**. That covers:
- Enter, Tab, Escape, arrows and the other special keys
- Ctrl and Alt combinations
- pastes
- forwarded mouse input
- switching tab, pane or session

**Where predictions are drawn.**
- **Position:** at the cursor cell from the latest live content.
- **Overwrite, not insert:** predicted characters overwrite cells from the
  cursor rightwards, and the drawn cursor moves past them.
- **Look:** they look like ordinary typed text, with no underline or dimming.
  Use the style of the text just left of the cursor, without dim. Fall back to
  the default foreground.

**When not to predict.** In these cases the key is still sent, and the real
screen does the echo:
- **Real text to the right of the cursor.** If the cursor line has non-blank
  text to the right of the cursor that isn't dim, it's mid-line editing.
  Insert vs overwrite is unknown. Dim text counts as blank, because Claude
  Code shows its prompt suggestion as dim text after the cursor (seen in live
  sessions).
- **End of the line.** The next character would go past column `columns`.
- **Not at the live screen.** The view is scrolled into history, or the cursor
  isn't in the loaded lines.

**Confirming and undoing.** Every prediction records its absolute line and
column, plus the expected character (Backspace expects a blank). Each new live
snapshot is checked against the pending predictions in order:
- **Confirmed:** the cell now shows the expected character. Drop the
  prediction and mark the epoch confirmed.
- **Still pending:** the cell is unchanged from what it was when the
  prediction was made. Keep drawing it.
- **Contradicted:** anything else. Drop all pending predictions and start a
  new epoch. The real screen shows as is, and that's the undo.

A prediction not confirmed within **1 s (my call)** is dropped the same way.

**Epoch rule** (mosh's approach, agreed with the user). A new epoch starts at
connect, on every reset key, and on every contradiction or timeout.
- **Hidden at first:** predictions in a new epoch are tracked but not drawn
  until one of them is confirmed.
- **Then instant:** once one is confirmed, later ones are drawn immediately.
- **Why:** it keeps password prompts and non-echoing keys from ever being
  drawn.
- **Cost:** the first character after Enter appears at real-echo speed.

**Speed.**
- **Send first:** the key is sent before any prediction work.
- **Next frame:** a predicted character is painted in the first frame after
  the key event.

### Window and connection status

- **Window:** titled "Remote iTerm", with its own icon. It remembers its size,
  position and maximized state in the config directory.
- **Disconnected:** a status overlay says it's reconnecting, retries forever
  with backoff, and has a "retry now" button.

### Build and docs

- **`desktop/README.md`:** prerequisites for Windows (rustup, MSVC Build Tools
  and the Windows SDK, plus whatever GPUI needs), then
  `cargo build --release`, and where the `.exe` ends up.
- **`docs/ARCHITECTURE.md`:** gains a short section on the native client.

## Verification

- **Server:** unit tests for the keystroke window and the grid-size fields,
  and the full Python suite. Rerun the latency probe: keystroke-to-echo should
  stay around 21 ms even while another part of the screen changes every frame.
- **Client unit tests:**
  - prediction (predict, confirm, contradict, timeout, epoch gating, the
    no-predict cases)
  - key encoding
  - paste sequence
  - SGR mouse encoding
  - content merging
- **Client on the Mac, against the live server:**
  - typing in Claude Code and zsh appears instantly after the first character
  - a `sudo` password prompt never shows typed characters
  - vim normal-mode keys don't flicker
  - mid-line edits fall back to the real echo
  - wheel and drag work in Claude Code
  - scrollback pages load
  - tab and pane switching, and Ctrl+1–9, work
  - text and image paste work
  - the Mac clipboard button works
  - stopping the server shows the reconnect overlay, and it recovers
- **Windows (the user):** builds it and repeats the Mac checks, plus Ctrl+W
  deleting a word in zsh and the window remembering its place.

## Left to the builder

- Crate layout.
- The Socket.IO client crate or a hand-written Engine.IO client.
- The GPUI version.
- How to render efficiently from 250-line snapshots.
- Horizontal overflow handling.
- Visual design of the tab bar, pane switcher, key screen and overlays, kept
  plain and dark like the web client.
- Whether to show a latency figure.
- Whether to add a keyboard shortcut for the Mac clipboard button.
- The icon.

## Calls made on the user's behalf

- Keystroke window: 100 ms.
- Prediction timeout: 1 s.
- Predicted text looks identical to real text.
- No prediction for mid-line edits or past the last column.
- Backspace is predicted.
- No soft wrap.
- Typing jumps to the live tail.
- Fonts: Cascadia Mono on Windows and Menlo on macOS.
- Ctrl+1–8 select tab N, and Ctrl+9 selects the last tab.
- Ctrl+C copies when there's a selection.
- Ctrl+click opens links.
- The app follows focus changes made on the Mac.
- A plain `.exe` with no installer.
- The default URL is `https://term.jiye.ca:8443`.
- The web client is left unchanged.
