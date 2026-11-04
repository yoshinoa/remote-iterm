# Native Desktop Client (GPUI) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a native Rust/GPUI desktop client for remote-iterm, mainly for
Windows, with predictive typing. Also add two server changes: keystrokes skip
the bundling wait, and content carries the grid size.

**Architecture:**
- **The client:** a new `desktop/` crate.
- **Pure, unit-tested modules:** protocol types and merging, key and mouse
  encoding, prediction, config, the selection model and geometry. None of
  these depend on GPUI.
- **The UI:** thin GPUI views over those modules.
- **Networking:** runs on its own tokio thread. It talks to the UI over
  `futures` channels.
- **The server:** two small, test-first changes to `server/server.py`.

**Tech Stack:**
- Rust 1.99.1, `gpui =0.2.2`, `rust_socketio =0.6.0` (async).
- tokio, serde/serde_json, futures, unicode-width, dirs, base64.
- Python 3 / python-socketio for the server.

**Spec:** `docs/superpowers/specs/2026-11-04-predictive-typing-and-desktop-app-design.md`

## Global Constraints

### Toolchain and dependencies

- **Rust:** `desktop/rust-toolchain.toml` pins `channel = "1.99.1"`.
- **Running cargo on this Mac:** Homebrew's `rustc` 1.85 is earlier on PATH and
  too old, so every cargo command below means:
  `PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" cargo …`
  run in `desktop/`. That toolchain is 1.99.1.
- **Pinned crates:** `gpui = "=0.2.2"` and
  `rust_socketio = { version = "=0.6.0", features = ["async"] }`.
- **macOS shaders:** on macOS only, enable gpui's `runtime_shaders` feature
  through a `[target.'cfg(target_os = "macos")'.dependencies]` entry. This
  Mac's Xcode has no Metal Toolchain, and the default feature set fails to
  build.

### Fixed values from the spec

- **Default server URL:** `https://term.jiye.ca:8443`. The config file lives at
  `dirs::config_dir()/remote-iterm/desktop.json`.
- **Window:** title `Remote iTerm`.
- **Fonts:** `Cascadia Mono` on Windows and `Menlo` on macOS.
- **Timings:** the keystroke window is `0.1` s and the prediction timeout is
  `1` s.
- **Paste:** `PLAIN_PASTE_MAX_BYTES = 512`.
- **Ctrl+1–9:** Ctrl+1–8 select tab N, Ctrl+9 selects the last tab, and these
  only exist in the app.
- **SGR mouse codes:** Shift = 4, Ctrl = 16, motion = 32, wheel up = 64, wheel
  down = 65.

### Protocol and scope

- **No server protocol changes** beyond the two additive fields in Task 2. The
  web client must keep working unchanged.
- **Out of scope:**
  - split screen
  - broadcast
  - renaming sessions
  - the long-command alert
  - the quick-action bar
  - the pane map
  - automatic clipboard sync
  - an installer or code signing

### Git

