#!/usr/bin/env python3
"""Real-Vim wire checks for reuse of already serialized native paint objects."""

import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import test_bridge
from test_bridge import Editor, VIM


@unittest.skipUnless(VIM, "Install Vim 9 for native paint wire tests")
class RenderSerializationTests(unittest.TestCase):
    def test_full_delta_fallback_and_batched_scroll_have_one_ordered_envelope(self):
        decode = json.loads
        text = 'quotes " \\ closing } 中文é😀 "protocol":9,"seq":99,"revision":88'
        for protocol in (1, 2, 7):
            with self.subTest(protocol=protocol), tempfile.TemporaryDirectory() as directory:
                received = []

                def unique_object(pairs):
                    result = {}
                    for key, value in pairs:
                        self.assertNotIn(key, result, f"Duplicate JSON field: {key}")
                        result[key] = value
                    return result

                def strict_decode(value):
                    message = decode(value, object_pairs_hook=unique_object)
                    received.append(message)
                    return message

                with patch.object(test_bridge.json, "loads", strict_decode):
                    editor = Editor(Path(directory), authenticate=False)
                    try:
                        editor.send({"action": "auth", "token": editor.token,
                                     "protocol": 1, "render_protocol": protocol})
                        editor.wait(lambda m: m.get("event") == "state")
                        editor.file.write_text((text + "\n") * 100, encoding="utf-8")
                        editor.ex("edit! | set nonumber norelativenumber nocursorline")
                        first = editor.gui_frame()
                        self.assertTrue(first["supported"], first.get("fallback_reason"))
                        self.assertEqual("".join(run["text"] for run in first["runs"]
                                                 if run["line"] == 1), text)
                        editor.keys("l")
                        moved = editor.wait(lambda m: m.get("event") in ("render", "render_delta")
                                            and m["revision"] > first["revision"])
                        self.assertEqual(moved["event"], "render" if protocol == 1 else "render_delta")
                        if protocol >= 2:
                            self.assertEqual(moved["base_revision"], first["revision"])
                            self.assertEqual(moved["row_updates"], [])
                        editor.ex("set spell")
                        fallback = editor.gui_frame()
                        self.assertFalse(fallback["supported"])
                        self.assertIn("compatibility", fallback["fallback_reason"].lower())
                        editor.ex("set nospell")
                        restored = editor.gui_frame()
                        self.assertTrue(restored["supported"])
                        self.assertGreater(restored["revision"], fallback["revision"])
                        if protocol >= 3:
                            window = restored["windows"][0]
                            response = editor.request("scroll_view", lines=1, **{
                                key: window[key] for key in
                                ("window", "buffer", "changedtick", "topline", "leftcol")})
                            acknowledged = editor.wait(lambda m:
                                m.get("event") in ("render", "render_delta")
                                and any(w.get("scroll_ack") == response["id"]
                                        for w in m.get("windows", [])))
                            self.assertLess(acknowledged["seq"], response["seq"])
                        editor.request("save")
                        self.assertEqual(editor.file.read_text(encoding="utf-8"), (text + "\n") * 100)
                    finally:
                        editor.close()
                sequences = [message["seq"] for message in received]
                self.assertEqual(sequences, sorted(set(sequences)))
                self.assertTrue(all(message["protocol"] == 1 for message in received))


if __name__ == "__main__":
    unittest.main(verbosity=2)
