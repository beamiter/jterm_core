#!/usr/bin/env python3
"""Native Xvfb investigation: real keyboard input, real shells, isolated state."""
import argparse
import hashlib
import json
import shutil
import os
from pathlib import Path
import select
import shlex
import signal
import subprocess
import termios
import time


def wait(probe, label, timeout=20):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        value = probe()
        if value:
            return value
        time.sleep(.1)
    raise RuntimeError("timeout: " + label)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("app", choices=["anvil", "forge", "ember", "frost"])
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--jsh-binary", type=Path, default=shutil.which("jsh"))
    parser.add_argument("--explicit-ide", action="store_true")
    parser.add_argument("--shell", choices=["bash", "jsh"], default="bash")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False, mode=0o700)
    env = os.environ.copy()
    for key in ("DISPLAY", "WAYLAND_DISPLAY", "SWAYSOCK"):
        env.pop(key, None)
    test_home = out / "home"
    test_home.mkdir(mode=0o700)
    for key, suffix in [("HOME", ""), ("XDG_CONFIG_HOME", "config"),
                        ("XDG_STATE_HOME", "state"), ("XDG_DATA_HOME", "data"),
                        ("XDG_CACHE_HOME", "cache"), ("XDG_RUNTIME_DIR", "runtime")]:
        path = test_home / suffix
        path.mkdir(mode=0o700, exist_ok=True)
        env[key] = str(path)
    env.update(LIBGL_ALWAYS_SOFTWARE="1", WGPU_BACKEND="gl", GDK_BACKEND="x11",
               NO_AT_BRIDGE="1", GTK_A11Y="none", HISTFILE="/dev/null",
               VIMINIT="set nocompatible viminfofile=NONE noswapfile nomore")
    shell = out / "probe-shell"
    if args.shell == "bash":
        rc = out / "bashrc"
        rc.write_text("""HISTFILE=/dev/null
__probe_prompt() {
    local status=$?
    if [[ ${__probe_seen:-0} == 1 ]]; then printf '\\e]133;D;%s\\a' "$status"; fi
    __probe_seen=1
    printf '\\e]7;file://localhost%s\\a' "$PWD"
}
PROMPT_COMMAND=__probe_prompt
PS0=$'\\e]133;C\\a'
PS1='\\[\\e]133;A\\a\\]CD_PROBE> \\[\\e]133;B\\a\\]'
""")
        shell.write_text("#!/bin/sh\nexec /bin/bash --noprofile --rcfile " + shlex.quote(str(rc)) + " -i\n")
    else:
        if not args.jsh_binary:
            raise ValueError("--shell jsh requires jsh on PATH or --jsh-binary")
        shell.write_text("#!/bin/sh\nexec " + shlex.quote(str(args.jsh_binary.resolve(strict=True))) + "\n")
    shell.chmod(0o755)
    config = Path(env["XDG_CONFIG_HOME"]) / args.app
    config.mkdir(mode=0o700)
    config_text = "\n".join([f"shell = {json.dumps(str(shell))}",
                             'jsh_update_check = "never"',
                             'terminal_mode = "block"', ""])
    if args.app in ("ember", "frost"):
        config_text += 'restore_session = false\napp_renderer = "wgpu"\n'
    (config / "config.toml").write_text(config_text)
    (config / "config.toml").chmod(0o600)
    paths = [out / name for name in ["plain", "vim", "nvim codex", "空 格 雪"]]
    for path in paths:
        path.mkdir()
    completion_dir = out / "vim-completion-path"
    completion_dir.mkdir()
    test_file = out / "editor.txt"
    test_file.write_text("CD regression fixture\n")
    sentinel = out / "commands.tsv"
    processes, logs = [], []
    result = {"app": args.app, "shell": args.shell, "status": "fail", "cases": []}

    def spawn(argv, label, **kwargs):
        log = (out / (label + ".log")).open("w")
        logs.append(log)
        process = subprocess.Popen(argv, env=env, stdout=log, stderr=log,
                                   start_new_session=True, **kwargs)
        processes.append(process)
        return process

    def run(argv):
        return subprocess.run(argv, env=env, check=True, capture_output=True,
                              text=True, timeout=12).stdout

    def key(value):
        run(["xdotool", "key", "--clearmodifiers", value])

    def text(value):
        run(["xdotool", "type", "--clearmodifiers", "--delay", "8", "--", value])

    def record(label, command, expected=None):
        full = command + " && printf '%s\\t%s\\t%s\\n' " + shlex.quote(label) + ' "$$" "$PWD" >> ' + shlex.quote(str(sentinel))
        text(full)
        key("Return")
        wait(lambda: sentinel.exists() and any(line.startswith(label + "\t") for line in sentinel.read_text().splitlines()), label)
        rows = [line.split("\t", 2) for line in sentinel.read_text().splitlines() if line.startswith(label + "\t")]
        assert len(rows) == 1, rows
        result["cases"].append({"label": label, "pid": rows[0][1], "cwd": rows[0][2]})
        if expected is not None:
            assert rows[0][2] == str(expected), rows[0]

    def ready_shells(pid):
        found = []
        try:
            children = Path(f"/proc/{pid}/task/{pid}/children").read_text().split()
        except OSError:
            return found
        for child in children:
            try:
                executable = Path(f"/proc/{child}/exe").resolve().name
                if executable in ("bash", "jsh"):
                    fd = os.open(f"/proc/{child}/fd/0", os.O_RDONLY | os.O_NOCTTY | os.O_NONBLOCK)
                    try:
                        flags = termios.tcgetattr(fd)[3]
                        if not flags & (termios.ICANON | termios.ECHO):
                            found.append(child)
                    finally:
                        os.close(fd)
            except (OSError, termios.error):
                pass
            found.extend(ready_shells(child))
        return found

    try:
        read_fd, write_fd = os.pipe()
        server = spawn(["Xvfb", "-screen", "0", "1400x950x24", "-nolisten", "tcp", "-displayfd", str(write_fd)], "xvfb", pass_fds=(write_fd,))
        os.close(write_fd)
        assert select.select([read_fd], [], [], 10)[0]
        env["DISPLAY"] = ":" + os.read(read_fd, 40).decode().strip()
        os.close(read_fd)
        assert server.poll() is None
        binary = args.binary.resolve(strict=True)
        result["binary"] = str(binary)
        result["binary_sha256"] = hashlib.sha256(binary.read_bytes()).hexdigest()
        argv = [str(binary)]
        if args.app in ("anvil", "forge"):
            argv += ["--no-restore"]
        if args.explicit_ide:
            argv = [str(binary), "--ide", str(test_file)]
        app = spawn(argv, args.app, cwd=paths[0])
        def find_window():
            probe = subprocess.run(["xdotool", "search", "--onlyvisible", "--pid", str(app.pid)], env=env, capture_output=True, text=True)
            return probe.stdout.splitlines()[0] if probe.returncode == 0 and probe.stdout.strip() else None
        window = wait(find_window, "mapped application")
        run(["xdotool", "windowfocus", "--sync", window])
        if args.explicit_ide:
            time.sleep(4)
            assert app.poll() is None
            run(["import", "-window", window, str(out / "explicit-ide.png")])
            result["window_tree"] = run(["xwininfo", "-root", "-tree"])
            result["status"] = "captured"
            return 0
        wait(lambda: ready_shells(app.pid), "shell line editor raw mode")
        time.sleep(.5)
        run(["import", "-window", window, str(out / "startup.png")])
        record("initial", "true")
        for cycle in range(20):
            text("cd " + shlex.quote(str(out)) + "/vim-comp")
            if cycle % 2 == 0:
                key("Tab")
                time.sleep(.03)
                key("Return")
            else:
                run(["xdotool", "key", "--clearmodifiers", "--delay", "0", "Tab", "Return"])
            first_pid = result["cases"][0]["pid"]
            wait(lambda: Path(f"/proc/{first_pid}/cwd").resolve() == completion_dir,
                 f"Tab completion cd-{cycle}")
            record(f"completion-{cycle}", "true", completion_dir)
            record(f"completion-return-{cycle}", "cd " + shlex.quote(str(paths[0])), paths[0])
        for cycle in range(5):
            for index, path in enumerate(paths):
                # ASCII-only keyboard traffic resolves the Unicode path in shell.
                command = "cd " + shlex.quote(str(path))
                if index == 3:
                    command = "cd " + shlex.quote(str(out)) + r'''/"$(/usr/bin/printf '\347\251\272 \346\240\274 \351\233\252')"'''
                record(f"cd-{cycle}-{index}", command, path)
            record(f"cd-minus-{cycle}", "cd -", paths[2])
        record("rapid-cd", "for probe_index in 1 2 3 4 5 6 7 8 9 10; do cd " + shlex.quote(str(paths[1])) + "; cd " + shlex.quote(str(paths[2])) + "; done", paths[2])
        record("vim-title", "printf '\\033]0;vim nvim codex\\007'")
        record("after-vim-title", "cd " + shlex.quote(str(paths[0])))
        run(["import", "-window", window, str(out / "after-cd.png")])
        text("/usr/bin/vim -Nu NONE -i NONE -n " + shlex.quote(str(test_file)))
        key("Return")
        time.sleep(1)
        run(["import", "-window", window, str(out / "real-vim.png")])
        key("Escape")
        text(":q!")
        key("Return")
        time.sleep(.5)
        record("after-editor-exit", "cd " + shlex.quote(str(paths[1])))
        key("ctrl+shift+t")
        wait(lambda: len(ready_shells(app.pid)) >= 2, "second shell line editor raw mode")
        time.sleep(.5)
        record("second-tab", "cd " + shlex.quote(str(paths[2])))
        key("ctrl+Page_Up")
        time.sleep(.3)
        record("first-tab-return", "cd " + shlex.quote(str(paths[0])))
        run(["import", "-window", window, str(out / "final.png")])
        first_pid = result["cases"][0]["pid"]
        assert all(case["pid"] == first_pid for case in result["cases"] if case["label"] != "second-tab")
        assert next(case["pid"] for case in result["cases"] if case["label"] == "second-tab") != first_pid
        result["window_tree"] = run(["xwininfo", "-root", "-tree"])
        result["status"] = "pass"
    except Exception as error:
        result["error"] = repr(error)
        if env.get("DISPLAY"):
            try:
                run(["import", "-window", "root", str(out / "failure.png")])
            except Exception:
                pass
    finally:
        for process in reversed(processes):
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(3)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(3)
        for log in logs:
            log.close()
        (out / "result.json").write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
        print(json.dumps({k: v for k, v in result.items() if k != "window_tree"}, ensure_ascii=False))
    return 0 if result["status"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main())
