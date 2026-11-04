import asyncio
import base64
import unittest
from types import SimpleNamespace
from unittest import mock

from server import server as server_module
from server.server import (
    _DEFAULT_PALETTE,
    _content_line_range,
    _image_uti,
    _line_runs,
    _wrapped_links,
    _valid_input,
    client_origins,
    decode_image_payload,
    MAX_IMAGE_BYTES,
    MAX_INPUT_BYTES,
    clients,
    delivery_wakeups,
    pending_events,
    queue_client_event,
    queue_content_for_watchers,
    resolve_session,
    wait_for_clipboard_change,
    watched_by_sid,
)


class _DefaultColor:
    is_rgb = False
    is_standard = False


class _Url:
    def __init__(self, url):
        self.url = url


class _Style:
    def __init__(self, *, faint=False, url=None):
        self.fg_color = _DefaultColor()
        self.bg_color = _DefaultColor()
        self.bold = False
        self.faint = faint
        self.inverse = False
        self.url = _Url(url) if url else None


class _Line:
    def __init__(self, text, faint_at=(), url_at=None, hard_eol=True):
        self.text = text
        self.hard_eol = hard_eol
        faint_at = set(faint_at)
        url_at = url_at or {}
        self.styles = [_Style(faint=i in faint_at, url=url_at.get(i))
                       for i in range(len(text))]

    def style_at(self, index):
        return self.styles[index] if index < len(self.styles) else None

    def string_at(self, index):
        return self.text[index]


class LineRunsTest(unittest.TestCase):
    def test_marks_cursor_without_shifting_text(self):
        self.assertEqual(
            _line_runs(_Line("abc"), _DEFAULT_PALETTE, cursor_x=1),
            [{"t": "a"}, {"t": "", "c": True}, {"t": "bc"}],
        )

    def test_preserves_cursor_on_an_otherwise_blank_line(self):
        self.assertEqual(
            _line_runs(_Line("    "), _DEFAULT_PALETTE, cursor_x=2),
            [{"t": "  "}, {"t": "", "c": True}],
        )

    def test_preserves_faint_as_a_distinct_style(self):
        self.assertEqual(
            _line_runs(_Line("ab", faint_at={1}), _DEFAULT_PALETTE),
            [{"t": "a"}, {"t": "b", "d": True}],
        )

    def test_carries_osc8_hyperlinks(self):
        url = "https://example.com/ad?id=5"
        self.assertEqual(
            _line_runs(_Line("see Preview", url_at={i: url for i in range(4, 11)}),
                       _DEFAULT_PALETTE),
            [{"t": "see "}, {"t": "Preview", "u": url}],
        )

    def test_links_bare_urls_without_trailing_punctuation(self):
        self.assertEqual(
            _line_runs(_Line("open http://localhost:5173/a. now"), _DEFAULT_PALETTE),
            [{"t": "open "},
             {"t": "http://localhost:5173/a", "u": "http://localhost:5173/a"},
             {"t": ". now"}],
        )

    def test_drops_hyperlinks_a_browser_cannot_open(self):
        self.assertEqual(
            _line_runs(_Line("img", url_at={i: "file:///tmp/a.png" for i in range(3)}),
                       _DEFAULT_PALETTE),
            [{"t": "img"}],
        )


class WrappedLinksTest(unittest.TestCase):
    def _render(self, lines):
        return [_line_runs(line, _DEFAULT_PALETTE, links=links)
                for line, links in zip(lines, _wrapped_links(lines))]

    def test_links_a_soft_wrapped_url_to_its_full_target(self):
        url = "https://example.com/oauth?code=abc"
        lines = [_Line("go " + url[:12], hard_eol=False), _Line(url[12:] + " ok")]
        self.assertEqual(self._render(lines), [
            [{"t": "go "}, {"t": url[:12], "u": url}],
            [{"t": url[12:], "u": url}, {"t": " ok"}],
        ])

    def test_does_not_join_across_hard_newlines(self):
        lines = [_Line("https://a.com/x"), _Line("yz")]
        self.assertEqual(self._render(lines), [
            [{"t": "https://a.com/x", "u": "https://a.com/x"}],
            [{"t": "yz"}],
        ])


