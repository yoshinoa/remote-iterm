import asyncio
import base64
import unittest
from unittest import mock

from server import server as server_module
from server.server import (
    _DEFAULT_PALETTE,
    _content_line_range,
    _image_uti,
    _line_runs,
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
    watched_by_sid,
)


class _DefaultColor:
    is_rgb = False
    is_standard = False


class _Style:
    def __init__(self, *, faint=False):
        self.fg_color = _DefaultColor()
        self.bg_color = _DefaultColor()
        self.bold = False
        self.faint = faint
        self.inverse = False


class _Line:
    def __init__(self, text, faint_at=()):
        self.text = text
        faint_at = set(faint_at)
        self.styles = [_Style(faint=i in faint_at) for i in range(len(text))]

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
