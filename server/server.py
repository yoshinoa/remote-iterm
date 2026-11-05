#!/usr/bin/env python3
"""Remote iTerm server — drives iTerm2 over its native Python API.

Replaces the previous AppleScript/osascript implementation. A single asyncio
loop runs both the Socket.IO server (for the web/phone client) and the iTerm2
connection. Terminal output and window/tab state are PUSHED from iTerm2's
notification streams instead of being polled, so no per-call processes are
spawned. The Socket.IO contract retains the upstream event vocabulary while
adding pane watches and structured, styled terminal content for the rewritten
client.
"""
import asyncio
import base64
import binascii
import contextlib
import ctypes
import json
import math
import os
import re
import resource
import signal
import socket
import subprocess
import sys

import iterm2
import socketio
from aiohttp import web

try:
    from .auth import is_valid_key, load_or_create_key
except ImportError:  # Running server.py directly from the server directory.
    from auth import is_valid_key, load_or_create_key

PORT = 7291
CLIENT_PORT = 7292  # Vite client origin the phone/browser is served from.

# Reject a single execute/sendKeys/broadcast payload larger than this many bytes.
# Ample for real pastes while blocking memory-exhaustion floods from a client
# that authenticated but then misbehaves.
MAX_INPUT_BYTES = 1_048_576

# Upper bound on a pasted image (decoded bytes) written to the Mac pasteboard.
MAX_IMAGE_BYTES = 25 * 1024 * 1024
MAX_SOCKET_PAYLOAD = MAX_IMAGE_BYTES * 4 // 3 + 64 * 1024

# Watched panes receive only their recent tail on each screen change. Older
# content is fetched in pages when the client scrolls upward, avoiding a full
# scrollback read (and a large Socket.IO payload) for every keypress.
LIVE_CONTENT_LINES = 250
HISTORY_PAGE_LINES = 500
PREVIEW_CONTENT_LINES = 40
STREAM_MIN_INTERVAL = 0.05  # At most 20 live snapshots per second/session.
MAX_SOCKET_QUEUE_DEPTH = 2
KEYSTROKE_WINDOW = 0.1
MAX_WATCHED_SESSIONS = 2

def _local_ipv4s() -> set[str]:
    """Every IPv4 address bound to a local interface (LAN, Tailscale/VPN, …)."""
    ips: set[str] = set()
    try:
        out = subprocess.run(
            ["/sbin/ifconfig"], capture_output=True, text=True, timeout=2).stdout
        for line in out.splitlines():
            fields = line.split()
            if len(fields) >= 2 and fields[0] == "inet":
                ips.add(fields[1])
    except (OSError, subprocess.SubprocessError):
        pass
    # Fallback in case ifconfig is unavailable for some reason.
    try:
        for info in socket.getaddrinfo(socket.gethostname(), None, socket.AF_INET):
            ips.add(info[4][0])
    except OSError:
        pass
    return ips


def client_origins() -> list[str]:
    """Web origins allowed to open a Socket.IO connection.

    Only the client app — served on CLIENT_PORT from this machine — is a
    legitimate origin, so we allow every local address it could be loaded from
    (loopback plus each interface's IPv4, including the LAN and any Tailscale/VPN
    address) on that port, and reject everything else. This blocks arbitrary web
    pages and DNS-rebinding hosts that a wildcard (`cors_allowed_origins="*"`)
    let through, while still working over Tailscale.

    Set REMOTE_ITERM_ORIGINS (comma-separated, e.g. "https://term.example.com")
    to allow additional origins — needed when serving behind an HTTPS reverse
    proxy on a domain, where the browser's Origin is that domain, not a local IP.
    """
    hosts = {"localhost", "127.0.0.1"} | _local_ipv4s()
    origins = {f"http://{host}:{CLIENT_PORT}" for host in hosts}
    extra = os.environ.get("REMOTE_ITERM_ORIGINS", "")
    origins |= {o.strip() for o in extra.split(",") if o.strip()}
    return sorted(origins)


sio = socketio.AsyncServer(
    async_mode="aiohttp", cors_allowed_origins=client_origins(),
    max_http_buffer_size=MAX_SOCKET_PAYLOAD)
app = web.Application()
sio.attach(app)

clients: set[str] = set()

connection: iterm2.Connection | None = None
itermapp: iterm2.App | None = None
shared_key: str | None = None

screen_size = {"width": 1470, "height": 956}

# Sessions each connected client is actively viewing (its main pane + optional
# split pane). We live-stream exactly the union of these, independent of which
# session iTerm has focused on the Mac — so the phone can watch any pane it
# likes (including a background split pane) and still get live updates.
watched_by_sid: dict[str, set[str]] = {}
stream_tasks: dict[str, asyncio.Task] = {}
last_content: dict[str, dict] = {}
keystroke_at: dict[str, float] = {}
stream_reconcile_lock = asyncio.Lock()

# Socket.IO/Engine.IO queues are unbounded. Keep a bounded, latest-wins
# application outbox per client and stop feeding Engine.IO while its real
# socket queue is backed up (for example when a phone sleeps).
pending_events: dict[str, dict[str, tuple[str, object]]] = {}
delivery_wakeups: dict[str, asyncio.Event] = {}
delivery_tasks: dict[str, asyncio.Task] = {}

