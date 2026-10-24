#!/usr/bin/env python3
"""Deterministic benchmark guards, independent of Vim and GUI availability."""

import copy
import math
import unittest

from benchmark_metrics import distribution, scroll_frame_matches, validate_workload


def frame():
    view = {"row": 0, "column": 0, "rows": 28, "columns": 59}
    sibling_view = {"row": 0, "column": 60, "rows": 28, "columns": 60}
    return {
        "event": "render", "supported": True, "revision": 4,
        "window": 1001, "buffer": 7, "changedtick": 2,
        "rows": 30, "columns": 120, "view": view,
        "windows": [
            {"window": 1001, "buffer": 7, "changedtick": 2, "topline": 100,
             "leftcol": 0, "scroll_ack": 10, "view": view},
            {"window": 1002, "buffer": 7, "changedtick": 2, "topline": 1,
             "leftcol": 0, "scroll_ack": 0, "view": sibling_view},
        ],
    }


def scrolled(before):
    after = copy.deepcopy(before)
    after["revision"] += 1
    after["windows"][0]["topline"] += 1
    after["windows"][0]["scroll_ack"] = 11
    return after


class DistributionTests(unittest.TestCase):
    def test_nearest_rank_at_twenty_samples(self):
        result = distribution(range(1, 21))
        self.assertEqual(result["p95_ms"], 19)
        self.assertEqual(result["median_ms"], 10.5)
        self.assertEqual(result["max_ms"], 20)
        self.assertEqual(result["samples"], 20)

    def test_nearest_rank_at_one_hundred_samples(self):
        self.assertEqual(distribution(range(1, 101))["p95_ms"], 95)

    def test_single_sample_and_zero(self):
        self.assertEqual(distribution([0])["p95_ms"], 0)
        self.assertEqual(distribution([1.23456])["median_ms"], 1.235)

    def test_generator_and_input_order(self):
        values = [3.0, 1.0, 2.0]
        self.assertEqual(distribution(iter(values))["median_ms"], 2.0)
        self.assertEqual(values, [3.0, 1.0, 2.0])

    def test_invalid_measurements_are_errors(self):
        for values in ([], [math.nan], [math.inf], [-math.inf], [-0.1], [1.0, math.nan]):
            with self.subTest(values=values), self.assertRaises(ValueError):
                distribution(values)


class WorkloadTests(unittest.TestCase):
    def test_valid_workloads(self):
        for windows in (1, 2):
            validate_workload(1000, 20, windows)
            validate_workload(201, 1, windows)

    def test_reject_end_of_buffer_and_invalid_inputs(self):
        for values in ((200, 1, 1), (1000, 801, 1), (1000, 0, 1),
                       (1000, 1, 0), (1000, 1, 3), (1000, True, 1),
                       (1000.0, 1, 1), (1000, 1, True)):
            with self.subTest(values=values), self.assertRaises(ValueError):
                validate_workload(*values)