- **Branch:** work on a branch `native-desktop-client`, never on `main`.
- **Commit messages:** plain, with no `Co-Authored-By` trailer or any other
  tool attribution (the user's CLAUDE.md).
- **Existing work first:** the working tree starts with uncommitted user work
  in `server/server.py`, `server/test_server.py`, `client/src/App.tsx`,
  `.gitignore` and `iterm-server`. Pre-flight commits that as one commit before
  Task 1, so later task commits contain only their own changes.

## Review Focus

These are failure modes the spec implies but tasks wouldn't naturally test.
Each has a pinned test in the task named.

1. **Fast typing outruns the echo.** A snapshot arrives with only the first of
   several typed characters. Expected: the rest stay drawn, with no reset and
   no flicker. Test `burst_keeps_unconfirmed_predictions` in Task 6.
2. **Claude Code's dim suggestion text sits where you type.** A dim cell that
   happens to equal the typed character must not count as confirmation.
   Expected: the prediction stays pending until real text arrives. Test
   `dim_cell_does_not_confirm` in Task 6.
3. **Content for a different session arrives while predictions are pending,**
   for example a background session or a just-switched pane. Expected: it's
   ignored by the predictor. Test `other_session_content_is_ignored` in Task 6.
4. **The server restarts (reconnect) while the app is open.** Expected: on
   reconnect the app re-sends `watch` and `getContent` for the shown session,
   and the screen comes back. Test `connected_rewatches_shown_session` in
   Task 8, plus a manual check in Task 14.
5. **AltGr characters on Windows layouts** arrive as Ctrl+Alt with a typed
   character. Expected: the character is sent, not a control code. Test
   `altgr_sends_typed_character` in Task 4.

---

### Pre-flight

- [ ] `git status` on `main`. Then `git checkout -b native-desktop-client`.
- [ ] Commit the existing uncommitted work as is. That covers the clipboard
  button, the throttle fix, and the user's earlier edits to `.gitignore`,
  `iterm-server`, `server/*` and `client/src/App.tsx`. Leave
  `package-lock.json` and `scripts/` untracked unless the user says otherwise.
  Message: `Add Mac clipboard button and send first screen change immediately`.
- [ ] Run `server/.venv/bin/python -m unittest server.test_server server.test_auth`.
  Expected: `OK` (42 tests).

### Task 1: Server — keystrokes skip the bundling wait

**Files:**
- Modify: `server/server.py` (`stream_session`, `on_send_keys`, and the
  constants near `STREAM_MIN_INTERVAL`)
- Test: `server/test_server.py` (extend `StreamThrottleTest`)

**Interfaces:**
- Produces:
  - `KEYSTROKE_WINDOW = 0.1`
  - `keystroke_at: dict[str, float]` (session id → loop time)
  - `note_keystroke(session_id: str) -> None`
  - `on_send_keys` calls `note_keystroke(session.session_id)` after sending.

- [ ] **Step 1: Write the failing tests** in `StreamThrottleTest`. Also clear
  `keystroke_at` in `asyncTearDown`.

```python
async def test_change_soon_after_a_keystroke_skips_the_wait(self):
    await asyncio.sleep(self.INTERVAL * 1.5)
    self.session.notifications.put_nowait(None)
    await asyncio.sleep(0.01)
    server_module.note_keystroke("session-1")
    before = len(self.sent)
    changed_at = asyncio.get_running_loop().time()
    self.session.notifications.put_nowait(None)
    await asyncio.sleep(self.INTERVAL / 4)
    self.assertEqual(len(self.sent), before + 1)
    self.assertLess(self.sent[-1] - changed_at, self.INTERVAL / 4)

async def test_keystroke_window_expires(self):
    with mock.patch.object(server_module, "KEYSTROKE_WINDOW", 0.05):
        await asyncio.sleep(self.INTERVAL * 1.5)
        self.session.notifications.put_nowait(None)
        await asyncio.sleep(0.01)
        first = self.sent[-1]
        server_module.note_keystroke("session-1")
        await asyncio.sleep(0.08)
        self.session.notifications.put_nowait(None)
        await asyncio.sleep(self.INTERVAL * 1.5)
    self.assertGreaterEqual(self.sent[-1] - first, self.INTERVAL * 0.95)
```

- [ ] **Step 2: Run the tests and confirm they fail.**
  `server/.venv/bin/python -m unittest server.test_server.StreamThrottleTest -v`.
  Expected: the first new test FAILS (it waits a full interval), and the
  second errors on the missing `note_keystroke`.
- [ ] **Step 3: Implement.** `note_keystroke` stores
  `asyncio.get_running_loop().time()`. In `stream_session`, skip the
  `asyncio.sleep(wait)` when
  `loop.time() - keystroke_at.get(session_id, -inf) <= KEYSTROKE_WINDOW`.
  `on_send_keys` calls `note_keystroke` after `async_send_text`.
- [ ] **Step 4: Run the tests and confirm they pass.** Run the full suite:
  `server/.venv/bin/python -m unittest server.test_server server.test_auth`.
  Expected: `OK`.
- [ ] **Step 5: Commit.** `git commit -am "Read keystroke echoes without the bundling wait"`

### Task 2: Server — grid size in content

**Files:**
- Modify: `server/server.py` (`read_content`)
- Test: `server/test_server.py`

**Interfaces:**
- Produces: `_grid_fields(session) -> dict`, which returns
  `{"columns": int, "rows": int}` from `session.grid_size.width/height`, or `{}`
  if that's unavailable. Both return paths of `read_content` merge it in, so
  `content` and `historyContent` payloads carry `columns` and `rows`.

- [ ] **Step 1: Write the failing test** `GridFieldsTest`:
  - `_grid_fields(SimpleNamespace(grid_size=SimpleNamespace(width=120, height=40)))`
    returns `{"columns": 120, "rows": 40}`.
  - `_grid_fields(SimpleNamespace())` returns `{}`.
- [ ] **Step 2: Run it and confirm it fails.**
  `…unittest server.test_server.GridFieldsTest`. Expected: `ImportError` or
  `AttributeError` on `_grid_fields`.
- [ ] **Step 3: Implement** `_grid_fields`, and spread it into both dicts
  returned by `read_content`.
- [ ] **Step 4: Run the full suite and confirm it passes.**
- [ ] **Step 5: Verify live.** Restart the service:
  `launchctl kickstart -k gui/$(id -u)/ca.jiye.remote-iterm`. Then, with an
  `AsyncClient` from `server/.venv` connected to `localhost:7291`, using the key
  from `~/Library/Application Support/remote-iterm/access-key`, `emit
  getContent` for any session. Expected: the `content` payload has integer
  `columns` and `rows`.
- [ ] **Step 6: Commit.** `"Include session grid size in content payloads"`

### Task 3: Desktop crate scaffold and protocol types

**Files:**
- Create: `desktop/Cargo.toml`, `desktop/rust-toolchain.toml`,
  `desktop/.gitignore` (`/target`), `desktop/src/main.rs` (declares the
  modules, plus a placeholder `fn main` until Task 10), and
  `desktop/src/protocol.rs`

**Interfaces:**
- Produces (in `protocol.rs`, all `Clone + Debug + PartialEq + Deserialize`,
  camelCase):
  - `Run { t: String, f: Option<String>, g: Option<String>, b: bool, d: bool, c: bool, u: Option<String> }`
    The bools and `t` are `#[serde(default)]`.
  - `Content { lines: Vec<Vec<Run>>, fg: String, bg: String, first_line: i64, available_first_line: i64, terminal_end: i64, is_latest: bool, mouse: bool, screen_first_line: Option<i64>, columns: Option<u32>, rows: Option<u32> }`
  - `SessionContent { session_id: String, #[serde(flatten)] content: Content }`
  - `WindowState { id: String, is_front: bool, tabs: Vec<TabState> }`
  - `TabState { index: u32, id: String, tab_id: String, title: String, is_selected: bool, current_session_id: String, sessions: Vec<SessionInfo> }`
  - `SessionInfo { id: String, name: String, rect: Option<Rect> }`
  - `Rect { x: f32, y: f32, w: f32, h: f32 }`
  - `Cell { ch: char, dim: bool }`
  - `merge_content(current: Option<&Content>, incoming: Content) -> Content`
  - `Content::cursor(&self) -> Option<(i64, usize)>`, giving the absolute
    line and the character index within the line text.
  - `Content::cell(&self, line: i64, col: usize) -> Option<Cell>`, which is
    `None` past the end of the text.
  - `Content::line_text(&self, line: i64) -> Option<String>`

- [ ] **Step 1: Scaffold.**
  - Install the toolchain: `~/.cargo/bin/rustup toolchain install 1.99.1 --profile minimal`.
    This is so `rust-toolchain.toml` resolves for anyone using rustup proxies.
  - Write `Cargo.toml`: package `remote-iterm-desktop`, bin name
    `remote-iterm`, edition 2024, with the dependencies from Global
    Constraints plus `tokio` (rt-multi-thread, macros, sync), `serde`
    (derive), `serde_json`, `futures`, `unicode-width`, `dirs` and `base64`.
- [ ] **Step 2: Write the failing tests** in `protocol.rs` `#[cfg(test)]`:
  - `deserializes_server_content`: a JSON literal shaped like a real payload,
    with `sessionId`, runs carrying `f/d/c`, `firstLine: 1000`, `columns: 100`.
    It parses into `SessionContent`, `columns == Some(100)`, and the second
    run has `c == true`.
  - `deserializes_state_without_optional_fields`: a tab missing `title` and a
    session missing `rect` parse with defaults.
  - `cursor_reports_absolute_line_and_char_index`: lines
    `[[{t:"x"}],[{t:"❯ "},{t:"",c:true}]]` with `first_line 50` give
    `cursor() == Some((51, 2))`.
  - `cell_reads_across_runs_and_dim`: `[{t:"❯ "},{t:"",c:true},{t:"fix",d:true}]`
    gives `cell(l,2) == Some(Cell{ch:'f',dim:true})` and `cell(l,9) == None`.
  - Merge cases. These are a port of the web client's `mergeStyledContent`
    (`client/src/App.tsx`), so the semantics are identical:
    - `merge_without_current_returns_incoming`
    - `merge_live_tail_replaces_tail_and_strips_old_cursor`: current lines
      1000..1010 with a cursor at 1009, and an incoming live update for
      1005..1012 with the cursor at 1011. The result spans 1000..1012, there's
      exactly one cursor at 1011, and lines 1000..1004 come from current.
    - `merge_gap_returns_incoming`: current 1000..1010, incoming 1020..1030.
    - `merge_history_prepends`: current is live 1000..1010, incoming is
      `is_latest:false`, 900..1000. The result spans 900..1010, and `is_latest`
      stays true.
- [ ] **Step 3: Run the tests and confirm they fail.** `cargo test protocol`.
  Expected: compile errors or failures for the missing items.
- [ ] **Step 4: Implement** the types and functions. Character indices use
  `chars()`, not bytes.
- [ ] **Step 5: Run the tests and confirm they pass.** `cargo test protocol`.
  Expected: all pass.
- [ ] **Step 6: Commit.** `"Add desktop crate with protocol types and content merging"`

### Task 4: Key encoding and paste

**Files:**
- Create: `desktop/src/keys.rs`

**Interfaces:**
- Produces:
  - `pub struct KeyInput<'a> { pub key: &'a str, pub key_char: Option<&'a str>, pub ctrl: bool, pub alt: bool, pub shift: bool, pub platform: bool }`
    The field names mirror `gpui::Keystroke`/`Modifiers`. GPUI key names are
    lowercase: `enter`, `backspace`, `tab`, `escape`, `up`/`down`/`left`/`right`,
    `home`, `end`, `pageup`, `pagedown`, `insert`, `delete`, `f1`…`f12`,
    `space`.
  - `pub fn encode_key(k: &KeyInput) -> Option<String>`
  - `pub fn paste_sequence(text: &str) -> String`
  - `pub const PLAIN_PASTE_MAX_BYTES: usize = 512`

- [ ] **Step 1: Write the failing tests.** They port the web client's
  `encodeKeyEvent` and `pasteSequence`. Each is one assertion:

| Test | Input | Expected |
|---|---|---|
| `printable_uses_key_char` | `a`, key_char `A`, shift | `"A"` |
| `space` | `space` | `" "` |
| `ctrl_letters_are_c0` | ctrl `w` / ctrl `c` | `"\x17"` / `"\x03"` |
| `ctrl_punctuation` | ctrl `[` / ctrl `space` | `"\x1b"` / `"\x00"` |
| `ctrl_digit_is_unhandled` | ctrl `1` | `None` |
| `alt_prefixes_escape` | alt `b` | `"\x1bb"` |
| `altgr_sends_typed_character` | ctrl+alt `q`, key_char `@` | `"@"` |
| `enter_and_shift_enter` | `enter` / shift `enter` | `"\r"` / `"\n"` |
| `editing_keys` | `backspace`, `tab`, shift `tab`, `escape` | `"\x7f"`, `"\t"`, `"\x1b[Z"`, `"\x1b"` |
| `arrows_with_shift` | `up` / shift `up` | `"\x1b[A"` / `"\x1b[1;2A"` |
| `navigation` | `home`, `end`, `pageup`, `delete` | `"\x1b[H"`, `"\x1b[F"`, `"\x1b[5~"`, `"\x1b[3~"` |
| `function_keys` | `f1` / `f5` | `"\x1bOP"` / `"\x1b[15~"` |
| `platform_modifier_is_unhandled` | platform `c` | `None` |
| `short_paste_is_plain` | `"ls -la"` | `"ls -la"` |
| `multiline_paste_is_bracketed` | `"a\r\nb\x1b"` | `"\x1b[200~a\nb\x1b[201~"` |
| `long_paste_is_bracketed` | 513 × `"x"` | starts with `"\x1b[200~"` |

- [ ] **Step 2: Run the tests and confirm they fail.** `cargo test keys`.
- [ ] **Step 3: Implement.** Named keys are checked before printable ones,
  because GPUI gives `enter` a `key_char` of `"\n"`. When ctrl+alt carries a
  `key_char`, the character is sent as is.
- [ ] **Step 4: Run the tests and confirm they pass.** `cargo test keys`.
- [ ] **Step 5: Commit.** `"Add terminal key encoding and paste sequences"`

### Task 5: Mouse encoding and geometry

**Files:**
- Create: `desktop/src/mouse.rs`, `desktop/src/geometry.rs`

**Interfaces:**
- Produces:
  - `mouse::{MOD_SHIFT=4, MOD_CTRL=16, MOTION=32, WHEEL_UP=64, WHEEL_DOWN=65}`
  - `mouse::modifier_bits(shift: bool, ctrl: bool) -> u8`
  - `mouse::sgr(code: u8, col: u32, row: u32, release: bool) -> String`
  - `mouse::terminal_row(line: i64, screen_first_line: i64) -> u32`, which is
    `max(1, line - screen_first_line + 1)`, matching the web client's
    `rowOffset`.
  - `geometry::point_to_cell(x: f32, y: f32, cell_width: f32, line_height: f32, scroll_top: f32, first_line: i64) -> (i64, usize)`,
    giving the absolute line and the 0-based column. The coordinates are
    relative to the text area's origin.
  - `geometry::selection_text(content: &Content, a: (i64, usize), b: (i64, usize)) -> String`.
    The endpoints may be in either order, the range is inclusive of `a` and
    exclusive of `b`, lines are joined with `\n`, and trailing spaces are
    trimmed per line.

- [ ] **Step 1: Write the failing tests.**
  - `sgr_press_and_release`: `sgr(0,5,3,false) == "\x1b[<0;5;3M"` and
    `sgr(0,5,3,true) == "\x1b[<0;5;3m"`.
  - `modifiers_and_motion`: `MOTION | modifier_bits(true, true) == 52`.
  - `terminal_row_offsets_and_clamps`: `terminal_row(1040,1000) == 41` and
    `terminal_row(990,1000) == 1`.
  - `point_maps_to_cell`: `point_to_cell(25.0, 37.0, 8.0, 16.0, 32.0, 1000) == (1004, 3)`.
  - `selection_spans_lines`: on a 3-line content, `((l0,2),(l1,3))` gives
    `"<rest of l0>\n<first 3 of l1>"`, and reversed endpoints give the same.
- [ ] **Step 2: Run the tests and confirm they fail.** `cargo test mouse geometry`.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests and confirm they pass.**
- [ ] **Step 5: Commit.** `"Add SGR mouse encoding and cell geometry"`

### Task 6: Predictive typing engine

**Files:**
- Create: `desktop/src/prediction.rs`

**Interfaces:**
- Consumes: `protocol::{Content, Cell}`
- Produces:
  - `pub const PREDICTION_TIMEOUT: Duration = Duration::from_secs(1)`
  - `pub enum PredictKey { Char(char), Backspace }`
  - `pub struct OverlayCell { pub line: i64, pub col: usize, pub ch: char }`
  - `pub struct Predictor`, with:
    - `new() -> Self`
    - `reset(&mut self)`
    - `on_key(&mut self, session_id: &str, key: PredictKey, screen: &Content, now: Instant)`
    - `on_content(&mut self, session_id: &str, screen: &Content, now: Instant)`
    - `tick(&mut self, now: Instant)`
    - `overlay(&self) -> Vec<OverlayCell>`
    - `cursor(&self) -> Option<(i64, usize)>`

**The algorithm.** The tests don't fully determine it, so here it is.

*State:*
- `session: Option<String>`
- `confirmed_epoch: bool`
- `pending: Vec<Pending { line, col, expected: char, before: Option<Cell>, made_at }>`
- `cursor: Option<(i64, usize)>`

*`on_key`:*
- If `session_id` differs from `session`, `reset()` first and adopt it.
- The base position is `self.cursor`, or else `screen.cursor()`.
- Call `reset()` and stop when any of these hold:
  - `!screen.is_latest`
  - there's no base position
  - `columns` is `None`
  - the line contains any char whose `unicode-width` isn't 1
  - any cell right of the base has a non-space, non-dim char (pending
    predictions are applied first)
- `Char(c)`:
  - requires `col + 1 < columns`, otherwise `reset()`
  - push `Pending{line, col, expected: c, before: screen.cell(line,col)}`
  - set the cursor to `col + 1`
- `Backspace`:
  - if the last pending entry is a `Char` at `(line, col-1)`, pop it and set
    the cursor to `col - 1`
  - otherwise, if `col == 0`, `reset()`
  - otherwise push `Pending{line, col-1, expected: ' ', before: cell(line,col-1)}`
    and set the cursor to `col - 1`

*`on_content`:*
- Ignore it unless `session_id == session` and `screen.is_latest`.
- Then check each pending entry against `screen.cell(line,col)`:
  - **Confirmed:**
    - for `expected != ' '`: the cell is `Some(Cell{ch: expected, dim: false})`
    - for `expected == ' '`: the cell is `None`, a space, or dim
    - remove the entry and set `confirmed_epoch = true`
  - **Still pending:** the cell equals `before`, comparing `ch` and `dim`, with
    `None` equal to `None`. Keep it.
  - **Contradicted:** anything else. Call `reset()` and return.
- When `pending` becomes empty, set `cursor = None`.

*`reset()`:* clears `pending` and `cursor`, and sets `confirmed_epoch = false`.
It keeps `session`.

*`tick`:* if any pending entry is older than `PREDICTION_TIMEOUT`, call
`reset()`.

*`overlay()` and `cursor()`:* return the pending cells and the cursor only
when `confirmed_epoch`. Otherwise they return empty and `None`.

- [ ] **Step 1: Write the failing tests.** Use a helper
  `screen(line_text_runs, first_line, columns, cursor_col)` that builds a live
  `Content` with a single cursor line. Each test names its assertion:
  - `first_prediction_in_epoch_is_hidden`: after typing `a`, the overlay is
    empty and `cursor()` is `None`.
  - `confirmed_epoch_draws_next_key_instantly`: type `a`, get a snapshot with
    `a` at the column (confirmed), type `b`. The overlay is `[(L, c+1, 'b')]`
    and the cursor is `(L, c+2)`.
  - `burst_keeps_unconfirmed_predictions`: in a confirmed epoch, type `b c d`,
    then get a snapshot showing only `b`. The overlay is `[c, d]` and the
    epoch is still confirmed.
  - `contradiction_resets_and_hides`: in a confirmed epoch, type `x`, then get
    a snapshot with `y` at that cell. The overlay is empty, and the next typed
    key is hidden.
  - `timeout_resets`: with a pending entry, `tick(now + 1001ms)` empties the
    overlay.
  - `backspace_removes_pending_char`: in a confirmed epoch, type `b` and then
    Backspace. The overlay is empty and the cursor is `None`, since nothing is
    pending.
  - `backspace_predicts_erasing_confirmed_text`: in a confirmed epoch on
    `"❯ abc"` with the cursor at 5, Backspace gives the overlay
    `[(L, 4, ' ')]` and the cursor `(L, 4)`. A snapshot with a blank at 4
    confirms it.
  - `dim_text_counts_as_blank`: on the line `[❯ ][cursor][fix (dim)]`, typing
    `x` is predicted (not reset).
  - `dim_cell_does_not_confirm`: on the same line, type `f`. A snapshot that
    still shows dim `f` keeps it pending, and the epoch isn't confirmed.
  - `no_prediction_mid_line`: with non-dim `"abc"` right of the cursor, typing
    `x` resets. In a previously confirmed epoch, the overlay is empty and the
    epoch is hidden.
  - `no_prediction_at_last_column`: `columns = 10` with the cursor at col 9.
    Typing resets.
  - `no_prediction_without_columns_or_history_view`: `columns: None`, or
    `is_latest: false`. Typing resets.
  - `no_prediction_on_wide_char_line`: a line containing `"漢"`. Typing resets.
  - `other_session_content_is_ignored`: with pending entries for `s1`, an
    `on_content("s2", contradicting)` leaves the overlay unchanged.
  - `key_for_new_session_resets`: in a confirmed epoch on `s1`, typing on
    `s2` gives a hidden overlay.
- [ ] **Step 2: Run the tests and confirm they fail.** `cargo test prediction`.
- [ ] **Step 3: Implement** the algorithm above.
- [ ] **Step 4: Run the tests and confirm they pass.** `cargo test prediction`.
- [ ] **Step 5: Commit.** `"Add predictive typing engine"`

### Task 7: Config

**Files:**
- Create: `desktop/src/config.rs`

**Interfaces:**
- Produces:
  - `pub const DEFAULT_SERVER_URL: &str = "https://term.jiye.ca:8443"`
  - `#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)] pub struct Config { pub server_url: String, pub key: Option<String>, pub window: Option<SavedWindow> }`
    It has a serde default for `server_url`.
  - `pub struct SavedWindow { pub x: f32, pub y: f32, pub width: f32, pub height: f32, pub maximized: bool }`
  - `pub fn config_path() -> PathBuf`
  - `pub fn load(path: &Path) -> Config`, which falls back to the default if
    the file is missing or invalid.
  - `pub fn save(path: &Path, cfg: &Config) -> std::io::Result<()>`, which
    creates the parent directory.
  - `pub fn parse_key_input(input: &str) -> Option<String>`
  - `pub fn with_env_overrides(cfg: Config) -> Config`. Here
    `REMOTE_ITERM_URL` and `REMOTE_ITERM_KEY` override the file, for
    development.

- [ ] **Step 1: Write the failing tests.**
  - `parses_raw_key`: `"  abc123  "` gives `Some("abc123")`.
  - `parses_qr_link`: `"https://h:7292/#key=ab%2Fc"` gives `Some("ab/c")`.
  - `rejects_blank`: `"   "` gives `None`.
  - `missing_file_gives_defaults`: `server_url == DEFAULT_SERVER_URL`, and
    `key` is `None`.
  - `save_then_load_round_trips`: use a unique dir under `std::env::temp_dir()`.
  - `env_overrides_apply`: set both env vars inside the test, then remove them.
- [ ] **Step 2: Run the tests and confirm they fail.** `cargo test config`.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests and confirm they pass.**
- [ ] **Step 5: Commit.** `"Add desktop config with key and link parsing"`

### Task 8: Selection model (following the Mac)

**Files:**
- Create: `desktop/src/model.rs`

**Interfaces:**
- Consumes: `protocol::*`
- Produces:
  - `pub enum Effect { Watch(Vec<String>), GetContent(String), Focus { window_id: String, tab_index: u32 }, ResetPrediction }`
  - `pub struct Model { pub windows: Vec<WindowState>, pub window_id: Option<String>, pub tab_id: Option<String>, pub session_id: Option<String>, pub contents: HashMap<String, Content> }`,
    with private fields `prev_front: Option<String>` and
    `explicit_session: bool`
  - `Model::default()`
  - Methods:
    - `apply_state(&mut self, windows: Vec<WindowState>) -> Vec<Effect>`
    - `select_window(&mut self, window_id: &str) -> Vec<Effect>`
    - `select_tab(&mut self, tab_id: &str) -> Vec<Effect>`
    - `select_tab_number(&mut self, n: u8) -> Vec<Effect>`
    - `select_session(&mut self, session_id: &str) -> Vec<Effect>`
    - `on_connected(&self) -> Vec<Effect>`
    - `apply_content(&mut self, sc: SessionContent) -> bool` (true when it's
      the shown session)
    - `apply_history(&mut self, sc: SessionContent) -> bool`
    - `shown(&self) -> Option<&Content>`
    - `current_tabs(&self) -> &[TabState]`
    - `current_tab(&self) -> Option<&TabState>`

**Rules.** These port the web client's `s.on('state')` handler and its
"keep the viewed pane valid" effect.

- **First state:** select the front window (or the first), its `is_selected`
  tab (or the first), and that tab's `current_session_id` (or its first
  session).