# Per-session color palette (default fg/bg + ANSI 0-15), read once from the
# session's iTerm2 profile so standard colors match the user's actual theme.
palette_cache: dict[str, dict] = {}


# --- Screen geometry (CoreGraphics, no AppKit/osascript) -----------------------

class _CGPoint(ctypes.Structure):
    _fields_ = [("x", ctypes.c_double), ("y", ctypes.c_double)]


class _CGSize(ctypes.Structure):
    _fields_ = [("width", ctypes.c_double), ("height", ctypes.c_double)]


class _CGRect(ctypes.Structure):
    _fields_ = [("origin", _CGPoint), ("size", _CGSize)]


def read_screen_size() -> dict:
    try:
        core_graphics = ctypes.CDLL(
            "/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics")
        core_graphics.CGMainDisplayID.restype = ctypes.c_uint32
        core_graphics.CGDisplayBounds.argtypes = [ctypes.c_uint32]
        core_graphics.CGDisplayBounds.restype = _CGRect
        bounds = core_graphics.CGDisplayBounds(core_graphics.CGMainDisplayID())
        if bounds.size.width <= 0 or bounds.size.height <= 0:
            raise RuntimeError("CoreGraphics did not return a display")
        return {
            "width": int(bounds.size.width),
            "height": int(bounds.size.height),
        }
    except Exception:
        return {"width": 1470, "height": 956}


# --- Pane (split) geometry -----------------------------------------------------
#
# A tab's panes live in a tree of Splitters (verified against the live API):
#   * Splitter.vertical=True  -> children are arranged left→right (divide x)
#   * Splitter.vertical=False -> children are arranged top→bottom (divide y)
#   * child order is visual order; Session.frame is top-left origin but LOCAL to
#     its parent splitter, so we reconstruct tab-wide rects by walking the tree
#     and weighting each child by its natural (frame) size along the split axis.

def _natural_size(node) -> tuple:
    if isinstance(node, iterm2.Session):
        f = node.frame
        if f is None:
            return (1.0, 1.0)
        return (float(f.size.width), float(f.size.height))
    sizes = [_natural_size(c) for c in node.children] or [(1.0, 1.0)]
    if node.vertical:
        return (sum(s[0] for s in sizes), max(s[1] for s in sizes))
    return (max(s[0] for s in sizes), sum(s[1] for s in sizes))


def _assign_rects(node, x: float, y: float, w: float, h: float, out: dict) -> None:
    if isinstance(node, iterm2.Session):
        out[node.session_id] = (x, y, w, h)
        return
    sizes = [_natural_size(c) for c in node.children]
    if node.vertical:
        total = sum(s[0] for s in sizes) or 1.0
        cx = x
        for c, s in zip(node.children, sizes):
            cw = w * s[0] / total
            _assign_rects(c, cx, y, cw, h, out)
            cx += cw
    else:
        total = sum(s[1] for s in sizes) or 1.0
        cy = y
        for c, s in zip(node.children, sizes):
            ch = h * s[1] / total
            _assign_rects(c, x, cy, w, ch, out)
            cy += ch


