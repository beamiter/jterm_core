"""Bounded metadata only; never issue Vim input to diagnose a failed capture."""
import stat


class CaptureInputEvidence:
    """Fixed PTY write statuses, not receipts that Vim consumed the keys."""

    def __init__(self):
        self._yank = self._command = self._enter = "unobserved"

    @staticmethod
    def _acceptance(requested, accepted):
        if (type(requested) is not int or type(accepted) is not int
                or requested <= 0 or not 0 <= accepted <= requested):
            return "invalid"
        if accepted == requested:
            return "complete"
        return "partial" if accepted else "none"

    def observe_yank(self, requested, accepted):
        self._yank = self._acceptance(requested, accepted)

    def observe_command(self, requested, accepted):
        self._command = self._acceptance(requested, accepted)

    def observe_enter(self, requested, accepted):
        self._enter = self._acceptance(requested, accepted)

    def metadata(self):
        return {"yank": self._yank, "command": self._command, "enter": self._enter}


def capture_failure_metadata(editor, output):
    state = editor.state if isinstance(editor.state, dict) else {}
    # Do not serialize arbitrary state, terminal bytes, exception text or paths.
    modes = {"n", "no", "i", "ic", "ix", "c", "cv", "ce", "v", "V",
             "R", "Rc", "Rx", "Rv", "s", "S", "t", "!"}
    mode = state.get("mode")
    result = {"mode": mode if isinstance(mode, str) and mode in modes else "other"}
    for field in ("window", "buffer", "changedtick"):
        value = state.get(field)
        result[field] = value if type(value) is int and 0 <= value < 2**63 else None
    result["bridge_connected"] = editor.connection is not None
    try:
        code = editor.process.poll()
        result["process_status"] = "running" if code is None else "exited"
        result["exit_code"] = code if type(code) is int and -128 <= code <= 255 else None
    except OSError:
        result["process_status"] = "unavailable"
    try:
        info = output.lstat()
        result["result_kind"] = "regular" if stat.S_ISREG(info.st_mode) else "other"
        if stat.S_ISREG(info.st_mode):
            result["result_bytes_capped"] = min(max(info.st_size, 0), 1024 * 1024)
    except FileNotFoundError:
        result["result_kind"] = "missing"
    except OSError:
        result["result_kind"] = "unavailable"
    return result