- **Front window changes:** if the front window's id differs from
  `prev_front`, follow it.
- **The Mac switches tabs:** within the same window, a different
  `is_selected` tab is followed.
- **The tab disappears:** fall back to the window's selected or first tab.
- **The pane disappears:** if the shown session isn't in the tab, fall back
  to the tab default.
- **Explicit pane choice:** a pane picked with `select_session` is kept while
  the tab is unchanged.
- **Every change of the shown session** emits
  `[ResetPrediction, Watch([sid]), GetContent(sid)]`.
- **Clicks only:** `select_tab`, `select_tab_number` and `select_window`
  additionally emit `Focus{window_id, tab_index}`. Following the Mac never
  emits `Focus`.
- **Tab numbers:** `select_tab_number`: `1..=8` selects the nth tab and `9`
  the last. Out of range returns `vec![]`.

- [ ] **Step 1: Write the failing tests.** Each builds `WindowState` fixtures:
  - `first_state_selects_front_window_tab_and_session`: the effects contain
    `Watch([sid])` and `GetContent(sid)`.
  - `follows_mac_tab_switch_without_focus_effect`
  - `follows_front_window_change`
  - `keeps_explicit_pane_while_tab_unchanged`
  - `falls_back_when_shown_pane_vanishes`
  - `tab_number_selects_nth_and_nine_is_last`: `9` on 3 tabs selects the third
    and emits `Focus{.., tab_index: 3}`. `5` on 3 tabs gives `vec![]`.
  - `select_tab_emits_focus_and_watch`
  - `apply_content_merges_and_reports_shown`: the shown session returns true
    and the content is merged. Another session is cached and returns false.
  - `connected_rewatches_shown_session`: `on_connected()` gives
    `[Watch([sid]), GetContent(sid)]`.