def pane_layout(tab) -> tuple:
    """Returns ({session_id: (x, y, w, h) in 0..1, top-left}, aspect, maximized).

    When a pane is maximized the others are "minimized": they have no frame and
    the split tree collapses to just the maximized pane, so the true geometry is
    gone. In that case we fall back to an even grid over all panes.
    """
    try:
        if tab.minimized_sessions:
            panes = tab.all_sessions
            n = len(panes)
            cols = max(1, math.ceil(math.sqrt(n)))
            rows = max(1, math.ceil(n / cols))
            rects = {
                s.session_id: ((i % cols) / cols, (i // cols) / rows,
                               1 / cols, 1 / rows)
                for i, s in enumerate(panes)
            }
            aspect = 1.6
            for s in panes:
                f = s.frame
                if f is not None and f.size.height:
                    aspect = f.size.width / f.size.height
                    break
            return rects, aspect, True

        root = tab.root
        nat = _natural_size(root)
        aspect = (nat[0] / nat[1]) if nat[1] else 1.0
        rects: dict = {}
        _assign_rects(root, 0.0, 0.0, 1.0, 1.0, rects)
        return rects, aspect, False
    except Exception:
        return {}, 1.0, False


# --- State serialization (windows / tabs / sessions) ---------------------------

async def build_state() -> list:
    """Mirror the old AppleScript getState() shape.

    Window/tab ids are opaque to the client; session ids are iTerm2 GUIDs and
    match what the client already persists. Bounds are converted from iTerm2's
    bottom-left Cocoa origin to the top-left origin the client's window map
    expects.
    """
    if itermapp is None:
        return []
    current_window = itermapp.current_window
    current_window_id = current_window.window_id if current_window else None
    flip_height = screen_size["height"]
    state = []
    for window in itermapp.windows:
        try:
            frame = await window.async_get_frame()
            bounds = {
                "x": int(frame.origin.x),
                "y": int(flip_height - (frame.origin.y + frame.size.height)),
                "w": int(frame.size.width),
                "h": int(frame.size.height),
            }
        except Exception:
            bounds = {"x": 0, "y": 0, "w": 800, "h": 600}

        current_tab = window.current_tab
        current_tab_id = current_tab.tab_id if current_tab else None
        tabs = []
        for index, tab in enumerate(window.tabs, start=1):
            # The tab bar in iTerm shows the tab's title (which a worktree/script
            # may set independently of the session name); mirror that.
            try:
                tab_title = await tab.async_get_variable("title")
            except Exception:
                tab_title = None
            rects, aspect, maximized = pane_layout(tab)
            sessions = []
            # all_sessions (not sessions) so a tab with a maximized pane still
            # reports every pane — the rest are "minimized", which tab.sessions
            # excludes. session.name avoids a per-pane RPC and is set for
            # minimized panes too.
            for session in tab.all_sessions:
                entry = {"id": session.session_id, "name": session.name or ""}
                r = rects.get(session.session_id)
                if r is not None:
                    entry["rect"] = {"x": round(r[0], 4), "y": round(r[1], 4),
                                     "w": round(r[2], 4), "h": round(r[3], 4)}
                sessions.append(entry)
            # Which pane iTerm has focused in this tab; the client uses it as the
            # default pane to show when you switch to the tab.
            cur_sess = tab.current_session
            tabs.append({
                "index": index,
                "id": f"{window.window_id}-{index}",
                "tabId": tab.tab_id,
                "title": tab_title or "",
                "isSelected": tab.tab_id == current_tab_id,
                "currentSessionId": cur_sess.session_id if cur_sess else "",
                "aspect": round(aspect, 4),
                "maximized": maximized,
                "sessions": sessions,
            })

        state.append({
            "id": window.window_id,
            "isFront": window.window_id == current_window_id,
            "tabs": tabs,
            "bounds": bounds,
        })
    return state


_last_pushed_state: str | None = None


def _socket_queue_depth(sid: str) -> int | None:
    """Return Engine.IO's real outbound queue depth for a Socket.IO client."""
    eio_sid = sio.manager.eio_sid_from_sid(sid, "/")
    socket = sio.eio.sockets.get(eio_sid) if eio_sid else None
    return socket.queue.qsize() if socket is not None else None


def queue_client_event(
        sid: str, key: str, event: str, data: object) -> None:
    """Queue one latest-wins event without growing an unbounded backlog."""
    if sid not in clients:
        return
    outbox = pending_events.get(sid)
    wakeup = delivery_wakeups.get(sid)
    if outbox is None or wakeup is None:
        return
    outbox[key] = (event, data)
    wakeup.set()


async def _deliver_client_events(sid: str) -> None:
    """Feed Engine.IO only while a client has capacity for another packet."""
    wakeup = delivery_wakeups[sid]
    outbox = pending_events[sid]
    while sid in clients:
        await wakeup.wait()
        wakeup.clear()
        while sid in clients and outbox:
            depth = _socket_queue_depth(sid)
            if depth is None:
                return
            if depth >= MAX_SOCKET_QUEUE_DEPTH:
                await asyncio.sleep(STREAM_MIN_INTERVAL)
                continue
            key = next(iter(outbox))
            event, data = outbox.pop(key)
            await sio.emit(event, data, to=sid)


def start_client_delivery(sid: str) -> None:
    pending_events[sid] = {}
    delivery_wakeups[sid] = asyncio.Event()
    delivery_tasks[sid] = asyncio.create_task(_deliver_client_events(sid))


async def stop_client_delivery(sid: str) -> None:
    task = delivery_tasks.pop(sid, None)
    if task is not None:
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)
    pending_events.pop(sid, None)
    delivery_wakeups.pop(sid, None)


def queue_content_for_watchers(session_id: str, content: dict) -> None:
    data = {"sessionId": session_id, **content}
    for sid, watched in list(watched_by_sid.items()):
        if session_id in watched:
            queue_client_event(
                sid, f"content:{session_id}", "content", data)


async def push_state() -> None:
    """Broadcast window/tab state, skipping the emit when nothing changed."""
    global _last_pushed_state
    if not clients:
        return
    state = await build_state()
    serialized = json.dumps(state, sort_keys=True)
    if serialized == _last_pushed_state:
        return
    _last_pushed_state = serialized
    for sid in list(clients):
        queue_client_event(sid, "state", "state", state)


async def sync_loop() -> None:
    """Safety-net refresh for changes the notification monitors don't surface
    (job/title updates, pure window moves). Cheap API calls only, gated on
    connected clients, and deduplicated by push_state so it emits only on real
    changes — this is what keeps tab labels fresh without polling iTerm2 via
    per-call subprocesses the way the old AppleScript server did.
    """
    while True:
        await asyncio.sleep(2.0)
        if clients:
            await push_state()