class ScrollSampleTests(unittest.TestCase):
    def setUp(self):
        self.before = frame()
        self.after = scrolled(self.before)

    def test_accept_real_full_frame_scroll(self):
        self.assertTrue(scroll_frame_matches(self.after, self.before))

    def test_accept_ordered_source_bound_delta(self):
        self.after["event"] = "render_delta"
        self.assertTrue(scroll_frame_matches(self.after, self.before, request_id=11))
        self.assertFalse(scroll_frame_matches(self.after, self.before))

    def test_new_revision_with_no_scroll_is_not_a_sample(self):
        self.after["windows"][0]["topline"] -= 1
        self.assertFalse(scroll_frame_matches(self.after, self.before))

    def test_old_or_missing_revision_is_not_a_sample(self):
        for revision in (3, 4, None, "5", True):
            with self.subTest(revision=revision):
                self.after["revision"] = revision
                self.assertFalse(scroll_frame_matches(self.after, self.before))

    def test_reject_unsupported_frame_and_non_render_events(self):
        self.after["supported"] = False
        self.assertFalse(scroll_frame_matches(self.after, self.before))
        self.after["supported"] = True
        for event in ("state", "response", "hello", None):
            self.after["event"] = event
            self.assertFalse(scroll_frame_matches(self.after, self.before))

    def test_reject_wrong_active_editor_identity(self):
        for key in ("window", "buffer", "changedtick"):
            with self.subTest(key=key):
                after = copy.deepcopy(self.after)
                after[key] += 1
                self.assertFalse(scroll_frame_matches(after, self.before))

    def test_reject_source_or_horizontal_view_changes(self):
        for key in ("buffer", "changedtick", "leftcol"):
            with self.subTest(key=key):
                after = copy.deepcopy(self.after)
                after["windows"][0][key] += 1
                self.assertFalse(scroll_frame_matches(after, self.before))

    def test_reject_resize_and_changed_split_geometry(self):
        for key in ("rows", "columns"):
            after = copy.deepcopy(self.after)
            after[key] += 1
            self.assertFalse(scroll_frame_matches(after, self.before))
        self.after["windows"][1]["view"]["column"] -= 1
        self.assertFalse(scroll_frame_matches(self.after, self.before))

    def test_reject_sibling_scroll_or_buffer_switch(self):
        for key in ("topline", "buffer", "changedtick", "leftcol"):
            with self.subTest(key=key):
                after = copy.deepcopy(self.after)
                after["windows"][1][key] += 1
                self.assertFalse(scroll_frame_matches(after, self.before))

    def test_ack_must_belong_to_target_not_sibling(self):
        self.after["windows"][0]["scroll_ack"] = 10
        self.after["windows"][1]["scroll_ack"] = 11
        self.assertFalse(scroll_frame_matches(self.after, self.before, request_id=11))

    def test_wrong_missing_and_invalid_ack_is_rejected(self):
        for ack in (0, 10, 12, None, "11", True):
            self.after["windows"][0]["scroll_ack"] = ack
            self.assertFalse(scroll_frame_matches(self.after, self.before, request_id=11))
        for request_id in (0, -1, True, "11"):
            self.assertFalse(scroll_frame_matches(scrolled(self.before), self.before, request_id=request_id))

    def test_jump_reverse_or_coalesced_multiple_lines_is_not_a_sample(self):
        for topline in (99, 100, 102):
            self.after["windows"][0]["topline"] = topline
            self.assertFalse(scroll_frame_matches(self.after, self.before))

    def test_missing_duplicate_or_replaced_window_is_rejected(self):
        for windows in ([], None, {}, [self.after["windows"][0]],
                        [self.after["windows"][0], self.after["windows"][0]]):
            after = copy.deepcopy(self.after)
            after["windows"] = windows
            self.assertFalse(scroll_frame_matches(after, self.before))
        self.after["windows"][1]["window"] = 1003
        self.assertFalse(scroll_frame_matches(self.after, self.before))

    def test_window_order_is_not_part_of_identity(self):
        self.after["windows"].reverse()
        self.assertTrue(scroll_frame_matches(self.after, self.before, request_id=11))

    def test_malformed_window_is_rejected_without_crashing(self):
        for value in (None, "window", {}, {"window": True}):
            after = copy.deepcopy(self.after)
            after["windows"][1] = value
            self.assertFalse(scroll_frame_matches(after, self.before))

    def test_cursor_movement_and_unicode_content_do_not_invalidate_scroll(self):
        self.after["windows"][0]["cursor"] = {"line": 105, "byte_column": 8}
        self.after["runs"] = [{"text": "中文é😀\t"}]
        self.assertTrue(scroll_frame_matches(self.after, self.before, request_id=11))

    def test_single_window_and_no_input_mutation(self):
        self.before["windows"] = self.before["windows"][:1]
        self.after = scrolled(self.before)
        original = copy.deepcopy((self.before, self.after))
        self.assertTrue(scroll_frame_matches(self.after, self.before))
        self.assertEqual((self.before, self.after), original)


if __name__ == "__main__":
    unittest.main()