- [ ] **Step 2: Run the tests and confirm they fail.** `cargo test model`.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests and confirm they pass.**
- [ ] **Step 5: Commit.** `"Add tab and pane selection model"`

### Task 9: Server connection

**Files:**
- Create: `desktop/src/connection.rs`

**Interfaces:**
- Consumes: `protocol::{WindowState, SessionContent}`
- Produces:
  - `pub enum ServerEvent { Connected, Disconnected, AuthRejected, State(Vec<WindowState>), Content(SessionContent), History(SessionContent) }`
  - `pub fn start(url: String, key: String) -> (ConnHandle, futures::channel::mpsc::UnboundedReceiver<ServerEvent>)`.
    It spawns an OS thread running a tokio runtime.
  - `#[derive(Clone)] pub struct ConnHandle`. Every method is non-blocking,
    and calls made while disconnected are dropped. It has:
    - `send_keys(&self, session_id: &str, keys: &str)`
    - `watch(&self, ids: Vec<String>)`
    - `get_content(&self, session_id: &str)`
    - `get_earlier(&self, session_id: &str, before_line: i64)`
    - `paste_image(&self, session_id: &str, data_url: String)`
    - `focus(&self, window_id: &str, tab_index: u32)`
    - `new_tab(&self)`
    - `close_tab(&self, tab_id: &str)`
    - `async fn clipboard_text(&self) -> Option<String>`
    - `async fn clipboard_count(&self) -> Option<i64>`
    - `async fn clipboard_since(&self, count: i64) -> Option<String>`
    - `reconnect_now(&self)`
    - `shutdown(&self)`
    - The async methods use `emit_with_ack` with a 5 s timeout and a
      `futures::channel::oneshot`.
  - `pub(crate) fn parse_event(event: &str, payload: &serde_json::Value) -> Option<ServerEvent>`
  - `pub(crate) fn is_auth_error(message: &str) -> bool`, which is true when
    the message contains `"invalid access key"`.