async def health_loop() -> None:
    """Periodic bounded-queue and peak-RSS telemetry for leak diagnosis."""
    while True:
        await asyncio.sleep(60)
        queue_depths = [
            depth for sid in clients
            if (depth := _socket_queue_depth(sid)) is not None
        ]
        max_rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        max_rss_mb = max_rss / (1024 * 1024 if sys.platform == "darwin" else 1024)
        print(
            "health: "
            f"max_rss={max_rss_mb:.1f}MB clients={len(clients)} "
            f"streams={len(stream_tasks)} pending={sum(map(len, pending_events.values()))} "
            f"engineio_queue_max={max(queue_depths, default=0)}")


# --- Reading terminal contents with faithful colors ---------------------------
#
# Each cell's style gives a foreground/background that is either true-color RGB,
# a standard ANSI index (0-255), or "default". We resolve those to hex strings
# (mapping ANSI 0-15 + default through the session's actual iTerm2 theme, and
# 16-255 through the xterm-256 palette) and emit run-length-grouped colored runs.

_DEFAULT_ANSI = [
    (0, 0, 0), (205, 0, 0), (0, 205, 0), (205, 205, 0),
    (0, 0, 238), (205, 0, 205), (0, 205, 205), (229, 229, 229),
    (127, 127, 127), (255, 0, 0), (0, 255, 0), (255, 255, 0),
    (92, 92, 255), (255, 0, 255), (0, 255, 255), (255, 255, 255),
]
_DEFAULT_PALETTE = {"fg": "#d4d4d8", "bg": "#0a0a0a", "ansi": _DEFAULT_ANSI}

_LINK_SCHEMES = ("http://", "https://", "mailto:")
_BARE_URL = re.compile(r"https?://[^\s<>\"'`]*[^\s<>\"'`.,;:!?)\]}]")


def _hex(rgb) -> str:
    # iTerm2 profile colors are floats (0-255); cell colors are ints. Normalize.
    return "#" + "".join(f"{min(255, max(0, int(round(c)))):02x}" for c in rgb)