class ContentLineRangeTest(unittest.TestCase):
    def test_returns_latest_bounded_page(self):
        class Info:
            overflow = 275
            scrollback_buffer_height = 10_000
            mutable_area_height = 40

        self.assertEqual(
            _content_line_range(Info(), 250),
            (10_065, 250, 275, 10_315),
        )

    def test_pages_back_to_the_first_retained_line(self):
        class Info:
            overflow = 275
            scrollback_buffer_height = 10_000
            mutable_area_height = 40

        self.assertEqual(
            _content_line_range(Info(), 500, before_line=600),
            (275, 325, 275, 10_315),
        )

    def test_alternate_screen_exposes_only_the_screen(self):
        class Info:
            overflow = 275
            scrollback_buffer_height = 10_000
            mutable_area_height = 40

        self.assertEqual(
            _content_line_range(Info(), 250, alternate_screen=True),
            (10_275, 40, 10_275, 10_315),
        )


class BoundedDeliveryTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        clients.update({"watching", "other"})
        watched_by_sid.update({"watching": {"session-1"}, "other": {"session-2"}})
        for sid in clients:
            pending_events[sid] = {}
            delivery_wakeups[sid] = asyncio.Event()

    async def asyncTearDown(self):
        clients.clear()
        watched_by_sid.clear()
        pending_events.clear()
        delivery_wakeups.clear()

    async def test_replaces_an_undelivered_snapshot_instead_of_growing(self):
        queue_client_event("watching", "content:session-1", "content", {"value": 1})
        queue_client_event("watching", "content:session-1", "content", {"value": 2})

        self.assertEqual(
            pending_events["watching"],
            {"content:session-1": ("content", {"value": 2})},
        )

    async def test_terminal_content_is_queued_only_for_its_watchers(self):
        queue_content_for_watchers("session-1", {"lines": ["latest"]})

        self.assertEqual(len(pending_events["watching"]), 1)
        self.assertEqual(pending_events["other"], {})


class _FakeStreamer:
    def __init__(self, notifications: asyncio.Queue):
        self.notifications = notifications

    async def __aenter__(self):
        return self

    async def __aexit__(self, *exc):
        return False

    async def async_get(self):
        return await self.notifications.get()


class _FakeStreamSession:
    def __init__(self):
        self.notifications = asyncio.Queue()

    def get_screen_streamer(self, want_contents=False):
        return _FakeStreamer(self.notifications)


class StreamThrottleTest(unittest.IsolatedAsyncioTestCase):
    INTERVAL = 0.2

    async def asyncSetUp(self):
        self.session = _FakeStreamSession()
        self.sent: list[float] = []
        self.reads = 0
        loop = asyncio.get_running_loop()

        async def fake_read_content(session_id):
            self.reads += 1
            return {"lines": [[{"t": str(self.reads)}]]}

        app = mock.Mock()
        app.get_session_by_id.return_value = self.session
        clients.add("watching")
        self.patches = [
            mock.patch.object(server_module, "itermapp", app),
            mock.patch.object(server_module, "read_content", fake_read_content),
            mock.patch.object(server_module, "queue_content_for_watchers",
                              lambda sid, content: self.sent.append(loop.time())),
            mock.patch.object(server_module, "STREAM_MIN_INTERVAL", self.INTERVAL),
        ]
        for p in self.patches:
            p.start()
        self.task = asyncio.create_task(server_module.stream_session("session-1"))
        await asyncio.sleep(0)

    async def asyncTearDown(self):
        self.task.cancel()
        await asyncio.gather(self.task, return_exceptions=True)
        for p in self.patches:
            p.stop()
        clients.clear()
        server_module.keystroke_at.clear()

    async def test_first_change_after_a_quiet_spell_is_sent_immediately(self):
        await asyncio.sleep(self.INTERVAL * 1.5)
        before = len(self.sent)
        changed_at = asyncio.get_running_loop().time()
        self.session.notifications.put_nowait(None)
        await asyncio.sleep(self.INTERVAL / 4)

        self.assertEqual(len(self.sent), before + 1)
        self.assertLess(self.sent[-1] - changed_at, self.INTERVAL / 4)

    async def test_a_burst_of_changes_is_spaced_by_the_min_interval(self):
        await asyncio.sleep(self.INTERVAL * 1.5)
        before = len(self.sent)
        for _ in range(3):
            self.session.notifications.put_nowait(None)
            await asyncio.sleep(0.01)
        await asyncio.sleep(self.INTERVAL * 3.5)

        burst = self.sent[before:]
        self.assertEqual(len(burst), 3)
        for earlier, later in zip(burst, burst[1:]):
            self.assertGreaterEqual(later - earlier, self.INTERVAL * 0.95)

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