**Behaviour.**
- **Connecting:** `ClientBuilder::new(url).auth(json!({"key": key}))`, with
  handlers for `state`, `content` and `historyContent`, and
  `Event::Connect/Close/Error`.
- **Reconnecting:** the reconnect loop has backoff from 0.5 s to 5 s and
  retries forever.
- **Wrong key:** an auth error emits `AuthRejected` and stops reconnecting.
- **`reconnect_now`:** skips the current backoff.

- [ ] **Step 1: Write the failing tests.**
  - `parses_state_content_history`: three JSON fixtures map to the right
    variants.
  - `unknown_event_is_none`
  - `detects_auth_error`
  - Two live tests, marked `#[ignore]`, that read the key from
    `REMOTE_ITERM_KEY` or the Mac key file:
    - `live_connect_receives_state`: `http://localhost:7291`, with `State`
      arriving within 5 s.
    - `live_wrong_key_is_rejected`: `AuthRejected` within 5 s.
- [ ] **Step 2: Run the tests and confirm they fail.** `cargo test connection`.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests and confirm they pass.** Run `cargo test connection`,
  then `cargo test connection -- --ignored` against the live server. Expected:
  all pass. If rust_socketio doesn't report the server's connect error, write
  that down and make the tests pass by treating "closed before any `state` on
  the first attempt" as `AuthRejected`.
