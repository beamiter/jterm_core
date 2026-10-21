#!/usr/bin/env python3
"""Real Vim v3 semantic margins; no viewport, cursor or edit mutations."""
import collections
import tempfile
import unittest
from pathlib import Path
from test_bridge import Editor, VIM


@unittest.skipUnless(VIM, "Install Vim 9 to test semantic margins")
class OverscanTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vim9-overscan-")
        self.editor = Editor(Path(self.directory.name), authenticate=False)
        e = self.editor
        e.send({"action":"auth", "token":e.token, "protocol":1, "render_protocol":3})
        e.wait(lambda m: m.get("event") == "state")
        e.ex("call setline(1, map(range(1, 160), {_, n -> printf('%03d 界 é', n)}))")
        e.ex("set nowrap scrolloff=0 number")
        e.ex("normal! 30Gzt")

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    def frame(self):
        frame = self.editor.gui_frame()
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        return frame

    def test_margins_are_bounded_source_rows_and_do_not_move_vim(self):
        e = self.editor
        before = e.snapshot()
        frame = self.frame()
        window = next(w for w in frame["windows"] if w["window"] == frame["window"])
        self.assertEqual(window["line_count"], 160)
        self.assertEqual([r["row"] for r in window["overscan"]], [-2, -1, window["view"]["rows"], window["view"]["rows"] + 1])
        for row in window["overscan"]:
            self.assertEqual(row["line"], window["topline"] + row["row"])
            self.assertGreater(row["byte_length"], 0)
            self.assertTrue(any(r["line"] == 0 for r in row["runs"]), "line number gutter")
            self.assertTrue(any("界" in r["text"] for r in row["runs"]))
            for run in row["runs"]:
                self.assertEqual(run["row"], 0)
                self.assertEqual(run["window"], window["window"])
                self.assertIn(run["line"], (0, row["line"]))
        after = e.snapshot()
        for key in ("window", "buffer", "cursor", "mode"):
            self.assertEqual(before.get(key), after.get(key), key)
        self.assertEqual(frame["changedtick"], self.frame()["changedtick"])

    def test_text_properties_keep_gui_styling_in_hidden_rows(self):
        e = self.editor
        e.ex("highlight MarginTest guifg=#123456 gui=underline")
        e.ex("call prop_type_add('margin', {'highlight':'MarginTest', 'override':1})")
        e.ex("call prop_add(29, 1, {'type':'margin', 'length':3})")
        frame = self.frame()
        row = next(r for r in frame["windows"][0]["overscan"] if r["line"] == 29)
        painted = next(r for r in row["runs"] if r["line"] == 29 and r["byte_start"] == 0)
        self.assertEqual(painted["style"]["foreground"], 0x123456)
        self.assertTrue(painted["style"]["underline"])

    def test_unsupported_hidden_row_does_not_disable_a_supported_visible_buffer(self):
        e = self.editor
        e.ex("call setline(29, nr2char(1))")
        frame = self.frame()
        self.assertEqual(frame["windows"][0]["overscan"], [])
        self.assertEqual(frame["windows"][0]["topline"], 30)
        self.assertTrue(frame["runs"])

    def test_plugin_matches_keep_byte_ranges_and_rgb_in_smooth_scroll_margins(self):
        e = self.editor
        e.ex("highlight MarginMatch guifg=#f1a234 guibg=#654321 gui=undercurl guisp=#ee3344")
        e.ex("call matchaddpos('MarginMatch', [[29,5,3]])")
        frame = self.frame()
        row = next(r for r in frame["windows"][0]["overscan"] if r["line"] == 29)
        glyph = next(r for r in row["runs"] if r["text"] == "界")
        self.assertEqual((glyph["byte_start"], glyph["byte_end"], glyph["cells"]), (4, 7, 2))
        self.assertEqual(glyph["style"]["foreground"], 0xf1a234)
        self.assertTrue(glyph["style"]["undercurl"])
        self.assertEqual(glyph["style"]["special"], 0xee3344)
        e.ex("call clearmatches()")
        row = next(r for r in self.frame()["windows"][0]["overscan"] if r["line"] == 29)
        glyph = next(r for r in row["runs"] if r["text"] == "界")
        self.assertNotEqual(glyph["style"]["foreground"], 0xf1a234)

    def test_empty_margin_is_explicit_and_edit_invalidates_it(self):
        e = self.editor
        e.ex("call setline(29, '')")
        before = self.frame()
        row = next(r for r in before["windows"][0]["overscan"] if r["line"] == 29)
        self.assertEqual(row["byte_length"], 0)
        e.ex("call setline(29, 'changed margin')")
        after = self.frame()
        self.assertNotEqual(before["changedtick"], after["changedtick"])
        row = next(r for r in after["windows"][0]["overscan"] if r["line"] == 29)
        self.assertEqual(row["byte_length"], len('changed margin'))
        self.assertIn('changed margin', ''.join(r["text"] for r in row["runs"] if r["line"] == 29))

    def test_split_margins_keep_their_window_identity(self):
        e = self.editor
        e.ex("vsplit")
        before = self.frame()
        active = before["window"]
        peer = next(w for w in before["windows"] if w["window"] != active)
        e.request("scroll", window=peer["window"], lines=4)
        after = self.frame()
        self.assertEqual(after["window"], active)
        for window in after["windows"]:
            for row in window["overscan"]:
                self.assertEqual(row["line"], window["topline"] + row["row"])
                self.assertTrue(all(r["window"] == window["window"] for r in row["runs"]))
        old_active = next(w for w in before["windows"] if w["window"] == active)
        new_active = next(w for w in after["windows"] if w["window"] == active)
        self.assertEqual(old_active["topline"], new_active["topline"])

    def test_document_edges_never_fabricate_source_lines(self):
        e = self.editor
        e.ex("normal! ggzt")
        frame = self.frame()
        self.assertTrue(all(r["row"] >= 0 for r in frame["windows"][0]["overscan"]))
        e.ex("normal! Gzb")
        frame = self.frame()
        window = frame["windows"][0]
        self.assertTrue(all(1 <= r["line"] <= 160 for r in window["overscan"]))
        self.assertTrue(all(r["row"] < 0 for r in window["overscan"]))

    def test_v3_delta_carries_fresh_margin_metadata(self):
        e = self.editor
        before = self.frame()
        e.messages = collections.deque()
        e.keys('l')
        message = e.wait(lambda m: m.get('event') in ('render', 'render_delta') and m.get('revision', 0) > before['revision'])
        self.assertIn('overscan', message['windows'][0])
        self.assertEqual(message['windows'][0]['overscan'], before['windows'][0]['overscan'])


if __name__ == '__main__':
    unittest.main()