class GridFieldsTest(unittest.TestCase):
    def test_reports_columns_and_rows(self):
        session = SimpleNamespace(grid_size=SimpleNamespace(width=120, height=40))
        self.assertEqual(server_module._grid_fields(session), {"columns": 120, "rows": 40})

    def test_missing_grid_size_gives_nothing(self):
        self.assertEqual(server_module._grid_fields(SimpleNamespace()), {})


class InputValidationTest(unittest.TestCase):
    def test_accepts_ordinary_text(self):
        self.assertEqual(_valid_input("ls -la"), "ls -la")

    def test_accepts_text_at_the_size_cap(self):
        at_cap = "a" * MAX_INPUT_BYTES
        self.assertEqual(_valid_input(at_cap), at_cap)

    def test_rejects_text_over_the_size_cap(self):
        self.assertIsNone(_valid_input("a" * (MAX_INPUT_BYTES + 1)))

    def test_measures_the_cap_in_bytes_not_characters(self):
        # A 4-byte codepoint just past the cap in bytes must be rejected.
        self.assertIsNone(_valid_input("😀" * (MAX_INPUT_BYTES // 4 + 1)))

    def test_rejects_non_string_payloads(self):
        for bad in (None, 123, ["ls"], {"command": "ls"}):
            self.assertIsNone(_valid_input(bad))


class _FakeApp:
    def __init__(self, sessions):
        self._sessions = sessions

    def get_session_by_id(self, session_id):
        return self._sessions.get(session_id)


class ResolveSessionTest(unittest.TestCase):
    def setUp(self):
        self._orig = server_module.itermapp

    def tearDown(self):
        server_module.itermapp = self._orig

    def test_returns_the_named_session(self):
        session = object()
        server_module.itermapp = _FakeApp({"s1": session})
        self.assertIs(resolve_session("s1"), session)

    def test_never_falls_back_to_the_focused_session(self):
        # A focused session exists, but missing/invalid/unknown ids must resolve
        # to None rather than silently targeting whatever pane is in focus.
        server_module.itermapp = _FakeApp({"focused": object()})
        for bad in (None, "", "undefined", "unknown-id"):
            self.assertIsNone(resolve_session(bad))

    def test_returns_none_when_iterm_is_unavailable(self):
        server_module.itermapp = None
        self.assertIsNone(resolve_session("s1"))


_PNG_B64 = (
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4"
    "2mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg=="
)


class ImageUtiTest(unittest.TestCase):
    def test_detects_common_formats(self):
        self.assertEqual(_image_uti(b"\x89PNG\r\n\x1a\n rest"), "public.png")
        self.assertEqual(_image_uti(b"\xff\xd8\xff\xe0 rest"), "public.jpeg")
        self.assertEqual(_image_uti(b"GIF89a rest"), "com.compuserve.gif")

    def test_rejects_unknown_bytes(self):
        self.assertIsNone(_image_uti(b"not an image"))


class DecodeImagePayloadTest(unittest.TestCase):
    def test_decodes_a_png_payload(self):
        raw = decode_image_payload(_PNG_B64)
        self.assertIsNotNone(raw)
        self.assertTrue(raw.startswith(b"\x89PNG\r\n\x1a\n"))

    def test_strips_a_data_url_prefix(self):
        self.assertEqual(
            decode_image_payload("data:image/png;base64," + _PNG_B64),
            decode_image_payload(_PNG_B64),
        )

    def test_rejects_invalid_base64(self):
        self.assertIsNone(decode_image_payload("not!valid!base64!"))

    def test_rejects_oversized_images(self):
        oversized = base64.b64encode(b"\x89PNG\r\n\x1a\n" + b"0" * MAX_IMAGE_BYTES)
        self.assertIsNone(decode_image_payload(oversized.decode()))

    def test_rejects_non_string_or_empty(self):
        for bad in (None, b"bytes", "", 123):
            self.assertIsNone(decode_image_payload(bad))

    def test_transport_carries_the_largest_accepted_image(self):
        data_url = "data:image/png;base64," + "A" * (4 * -(-MAX_IMAGE_BYTES // 3))
        packet = '42["pasteImage",{"sessionId":"%s","data":"%s"}]' % ("x" * 64, data_url)
        self.assertGreaterEqual(server_module.sio.eio.max_http_buffer_size, len(packet))


class WaitForClipboardChangeTest(unittest.IsolatedAsyncioTestCase):
    async def test_returns_text_once_the_pasteboard_changes(self):
        counts = iter([7, 7, 8])
        with mock.patch.object(server_module, "pasteboard_change_count", lambda: next(counts)), \
                mock.patch.object(server_module, "pasteboard_text", lambda: "copied"):
            text = await wait_for_clipboard_change(7, timeout=1, interval=0)
        self.assertEqual(text, "copied")

    async def test_returns_none_when_nothing_is_copied(self):
        with mock.patch.object(server_module, "pasteboard_change_count", lambda: 7), \
                mock.patch.object(server_module, "pasteboard_text", lambda: "stale"):
            text = await wait_for_clipboard_change(7, timeout=0.05, interval=0.01)
        self.assertIsNone(text)

    async def test_returns_none_without_a_pasteboard(self):
        with mock.patch.object(server_module, "pasteboard_change_count", lambda: None):
            self.assertIsNone(await wait_for_clipboard_change(7, timeout=1, interval=0))


class ClipboardTextTest(unittest.IsolatedAsyncioTestCase):
    async def test_returns_the_current_pasteboard_text(self):
        with mock.patch.object(server_module, "pasteboard_text", lambda: "from pbcopy"):
            self.assertEqual(await server_module.on_clipboard_text("sid"), "from pbcopy")

    async def test_returns_none_without_a_pasteboard(self):
        with mock.patch.object(server_module, "pasteboard_text", lambda: None):
            self.assertIsNone(await server_module.on_clipboard_text("sid"))


class ClientOriginsTest(unittest.TestCase):
    def test_always_allows_localhost(self):
        with mock.patch.dict("os.environ", {"REMOTE_ITERM_ORIGINS": ""}, clear=False):
            origins = client_origins()
        self.assertIn("http://localhost:7292", origins)
        self.assertIn("http://127.0.0.1:7292", origins)

    def test_appends_configured_proxy_origins(self):
        with mock.patch.dict(
            "os.environ",
            {"REMOTE_ITERM_ORIGINS": "https://term.example.com, https://a.b"},
            clear=False,
        ):
            origins = client_origins()
        self.assertIn("https://term.example.com", origins)
        self.assertIn("https://a.b", origins)

    def test_does_not_use_wildcard(self):
        self.assertNotIn("*", client_origins())


if __name__ == "__main__":
    unittest.main()