- [ ] **Step 5: Commit.** `"Add Socket.IO connection with reconnect"`

### Task 10: Window, tab bar and terminal rendering

**Files:**
- Modify: `desktop/src/main.rs`
- Create: `desktop/src/ui/mod.rs`, `desktop/src/ui/root.rs`,
  `desktop/src/ui/terminal.rs`, `desktop/src/ui/tab_bar.rs`,
  `desktop/src/ui/theme.rs`

**Interfaces:**
- Consumes: `config`, `connection::start`/`ServerEvent`, `model::Model`/`Effect`
- Produces:
  - `ui::root::RootView`, which owns `Model`, `Predictor`, `ConnHandle` and
    `Config`. Its `RootView::apply_effects(&mut self, effects: Vec<Effect>, cx)`
    is the only place `Effect`s turn into `ConnHandle` calls and
    `Predictor::reset`.
  - `ui::terminal::render_terminal(content: &Content, overlay: &[OverlayCell], cursor: Option<(i64, usize)>, …) -> impl IntoElement`.
    It uses `uniform_list`, so only visible rows are laid out. Each row is a
    `StyledText` whose `TextRun`s come from `Run` (colour `f` or else `fg`,
    background `g`, bold → `FontWeight::BOLD`, dim → 60 % alpha, link →
    underline on hover). The cursor is a solid block at its cell, and there's
    no soft wrap.
  - `ui::theme`: background `#0a0a0a`, accent `#10b981`, the platform font
    from Global Constraints, 13 px, with the cell width taken from
    `text_system().advance(font_id, size, 'M')`.

