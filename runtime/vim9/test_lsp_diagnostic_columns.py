#!/usr/bin/env python3
"""Real bridge diagnostics keep LSP UTF-16 positions and source byte columns."""

import json
from pathlib import Path
import tempfile
import unittest

from test_bridge import Editor, VIM


@unittest.skipUnless(VIM, "Install Vim 9 for LSP diagnostic column tests")
class LspDiagnosticColumnTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vim9-lsp-columns-")
        self.root = Path(self.directory.name)
        self.provider = self.root / "autoload" / "lsp" / "diag.vim"
        self.provider.parent.mkdir(parents=True)
        self.provider.write_text(
            "function! lsp#diag#GetDiagsForBuf(bufnr) abort\n"
            "  return g:column_test_diagnostics\n"
            "endfunction\n", encoding="utf-8")
        self.editor = Editor(self.root)
        self.editor.ex("let g:column_test_diagnostics = [] | source " + str(self.provider))

    def tearDown(self):
        self.editor.close()
        self.directory.cleanup()

    def publish(self, text, positions):
        fixture = self.root / "diagnostics.json"
        fixture.write_text(json.dumps({
            "text": text,
            "items": [{"range": {"start": {"line": 0, "character": position}},
                       "severity": 2, "message": "offset " + str(position)}
                      for position in positions],
        }, ensure_ascii=False), encoding="utf-8")
        self.editor.ex("let g:column_fixture = json_decode(join(readfile('diagnostics.json'))) | "
                       "call setline(1, g:column_fixture.text) | "
                       "let g:column_test_diagnostics = g:column_fixture.items | "
                       "doautocmd User LspDiagsUpdated")
        return {int(item["text"].split()[1]): item["column"]
                for item in self.editor.snapshot()["diagnostics"]}

    def test_utf16_codepoints_include_combining_marks_and_surrogate_boundaries(self):
        # Byte lengths: a=1, 中=3, 😀=4, e=1, combining accent=2, z=1.
        self.assertEqual(self.publish("a中😀éz", range(9)),
                         {0: 1, 1: 2, 2: 5, 3: 5, 4: 9,
                          5: 10, 6: 12, 7: 13, 8: 13})

    def test_long_line_positions_are_exact_and_clamp_to_eol(self):
        prefix = "中😀é" * 1000
        length = len(prefix.encode("utf-8"))
        self.assertEqual(self.publish(prefix + "tail", (4999, 5000, 5004, 9000)),
                         {4999: length - 1, 5000: length + 1,
                          5004: length + 5, 9000: length + 5})

    def test_empty_line_and_buffer_edits_refresh_the_same_source(self):
        self.assertEqual(self.publish("", (0, 1, 5)), {0: 1, 1: 1, 5: 1})
        self.assertEqual(self.publish("😀", (0, 1, 2, 3)),
                         {0: 1, 1: 1, 2: 5, 3: 5})
        self.assertEqual(self.publish("中", (0, 1, 2)), {0: 1, 1: 4, 2: 4})


if __name__ == "__main__":
    unittest.main(verbosity=2)
