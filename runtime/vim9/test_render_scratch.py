#!/usr/bin/env python3
"""Real-Vim native rendering and input for ordinary plugin scratch buffers."""

from pathlib import Path
import tempfile
import unittest

from test_bridge import Editor, VIM


@unittest.skipUnless(VIM, "Install Vim 9 for native scratch-buffer tests")
class RenderScratchTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vim9-scratch-")
        self.root = Path(self.directory.name)
        self.editor = Editor(self.root, authenticate=False)
        self.editor.send({"action": "auth", "token": self.editor.token,
                          "protocol": 1, "render_protocol": 5})
        self.editor.wait(lambda message: message.get("event") == "state")

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    def scratch(self, kind):
        self.scratch_path = self.root / ("plugin-scratch-" + kind + ".txt")
        self.editor.ex("enew! | setlocal buftype=" + kind
                       + " bufhidden=hide noswapfile nowrap nolist modifiable noreadonly")
        self.editor.ex("file " + self.scratch_path.name + " | call setline(1, "
                       "map(range(1,160), {_,n -> printf('%03d 界é', n)}))")
        self.editor.ex("set mouse=a scrolloff=0 | normal! 30Gzt0")

    def native(self):
        frame = self.editor.gui_frame()
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        return frame

    def window(self):
        frame = self.native()
        return next(w for w in frame["windows"] if w["window"] == frame["window"])

    @staticmethod
    def source(window):
        return {key: window[key] for key in ("window", "buffer", "changedtick")}

    def test_plain_scratch_kinds_keep_native_unicode_and_vim_options(self):
        for kind in ("nofile", "nowrite", "acwrite"):
            with self.subTest(kind=kind):
                self.scratch(kind)
                self.editor.ex("syntax match ScratchWord /界/ | "
                               "highlight ScratchWord guifg=#12ab34 ctermfg=1")
                before = self.editor.snapshot()
                frame = self.native()
                self.assertTrue(any(r["text"] == "界" and r["style"]["foreground"] == 0x12ab34
                                    for r in frame["runs"]))
                self.assertTrue(any(r["text"] == "é" for r in frame["runs"]))
                self.assertEqual(frame["changedtick"], before["changedtick"])
                self.editor.ex("call writefile([&l:buftype, string(&l:modifiable), "
                               "string(&l:readonly)], 'scratch-options.txt')")
                self.assertEqual((self.root / "scratch-options.txt").read_text().splitlines(),
                                 [kind, "1", "0"])

    def test_scratch_precision_scroll_and_source_pointer_keep_identity(self):
        self.scratch("nofile")
        w = self.window()
        self.assertTrue(w["native_pointer"])
        self.assertTrue(w["native_scroll"])
        reply = self.editor.request("scroll_view", **self.source(w),
                                    topline=w["topline"], leftcol=w["leftcol"], lines=1)
        after = self.window()
        self.assertEqual(after["scroll_ack"], reply["id"])
        self.assertEqual(after["topline"], w["topline"] + 1)
        self.assertEqual(self.source(after), self.source(w))
        for phase in ("press", "release"):
            self.editor.request("pointer", **self.source(w), phase=phase, gesture=1,
                                line=31, byte_column=4)
        state = self.editor.snapshot()
        self.assertEqual((state["line"], state["column"]), (31, 5))
        self.editor.ex("call setline(31, 'changed source')")
        self.editor.request("pointer", **self.source(w), phase="press", gesture=2,
                            line=32, byte_column=0, ok=False)
        self.assertEqual(self.editor.snapshot()["line"], 31)

    def test_non_writable_scratch_save_does_not_create_a_real_file(self):
        for kind in ("nofile", "nowrite"):
            with self.subTest(kind=kind):
                self.scratch(kind)
                self.native()
                # Vim does not mark these buffers modified, so :update is
                # normally a successful no-op; it must never become a write.
                self.editor.request("save")
                self.assertFalse(self.scratch_path.exists())
                self.editor.ex("call writefile([getline(31), &l:buftype], 'scratch-preserved.txt')")
                self.assertEqual((self.root / "scratch-preserved.txt").read_text().splitlines(),
                                 ["031 界é", kind])

    def test_acwrite_uses_the_existing_plugin_write_callback(self):
        self.scratch("acwrite")
        self.editor.ex("augroup ScratchWrite | autocmd! | "
                       "autocmd BufWriteCmd <buffer> call writefile(getline(1,'$'), "
                       "'plugin-saved.txt') | setlocal nomodified")
        self.editor.ex("augroup END")
        before = self.native()
        self.editor.request("save")
        self.assertFalse(self.editor.snapshot()["modified"])
        self.assertEqual((self.root / "plugin-saved.txt").read_text().splitlines()[30], "031 界é")
        self.assertFalse(self.scratch_path.exists())
        self.assertEqual(self.native()["buffer"], before["buffer"])

    def test_acwrite_without_a_handler_reports_vims_write_error(self):
        self.scratch("acwrite")
        before = self.native()
        self.editor.request("save", ok=False)
        self.assertFalse(self.scratch_path.exists())
        self.assertTrue(self.editor.snapshot()["modified"])
        self.assertEqual(self.native()["buffer"], before["buffer"])

    def test_readonly_scratch_remains_native_without_becoming_editable(self):
        self.scratch("nofile")
        self.editor.ex("setlocal readonly nomodifiable")
        before = self.native()
        w = self.window()
        self.assertTrue(w["native_pointer"])
        self.editor.request("pointer", **self.source(w), phase="press", gesture=1,
                            line=31, byte_column=0)
        self.editor.request("pointer", **self.source(w), phase="release", gesture=1,
                            line=31, byte_column=0)
        self.editor.keys("iX\x1b")
        self.editor.ex("call writefile([getline(31), string(&l:modifiable), "
                       "string(&l:readonly)], 'scratch-readonly.txt')")
        self.assertEqual((self.root / "scratch-readonly.txt").read_text().splitlines(),
                         ["031 界é", "0", "1"])
        self.assertEqual(self.native()["changedtick"], before["changedtick"])

    def test_special_input_and_unsupported_display_still_use_compatibility(self):
        self.scratch("nofile")
        self.editor.ex("setlocal spell")
        self.assertFalse(self.editor.gui_frame()["supported"])
        self.editor.ex("setlocal nospell")
        self.native()
        self.editor.ex("setlocal buftype=prompt")
        self.assertFalse(self.editor.gui_frame()["supported"])
        self.editor.ex("enew! | cgetexpr ['main.txt:1:1:example'] | copen")
        frame = self.editor.gui_frame()
        self.assertTrue(frame["supported"], frame.get("fallback_reason"))
        current = next(w for w in frame["windows"] if w["window"] == frame["window"])
        self.assertFalse(current["native_pointer"])
        self.assertFalse(current["native_scroll"])


if __name__ == "__main__":
    unittest.main()