- [ ] **Step 1: Wire the app.** `main.rs` loads config with env overrides. It
  opens a window titled `Remote iTerm`, at the saved bounds or a default of
  1100×700. It calls `connection::start`, and a `cx.spawn` loop reads
  `ServerEvent`s:
  - `State` goes to `model.apply_state`
  - `Content` and `History` go to `model.apply_*`, plus
    `predictor.on_content`
  - `Connected` goes to `on_connected`

  Effects are then applied and `cx.notify()` called.
- [ ] **Step 2: Render.**
  - **Tab bar:** a dark strip of the current window's tab titles, with the
    selected one in the accent colour. Clicking a tab calls
    `model.select_tab`, which emits `Focus`, `Watch` and `GetContent`.
  - **Terminal:** below the tab bar, showing `model.shown()`.
  - **Following:** stay at the bottom while already at the bottom.
- [ ] **Step 3: Verify on the Mac.** Run
  `REMOTE_ITERM_URL=http://localhost:7291 REMOTE_ITERM_KEY=$(cat ~/Library/Application\ Support/remote-iterm/access-key) cargo run`.
  Expected:
  - the window shows the iTerm tabs
  - the Claude Code session renders with the right colours, a cursor block
    after `❯`, and no wrapped lines
  - clicking another tab switches the view, and switches the Mac's tab
  - output in a zsh tab (`for i in $(seq 1 200); do echo $i; done`) scrolls
    live
- [ ] **Step 4: Run `cargo test` and confirm everything passes.** Run
  `cargo build --release` too, with no warnings in the crate.
- [ ] **Step 5: Commit.** `"Render tabs and terminal content in the desktop app"`

### Task 11: Typing, prediction overlay, paste and tab shortcuts

**Files:**
- Modify: `desktop/src/ui/root.rs`, `desktop/src/ui/terminal.rs`

**Interfaces:**
- Consumes:
  - `keys::{KeyInput, encode_key, paste_sequence}`
  - `prediction::{Predictor, PredictKey}`
  - `model::select_tab_number`
  - `ConnHandle::{send_keys, paste_image}`
  - GPUI `cx.read_from_clipboard()`, `ClipboardEntry::{String, Image}`

- [ ] **Step 1: Handle keys.** The terminal area has a `FocusHandle`, focused
  on launch and on click. In `on_key_down` with
  `ev.keystroke → KeyInput`, in this order:
  1. **Ctrl+digit 1–9:** `select_tab_number` and apply the effects.
  2. **Ctrl+V:**
     - clipboard image → `paste_image` with a `data:image/<fmt>;base64,…`
       URL, then `predictor.reset()`
     - text → `send_keys(paste_sequence(text))`, then `reset`
     - empty → `send_keys("\x16")`
  3. **Anything else:** `encode_key`. If it returns `Some(seq)`:
     - `send_keys(seq)` comes **first**
     - then a printable single char → `predictor.on_key(Char(c))`
     - `\x7f` → `on_key(Backspace)`
     - anything else → `predictor.reset()`
     - then jump the view to the live tail and call `cx.notify()`

  Call `cx.stop_propagation()` for handled keys, so GPUI doesn't move focus on
  Tab.
- [ ] **Step 2: Draw the overlay.** `render_terminal` draws `overlay` cells
  over the row text, in the style of the run left of the cursor without dim.
  It draws the cursor block at `predictor.cursor()` when that's `Some`.
  - While pending entries exist, a 250 ms `cx.spawn` timer calls
    `predictor.tick` and `notify`.
- [ ] **Step 3: Verify on the Mac against a scratch iTerm tab running zsh.**
  Open one with the app's new-tab button from Task 13, or open it on the Mac.
  Expected:
  - after the first character of a line, typed characters appear with no
    visible wait
  - Ctrl+W deletes a word
  - Ctrl+1 and Ctrl+2 switch tabs
  - pasting multi-line text arrives bracketed
  - `sudo -k; sudo true`: no password characters ever appear (Ctrl+C out)
  - `vim`: normal-mode `jjjkkk` doesn't flicker characters
  - in Claude Code's prompt, typing appears instantly, and Backspace and
    word edits behave
  - typing in the middle of a line falls back to the real echo
- [ ] **Step 4: Run `cargo test` and confirm everything passes.**
- [ ] **Step 5: Commit.** `"Send keys with predictive echo, paste and tab shortcuts"`

### Task 12: Mouse, selection, history paging and links

**Files:**
- Modify: `desktop/src/ui/terminal.rs`, `desktop/src/ui/root.rs`

**Interfaces:**
- Consumes:
  - `mouse::*`
  - `geometry::{point_to_cell, selection_text}`
  - `ConnHandle::{get_earlier, clipboard_count, clipboard_since}`
  - GPUI `cx.write_to_clipboard`, `cx.open_url`

**When `shown().mouse` is true:**
- **Left press:** start `clipboard_count()`, then send
  `sgr(modifier_bits, col+1, terminal_row(line, screen_first_line), false)`.
- **Drag:** sends `MOTION | mods` on cell change.
- **Release:** sends the release report. Then it awaits the count and calls
  `clipboard_since(count)`. If it gets `Some(text)`, the text goes to
  `write_to_clipboard`.
- **Wheel:** sends `WHEEL_UP`/`WHEEL_DOWN` at the pointer cell, one per line
  of delta.

**Otherwise:**
- **Wheel:** scrolls the list.
- **Reaching the top:** when `first_line > available_first_line` and no
  request is in flight, `get_earlier(session, first_line)`. Prepending keeps
  the visible row in place.
- **Drag:** makes a local selection, highlighted.
- **Ctrl+C:** with a selection, writes `selection_text` and clears the
  selection. Without one, it falls through to Task 11's key path (`\x03`).
- **Ctrl+Shift+C:** always copies.
- **Ctrl+click on a run with `u`:** `cx.open_url`.