def _xterm256(idx: int):
    if idx < 16:
        return _DEFAULT_ANSI[idx]
    if idx < 232:
        n = idx - 16
        levels = (0, 95, 135, 175, 215, 255)
        return (levels[n // 36], levels[(n // 6) % 6], levels[n % 6])
    gray = 8 + 10 * (idx - 232)
    return (gray, gray, gray)


def _system_is_dark() -> bool:
    try:
        import Foundation
        defaults = Foundation.NSUserDefaults.standardUserDefaults()
        return defaults.stringForKey_("AppleInterfaceStyle") == "Dark"
    except Exception:
        return True


async def get_palette(session) -> dict:
    pal = palette_cache.get(session.session_id)
    if pal is not None:
        return pal
    try:
        profile = await session.async_get_profile()
        # Honor "separate colors for light and dark mode": read the variant that
        # matches the current system appearance, so colors match the display.
        suffix = ""
        if profile.use_separate_colors_for_light_and_dark_mode:
            suffix = "_dark" if _system_is_dark() else "_light"

        def rgb(name):
            c = getattr(profile, name + suffix)
            return (c.red, c.green, c.blue)

        pal = {
            "fg": _hex(rgb("foreground_color")),
            "bg": _hex(rgb("background_color")),
            "ansi": [rgb(f"ansi_{i}_color") for i in range(16)],
        }
    except Exception:
        pal = _DEFAULT_PALETTE
    palette_cache[session.session_id] = pal
    return pal


def _resolve(color, pal) -> str | None:
    """A CellStyle.Color -> hex string, or None meaning the pane default."""
    if color.is_rgb:
        c = color.rgb
        return _hex((c.red, c.green, c.blue))
    if color.is_standard:
        idx = color.standard
        return _hex(pal["ansi"][idx] if idx < 16 else _xterm256(idx))
    return None  # alternate / default


def _cell_links(cells: list, styles: list) -> list:
    owner = [x for x, ch in enumerate(cells) for _ in ch]
    links = [None] * len(cells)
    for m in _BARE_URL.finditer("".join(cells)):
        for x in range(owner[m.start()], owner[m.end() - 1] + 1):
            links[x] = m.group()
    for x, style in enumerate(styles):
        url = style.url
        if url is not None and url.url.lower().startswith(_LINK_SCHEMES):
            links[x] = url.url
    return links


def _line_cells(line) -> tuple[list, list]:
    cells: list = []
    styles: list = []
    while (style := line.style_at(len(styles))) is not None:
        cells.append(line.string_at(len(styles)).replace("\x00", " "))
        styles.append(style)
    return cells, styles


def _wrapped_links(lines) -> list:
    out: list = []
    cells: list = []
    styles: list = []
    sizes: list = []
    for i, line in enumerate(lines):
        line_cells, line_styles = _line_cells(line)
        cells += line_cells
        styles += line_styles
        sizes.append(len(line_cells))
        if line.hard_eol or i == len(lines) - 1:
            links = _cell_links(cells, styles)
            for n in sizes:
                out.append(links[:n])
                links = links[n:]
            cells, styles, sizes = [], [], []
    return out


def _line_runs(line, pal, cursor_x: int | None = None, links: list | None = None) -> list:
    """Group a line into styled runs, optionally marking its cursor cell."""
    cells, styles = _line_cells(line)
    if links is None:
        links = _cell_links(cells, styles)
    runs: list = []
    cur = None
    buf = ""
    x = 0
    for style, ch, link in zip(styles, cells, links):
        if cursor_x == x:
            if buf:
                runs.append(_make_run(cur, buf))
                buf = ""
            # The client renders this as a zero-width overlay, so the cursor
            # does not move the character underneath it.
            runs.append({"t": "", "c": True})
        fg = _resolve(style.fg_color, pal)
        bg = _resolve(style.bg_color, pal)
        if style.inverse:
            fg, bg = (bg or pal["bg"]), (fg or pal["fg"])
        key = (fg, bg, bool(style.bold), bool(style.faint), link)
        if key != cur:
            if buf:
                runs.append(_make_run(cur, buf))
            cur, buf = key, ch
        else:
            buf += ch
        x += 1
    if buf:
        runs.append(_make_run(cur, buf))
    if cursor_x is not None and cursor_x >= x:
        if cursor_x > x:
            runs.append({"t": " " * (cursor_x - x)})
        runs.append({"t": "", "c": True})
    # Trim trailing whitespace runs that carry no background.
    # Never trim through the cursor: it may be on an otherwise blank line.
    while (runs and not runs[-1].get("c") and "g" not in runs[-1]
           and not runs[-1]["t"].strip()):
        runs.pop()
    if runs and not runs[-1].get("c") and "g" not in runs[-1]:
        runs[-1]["t"] = runs[-1]["t"].rstrip()
    return runs


def _make_run(key, text: str) -> dict:
    fg, bg, bold, dim, link = key
    run = {"t": text}
    if link:
        run["u"] = link
    if fg:
        run["f"] = fg
    if bg:
        run["g"] = bg
    if bold:
        run["b"] = True
    if dim:
        run["d"] = True
    return run


def _content_line_range(
        info, line_count: int, before_line: int | None = None,
        alternate_screen: bool = False,
) -> tuple[int, int, int, int]:
    """Return a page within the complete range retained by iTerm.

    Absolute line numbers below ``overflow`` have already been discarded by
    iTerm. Everything after it consists of the session's scrollback plus its
    mutable screen area and is available through ``async_get_contents``.
    """
    available_first = info.overflow
    terminal_end = (available_first + info.scrollback_buffer_height
                    + info.mutable_area_height)
    if alternate_screen:
        available_first = terminal_end - info.mutable_area_height
    page_end = terminal_end
    if before_line is not None:
        page_end = min(page_end, max(available_first, before_line))
    first = max(available_first, page_end - line_count)
    return first, page_end - first, available_first, terminal_end


def _grid_fields(session) -> dict:
    size = getattr(session, "grid_size", None)
    if size is None:
        return {}
    return {"columns": int(size.width), "rows": int(size.height)}


async def read_content(
        session_id: str | None,
        line_count: int = LIVE_CONTENT_LINES,
        before_line: int | None = None,
        screen=None,
) -> dict | None:
    if itermapp is None or not session_id or session_id == "undefined":
        return None
    session = itermapp.get_session_by_id(session_id)
    if session is None:
        return None
    try:
        pal = await get_palette(session)
        async with iterm2.Transaction(connection):
            info = await session.async_get_line_info()
            if screen is None:
                screen = await session.async_get_screen_contents()
            alternate = await session.async_get_variable("showingAlternateScreen")
            mouse_mode = await session.async_get_variable("mouseReportingMode")
            first, count, available_first, terminal_end = _content_line_range(
                info, line_count, before_line, bool(alternate))
            mouse = {
                "mouse": isinstance(mouse_mode, int) and mouse_mode >= 0,
                "screenFirstLine": terminal_end - info.mutable_area_height,
                **_grid_fields(session),
            }
            if count <= 0:
                return {
                    "lines": [], "fg": pal["fg"], "bg": pal["bg"],
                    "firstLine": first, "availableFirstLine": available_first,
                    "terminalEnd": terminal_end,
                    "isLatest": before_line is None,
                    **mouse,
                }
            lines = await session.async_get_contents(first, count)
        cursor = screen.cursor_coord
        links = _wrapped_links(lines)
        rendered = [
            _line_runs(line, pal, cursor.x if first + i == cursor.y else None, links[i])
            for i, line in enumerate(lines)
        ]
        return {
            "lines": rendered, "fg": pal["fg"], "bg": pal["bg"],
            "firstLine": first, "availableFirstLine": available_first,
            "terminalEnd": terminal_end,
            "isLatest": before_line is None,
            **mouse,
        }
    except Exception:
        return None


# --- Active-session selection helpers ------------------------------------------

def current_session() -> iterm2.Session | None:
    window = itermapp.current_window if itermapp else None
    if window is None:
        return None
    tab = window.current_tab
    return tab.current_session if tab else None


def current_active_session_id() -> str | None:
    session = current_session()
    return session.session_id if session else None


def resolve_session(session_id: str | None) -> iterm2.Session | None:
    # Fail safe: a missing/invalid/unknown id resolves to nothing rather than
    # silently falling back to whatever pane happens to be focused — otherwise a
    # malformed request would send keystrokes to the pane the user is actively
    # using.
    if itermapp is not None and session_id and session_id != "undefined":
        return itermapp.get_session_by_id(session_id)
    return None


def _valid_input(value) -> str | None:
    """Return text safe to forward to a session, or None to reject it.

    Guards the mutating handlers against non-string payloads and oversized
    floods from an authenticated-but-hostile client (see MAX_INPUT_BYTES).
    """
    if not isinstance(value, str):
        return None
    if len(value.encode("utf-8", "ignore")) > MAX_INPUT_BYTES:
        return None
    return value


def _image_uti(raw: bytes) -> str | None:
    """The pasteboard UTI for a known image format, from its magic bytes."""
    if raw.startswith(b"\x89PNG\r\n\x1a\n"):
        return "public.png"
    if raw[:3] == b"\xff\xd8\xff":
        return "public.jpeg"
    if raw[:4] in (b"II*\x00", b"MM\x00*"):
        return "public.tiff"
    if raw[:6] in (b"GIF87a", b"GIF89a"):
        return "com.compuserve.gif"
    return None


def set_clipboard_image(raw: bytes) -> bool:
    """Write raw image bytes to the Mac pasteboard under their native type.

    Writing the original bytes (rather than round-tripping through NSImage,
    whose lazy TIFF conversion fails in this headless process) preserves the
    image exactly, which is what Claude Code reads on its Ctrl+V image paste.
    AppKit is imported lazily so importing this module on a non-macOS host (e.g.
    for tests) never requires it.
    """
    uti = _image_uti(raw)
    if uti is None:
        return False
    try:
        from AppKit import NSPasteboard
        from Foundation import NSData
    except ImportError:
        return False
    nsdata = NSData.dataWithBytes_length_(raw, len(raw))
    pasteboard = NSPasteboard.generalPasteboard()
    pasteboard.clearContents()
    return bool(pasteboard.setData_forType_(nsdata, uti))


def decode_image_payload(value) -> bytes | None:
    """Decode a base64 (optionally data-URL) image payload, bounded in size."""
    if not isinstance(value, str) or not value:
        return None
    if value.startswith("data:"):
        _, _, value = value.partition(",")
    try:
        raw = base64.b64decode(value, validate=True)
    except (binascii.Error, ValueError):
        return None
    if not raw or len(raw) > MAX_IMAGE_BYTES:
        return None
    return raw


def pasteboard_change_count() -> int | None:
    try:
        from AppKit import NSPasteboard
    except ImportError:
        return None
    return int(NSPasteboard.generalPasteboard().changeCount())


def pasteboard_text() -> str | None:
    try:
        from AppKit import NSPasteboard, NSPasteboardTypeString
    except ImportError:
        return None
    text = NSPasteboard.generalPasteboard().stringForType_(NSPasteboardTypeString)
    return None if text is None else str(text)


async def wait_for_clipboard_change(
        since: int, timeout: float = 1.5, interval: float = 0.05,
) -> str | None:
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    while True:
        count = pasteboard_change_count()
        if count is None:
            return None
        if count != since:
            return pasteboard_text()
        if loop.time() >= deadline:
            return None
        await asyncio.sleep(interval)


# --- Live streaming of watched sessions ----------------------------------------
#
# The client tells us which sessions it is viewing (the `watch` event); we run
# one screen-streamer task per distinct watched session and push its content on
# every change. This is decoupled from iTerm's own focus, so the phone can watch
# any pane — including a background split pane — and still get live updates.

def note_keystroke(session_id: str) -> None:
    keystroke_at[session_id] = asyncio.get_running_loop().time()


@contextlib.asynccontextmanager
async def screen_updates(session_id: str):
    changed = asyncio.Event()

    async def on_update(_connection, _message):
        changed.set()

    token = await iterm2.notifications.async_subscribe_to_screen_update_notification(
        connection, on_update, session_id)
    try:
        yield changed
    finally:
        try:
            await iterm2.notifications.async_unsubscribe(connection, token)
        except iterm2.notifications.SubscriptionException:
            pass


async def stream_session(session_id: str) -> None:
    session = itermapp.get_session_by_id(session_id) if itermapp else None
    if session is None:
        return

    content = await read_content(session_id)
    if content is not None and content["lines"] and clients:
        last_content[session_id] = content
        queue_content_for_watchers(session_id, content)

    loop = asyncio.get_running_loop()
    last_read = loop.time()
    try:
        async with screen_updates(session_id) as changed:
            while True:
                await changed.wait()
                now = loop.time()
                wait = last_read + STREAM_MIN_INTERVAL - now
                typed = now - keystroke_at.get(session_id, float("-inf")) <= KEYSTROKE_WINDOW
                if wait > 0 and not typed:
                    await asyncio.sleep(wait)
                changed.clear()
                last_read = loop.time()
                if not clients:
                    continue
                content = await read_content(session_id)
                if content is not None and content != last_content.get(session_id):
                    last_content[session_id] = content
                    queue_content_for_watchers(session_id, content)
    except asyncio.CancelledError:
        raise
    except Exception as err:
        print(f"stream error ({session_id}): {err}")
    finally:
        last_content.pop(session_id, None)


async def apply_watches() -> None:
    """Reconcile running stream tasks with the union of watched sessions."""
    async with stream_reconcile_lock:
        union: set[str] = set()
        for ids in watched_by_sid.values():
            union |= ids

        finished = []
        for session_id, task in list(stream_tasks.items()):
            if session_id not in union:
                task.cancel()
                finished.append(task)
                stream_tasks.pop(session_id, None)
            elif task.done():
                finished.append(task)
                stream_tasks.pop(session_id, None)  # restart below if wanted

        if finished:
            await asyncio.gather(*finished, return_exceptions=True)

        if clients:
            for session_id in union:
                if session_id not in stream_tasks:
                    stream_tasks[session_id] = asyncio.create_task(
                        stream_session(session_id))


async def stop_all_streams() -> None:
    async with stream_reconcile_lock:
        tasks = list(stream_tasks.values())
        stream_tasks.clear()
        for task in tasks:
            task.cancel()
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)


# --- iTerm2 notification monitors (replace the old poll loops) -----------------

async def layout_monitor() -> None:
    async with iterm2.LayoutChangeMonitor(connection) as monitor:
        while True:
            await monitor.async_get()
            await push_state()


async def focus_monitor() -> None:
    async with iterm2.FocusMonitor(connection) as monitor:
        while True:
            await monitor.async_get_next_update()
            await push_state()


# --- Socket.IO handlers ---------------------------------------------------------

@sio.event
async def connect(sid, environ, auth=None):
    supplied_key = auth.get("key") if isinstance(auth, dict) else None
    if not is_valid_key(shared_key, supplied_key):
        print(f"Rejected unauthenticated client: {sid}")
        raise socketio.exceptions.ConnectionRefusedError("invalid access key")
    clients.add(sid)
    start_client_delivery(sid)
    print(f"Client connected: {sid}")
    await sio.emit("screenSize", screen_size, to=sid)
    await sio.emit("state", await build_state(), to=sid)
    # Seed the view with a snapshot of whatever iTerm currently has focused, so
    # there's instant output before the client sends its first `watch`.
    session_id = current_active_session_id()
    if session_id:
        content = await read_content(session_id)
        if content and content["lines"]:
            await sio.emit(
                "content", {"sessionId": session_id, **content}, to=sid)


@sio.event
async def disconnect(sid, reason=None):
    clients.discard(sid)
    watched_by_sid.pop(sid, None)
    await stop_client_delivery(sid)
    print(f"Client disconnected: {sid}")
    if not clients:
        await stop_all_streams()
    else:
        await apply_watches()


@sio.on("watch")
async def on_watch(sid, data):
    # The client lists the sessions it is currently viewing (main + split panes);
    # we live-stream exactly that union across all clients.
    ids = data.get("sessionIds") or []
    watched_by_sid[sid] = {
        s for s in ids[:MAX_WATCHED_SESSIONS] if s and s != "undefined"
    }
    await apply_watches()


@sio.event
async def ping(sid):
    # Returning sends the Socket.IO ack with no args -> client latency callback.
    return


@sio.on("getContent")
async def on_get_content(sid, data):
    session_id = data.get("sessionId")
    content = await read_content(session_id)
    if content and content["lines"]:
        queue_client_event(
            sid, f"content:{session_id}", "content",
            {"sessionId": session_id, **content})


@sio.on("getAllContent")
async def on_get_all_content(sid, data):
    session_ids = data.get("sessionIds") or []

    async def one(session_id):
        content = await read_content(session_id, PREVIEW_CONTENT_LINES)
        if content and content["lines"]:
            queue_client_event(
                sid, f"content:{session_id}", "content",
                {"sessionId": session_id, **content})

    await asyncio.gather(*(one(s) for s in session_ids))


@sio.on("getEarlierContent")
async def on_get_earlier_content(sid, data):
    session_id = data.get("sessionId")
    before_line = data.get("beforeLine")
    if not isinstance(before_line, int):
        return
    content = await read_content(
        session_id, HISTORY_PAGE_LINES, before_line=before_line)
    if content:
        queue_client_event(
            sid, f"history:{session_id}:{before_line}", "historyContent",
            {"sessionId": session_id, **content})


@sio.on("execute")
async def on_execute(sid, data):
    command = _valid_input(data.get("command", ""))
    if command is None:
        return
    session = resolve_session(data.get("sessionId"))
    if session is not None:
        # CR (0x0D), not LF: a real Return submits in shells (ICRNL maps CR->NL)
        # and in raw-mode TUIs like Claude Code, where LF only inserts a newline.
        await session.async_send_text(command + "\r")


@sio.on("broadcast")
async def on_broadcast(sid, data):
    command = _valid_input(data.get("command", ""))
    if command is None:
        return
    session_ids = data.get("sessionIds") or []
    sessions = [itermapp.get_session_by_id(s) for s in session_ids] if itermapp else []
    await asyncio.gather(*(
        s.async_send_text(command + "\r") for s in sessions if s is not None))
    await push_state()


@sio.on("sendKeys")
async def on_send_keys(sid, data):
    keys = _valid_input(data.get("keys", ""))
    if keys is None:
        return
    session = resolve_session(data.get("sessionId"))
    if session is not None:
        note_keystroke(session.session_id)
        await session.async_send_text(keys)


@sio.on("pasteImage")
async def on_paste_image(sid, data):
    # Bridge a client-pasted image onto the Mac pasteboard, then send Ctrl+V so
    # the focused app (e.g. Claude Code's image paste) ingests it — the image
    # bytes never travel through the TTY.
    raw = decode_image_payload(data.get("data"))
    if raw is None or not set_clipboard_image(raw):
        return
    session = resolve_session(data.get("sessionId"))
    if session is not None:
        await session.async_send_text("\x16")  # Ctrl+V


@sio.on("clipboardCount")
async def on_clipboard_count(sid):
    return pasteboard_change_count()


@sio.on("clipboardSince")
async def on_clipboard_since(sid, data):
    since = data.get("count") if isinstance(data, dict) else None
    if not isinstance(since, int):
        return None
    return await wait_for_clipboard_change(since)


@sio.on("clipboardText")
async def on_clipboard_text(sid):
    return pasteboard_text()


@sio.on("newTab")
async def on_new_tab(sid):
    window = itermapp.current_window if itermapp else None
    if window is not None:
        await window.async_create_tab()
    await push_state()


def find_tab(tab_id: str):
    if itermapp is None or not tab_id:
        return None
    for window in itermapp.windows:
        for tab in window.tabs:
            if tab.tab_id == tab_id:
                return tab
    return None


@sio.on("closeTab")
async def on_close_tab(sid, data=None):
    # Refuse to close the last remaining tab: it would close iTerm's last window
    # (potentially quitting the app) and drop the API connection, which looks
    # like the server "hanging". The client also hides close on a lone tab.
    if itermapp is not None and sum(len(w.tabs) for w in itermapp.windows) <= 1:
        return
    tab_id = data.get("tabId") if isinstance(data, dict) else None
    try:
        if tab_id:
            tab = find_tab(tab_id)
            if tab is not None:
                await tab.async_close(force=True)
        else:
            session = current_session()
            if session is not None:
                await session.async_close()
    except Exception as err:
        print(f"closeTab error: {err}")
    await push_state()


@sio.on("renameSession")
async def on_rename_session(sid, data):
    session_id = data.get("sessionId")
    name = data.get("name")
    if itermapp is None or not session_id or session_id == "undefined" or not name:
        return
    session = itermapp.get_session_by_id(session_id)
    if session is not None:
        await session.async_set_name(name)
    await push_state()


@sio.on("focus")
async def on_focus(sid, data):
    if itermapp is None:
        return
    window_id = data.get("windowId")
    tab_index = data.get("tabIndex") or 0
    if not window_id:
        return
    window = itermapp.get_window_by_id(str(window_id))
    if window is None:
        return

    await window.async_activate()
    await itermapp.async_activate()

    if 1 <= tab_index <= len(window.tabs):
        await window.tabs[tab_index - 1].async_activate()

    await push_state()


# --- Entry point ---------------------------------------------------------------

async def connection_watchdog(stop: asyncio.Event) -> None:
    """Exit the process when iTerm2's API connection dies.

    The iTerm2 Python API has no reconnection: if iTerm2 quits, every call
    against the dead websocket hangs or errors and the server wedges (ignoring
    even `stop`/`restart`). Probing with a cheap refresh and tripping `stop` on
    failure lets the process exit cleanly so a supervisor (launchd KeepAlive)
    can relaunch it once iTerm2 is back.
    """
    while not stop.is_set():
        await asyncio.sleep(5)
        try:
            await itermapp.async_refresh()
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            print(f"iTerm2 connection lost ({exc!r}); shutting down for restart")
            stop.set()
            return


async def main() -> None:
    global connection, itermapp, screen_size, shared_key
    screen_size = read_screen_size()
    print("Screen size:", screen_size)

    shared_key = load_or_create_key()

    connection = await iterm2.Connection.async_create()
    itermapp = await iterm2.async_get_app(connection)

    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, "0.0.0.0", PORT)
    await site.start()
    print(f"Server running on http://0.0.0.0:{PORT}")

    # Clean shutdown (incl. `iterm-server stop`, which sends SIGTERM).
    stop = asyncio.Event()

    background_tasks = [
        asyncio.create_task(layout_monitor()),
        asyncio.create_task(focus_monitor()),
        asyncio.create_task(sync_loop()),
        asyncio.create_task(health_loop()),
        asyncio.create_task(connection_watchdog(stop)),
    ]

    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            loop.add_signal_handler(sig, stop.set)
        except (NotImplementedError, RuntimeError):
            pass
    try:
        await stop.wait()
    finally:
        for task in background_tasks:
            task.cancel()
        await asyncio.gather(*background_tasks, return_exceptions=True)
        await stop_all_streams()
        for sid in list(delivery_tasks):
            clients.discard(sid)
            await stop_client_delivery(sid)
        await runner.cleanup()


if __name__ == "__main__":
    asyncio.run(main())
