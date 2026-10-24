"""Pure validation for Vim bridge benchmarks; no Vim, GUI or network required.

These checks validate semantic frame samples, not presentation latency or FPS.
Keep the workload identity stable before accepting a faster timing result.
"""

from __future__ import annotations

import math
import statistics
from collections.abc import Iterable, Mapping
from typing import Any


def distribution(values: Iterable[float]) -> dict[str, int | float]:
    """Return milliseconds with a nearest-rank P95 (ceil(0.95 * N), 1-based)."""
    ordered = sorted(float(value) for value in values)
    if not ordered or any(not math.isfinite(value) or value < 0 for value in ordered):
        raise ValueError("Latency samples must be nonempty, finite and nonnegative")
    # Integer arithmetic avoids rounding the percentile index at multiples of 20.
    p95_index = (95 * len(ordered) + 99) // 100 - 1
    return {
        "samples": len(ordered),
        "min_ms": round(ordered[0], 3),
        "median_ms": round(statistics.median(ordered), 3),
        "p95_ms": round(ordered[p95_index], 3),
        "max_ms": round(ordered[-1], 3),
    }


def validate_workload(lines: int, samples: int, windows: int) -> None:
    """Reserve enough source rows so a scroll sample cannot silently hit EOF."""
    if (type(lines) is not int or type(samples) is not int or type(windows) is not int
            or samples < 1 or lines < samples + 200 or windows not in (1, 2)):
        raise ValueError("Use positive samples, at least samples + 200 lines, and one or two windows")


def _window_map(frame: Mapping[str, Any]) -> dict[int, Mapping[str, Any]] | None:
    windows = frame.get("windows")
    if not isinstance(windows, list) or not 1 <= len(windows) <= 16:
        return None
    result = {}
    for window in windows:
        if not isinstance(window, dict):
            return None
        for key in ("window", "buffer", "changedtick", "topline", "leftcol"):
            value = window.get(key)
            if type(value) is not int or value < (1 if key in ("window", "buffer", "topline") else 0):
                return None
        identity = window["window"]
        if identity in result:
            return None
        result[identity] = window
    return result


def scroll_frame_matches(
    message: Mapping[str, Any],
    previous: Mapping[str, Any],
    *,
    request_id: int | None = None,
) -> bool:
    """Accept only the next one-line scroll of the same source-bound viewport.

    Without an ID the legacy benchmark expects a full render. With an ID,
    render_delta is also allowed, but the ACK must belong to the active window,
    not an arbitrary sibling. New revisions alone are not scroll evidence.
    A changed buffer, text generation, layout or stationary sibling invalidates
    the sample rather than making a fast but unrelated frame look successful.
    """
    events = ("render",) if request_id is None else ("render", "render_delta")
    if message.get("event") not in events or not message.get("supported"):
        return False
    revision, before_revision = message.get("revision"), previous.get("revision")
    if (type(revision) is not int or type(before_revision) is not int
            or revision <= before_revision or not previous.get("supported")):
        return False
    active = previous.get("window")
    if type(active) is not int or active <= 0:
        return False
    for key in ("window", "buffer", "changedtick", "rows", "columns", "view"):
        if key not in previous or message.get(key) != previous[key]:
            return False
    before, after = _window_map(previous), _window_map(message)
    if before is None or after is None or before.keys() != after.keys() or active not in before:
        return False
    for identity, old in before.items():
        new = after[identity]
        for key in ("buffer", "changedtick", "leftcol", "view"):
            if key not in old or new.get(key) != old[key]:
                return False
        expected_topline = old["topline"] + (1 if identity == active else 0)
        if new["topline"] != expected_topline:
            return False
    if request_id is not None:
        if type(request_id) is not int or request_id < 1:
            return False
        ack = after[active].get("scroll_ack")
        if type(ack) is not int or ack != request_id:
            return False
    return True