- [ ] **Step 1: Implement** the rules above. A key sent to the terminal
  clears any selection.
- [ ] **Step 2: Verify on the Mac.** Expected:
  - in Claude Code, the wheel scrolls its transcript, and dragging selects in
    Claude Code with the copied text landing on the clipboard
  - in zsh, the wheel scrolls history, and reaching the top loads older lines
    without jumping
  - drag + Ctrl+C copies and doesn't send `^C`
  - Ctrl+C without a selection interrupts `sleep 100`
  - Ctrl+clicking a URL in output opens the browser
- [ ] **Step 3: Run `cargo test` and confirm everything passes.**
- [ ] **Step 4: Commit.** `"Add mouse forwarding, selection, history paging and links"`

### Task 13: Key screen, connection overlay, pickers, clipboard button and window memory

**Files:**
- Create: `desktop/src/ui/key_screen.rs`, `desktop/src/ui/overlay.rs`
- Modify: `desktop/src/ui/root.rs`, `desktop/src/ui/tab_bar.rs`,
  `desktop/src/main.rs`

**Interfaces:**
- Consumes:
  - `config::{parse_key_input, save, SavedWindow}`
  - `ConnHandle::{clipboard_text, new_tab, close_tab, reconnect_now}`
  - `model::{select_window, select_session}`

**Behaviour:**
- **Key screen:** shown when there's no key, or after `AuthRejected`.
  - Typing and Ctrl+V append to a masked field, and Backspace deletes.
  - Enter runs `parse_key_input`. If it gets `Some`, it saves the config,
    restarts the connection with the new key, and hides the screen.
  - A rejected key shows "That key was not accepted."
- **Disconnected overlay:** shown after the first connect while
  disconnected. It says "Reconnecting…" and has a "Retry now" button that
  calls `reconnect_now`.
- **Tab bar additions:**
  - a close `×` on each tab, hidden when there's only one tab in total, which
    calls `close_tab(tab_id)`
  - a `+` button that calls `new_tab`
  - a window picker, shown only when there's more than one window, that calls
    `select_window`
  - a pane switcher, shown when the current tab has more than one session,
    with one button per session (name, or "Pane N"), that calls
    `select_session`
  - a Mac-clipboard button that calls `clipboard_text()`:
    - `Some(non-empty)` → `write_to_clipboard`, and a green ✓ for 1.5 s
    - otherwise → a red ✗ for 1.5 s
- **Window memory:** on close and on bounds change (debounced to 1 s), save
  `SavedWindow` from `window.window_bounds()`, including maximized. `main.rs`
  restores it.

- [ ] **Step 1: Implement** the above.
- [ ] **Step 1b: Add the app icon.** Add a simple icon at
  `desktop/assets/icon.ico` (a green `>_` on dark, matching the theme). Embed
  it on Windows from `desktop/build.rs`, using the `winresource`
  build-dependency under `cfg(windows)`. macOS needs no icon for development.
- [ ] **Step 2: Verify on the Mac.** Expected:
  - launching with no `REMOTE_ITERM_KEY` and an empty config shows the key
    screen
  - pasting the QR link connects
  - a wrong key shows the rejection message
  - `launchctl kickstart -k gui/$(id -u)/ca.jiye.remote-iterm` shows
    "Reconnecting…", then recovers to the same session (Review Focus #4)
  - `+` and `×` add and remove tabs on the Mac
  - the pane switcher changes panes in a split tab
  - `echo hi | pbcopy` then the clipboard button puts "hi" on the clipboard
  - moving, resizing and relaunching restores the window
- [ ] **Step 3: Run `cargo test` and confirm everything passes.**
- [ ] **Step 4: Commit.** `"Add key screen, reconnect overlay, pickers, clipboard button and window memory"`

### Task 14: Docs, latency check and final verification

**Files:**
- Create: `desktop/README.md`
- Modify: `docs/ARCHITECTURE.md` (add a "Native desktop client" section under
  Components)

- [ ] **Step 1: Write `desktop/README.md`.**
  - What the app is.
  - Windows prerequisites: rustup, so `rust-toolchain.toml` installs 1.99.1,
    and Visual Studio Build Tools with "Desktop development with C++" and the
    Windows 10/11 SDK.
  - Build: `cargo build --release`. The output is
    `target\release\remote-iterm.exe`.
  - First run: paste the QR link or the key.
  - Config location.
  - The dev env vars.
  - The macOS dev note about `runtime_shaders` and Homebrew's rustc.
- [ ] **Step 2: Add the architecture section.** Cover the module map, the fact
  that the client follows the Mac like the web client, prediction in one
  paragraph, and the two server fields and the keystroke window.
- [ ] **Step 3: Measure latency.** Re-run the probe
  `/private/tmp/claude-501/-Users-alexanderyoshino-remote-iterm-fork/ee34a383-13f8-4394-906b-5a489a20b9ef/scratchpad/latency_probe.py window`
  with `server/.venv/bin/python -I`, and again while another pane runs
  `while true; do date; sleep 0.02; done` in the same window. Expected: the
  end-to-end median stays around 21 ms in both. Record the numbers in the
  final report.
- [ ] **Step 4: Run the full verification.**
  - `server/.venv/bin/python -m unittest server.test_server server.test_auth`
  - `cargo test`
  - `cargo test connection -- --ignored`
  - `cargo build --release`
  - `cd client && npx tsc --noEmit`, to confirm the web client is untouched
    and still typechecks

  Expected: everything passes.
- [ ] **Step 5: Commit.** `"Document the desktop client"`
- [ ] **Step 6: Hand off the Windows checks to the user.**
  - It builds with the README steps.
  - The window opens and remembers its place.
  - Ctrl+W deletes a word in zsh.
  - Ctrl+1–9 switch tabs.
  - Typing in Claude Code feels instant.
  - AltGr characters type correctly, if they use such a layout.
  - The clipboard button works.
  - Image paste with Ctrl+V reaches Claude Code.
