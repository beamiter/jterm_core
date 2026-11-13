import importlib.util
import os
import json
import subprocess
import sys
import tempfile
from unittest import mock
from pathlib import Path
import threading
import time
import unittest
import termios
import tty

spec = importlib.util.spec_from_file_location('baseline', Path(__file__).with_name('terminal-baseline.py'))
baseline = importlib.util.module_from_spec(spec)
spec.loader.exec_module(baseline)


class ProbeTests(unittest.TestCase):
    def setUp(self):
        self.master, self.slave = os.openpty()
        tty.setraw(self.slave)
        os.set_blocking(self.slave, False)

    def tearDown(self):
        os.close(self.slave)
        os.close(self.master)

    def test_p95_nearest_rank(self):
        self.assertEqual(baseline.percentile95(list(range(1, 101))), 95)
        self.assertEqual(baseline.percentile95(list(range(1, 21))), 19)

    def test_large_write_and_fragmented_reply(self):
        data = bytearray()
        def consumer():
            while not data.endswith(b'\x1b[6n'):
                data.extend(os.read(self.master, 4096))
            for fragment in (b'\x1b', b'[12;', b'34R'):
                os.write(self.master, fragment)
                time.sleep(.002)
        worker = threading.Thread(target=consumer)
        worker.start()
        baseline.exchange(self.slave, baseline.WORKLOAD, 2)
        worker.join(2)
        self.assertFalse(worker.is_alive())
        self.assertEqual(bytes(data), baseline.WORKLOAD + b'\x1b[6n')

    def test_timeout_includes_backpressure(self):
        start = time.monotonic()
        with self.assertRaises(TimeoutError):
            baseline.exchange(self.slave, b'x' * 100000, .05)
        self.assertLess(time.monotonic() - start, 1)

    def test_refuse_user_keystrokes(self):
        os.write(self.master, b'q')
        with self.assertRaises(RuntimeError):
            baseline.exchange(self.slave, b'', .1)

    def test_refuse_early_cpr(self):
        os.write(self.master, b'\x1b[1;1R')
        with self.assertRaisesRegex(RuntimeError, 'before query'):
            baseline.exchange(self.slave, baseline.WORKLOAD, .1)

    def test_measure_does_not_flush_pending_input(self):
        os.write(self.master, b'q')
        with self.assertRaisesRegex(RuntimeError, 'pending input'):
            baseline.measure(self.slave, os.getpid(), 20, .1)
        self.assertEqual(os.read(self.slave, 1), b'q')

    def test_restore_terminal_after_failure(self):
        old = termios.tcgetattr(self.slave)
        with self.assertRaises(TimeoutError):
            baseline.measure(self.slave, os.getpid(), 20, .02)
        self.assertEqual(termios.tcgetattr(self.slave), old)
        self.assertFalse(os.get_blocking(self.slave))

    def test_restore_terminal_after_tree_census_failure(self):
        old = termios.tcgetattr(self.slave)
        with mock.patch.object(baseline, 'exchange'), mock.patch.object(
                baseline, 'process_tree_rss', side_effect=RuntimeError('census changed')):
            with self.assertRaisesRegex(RuntimeError, 'census changed'):
                baseline.measure(self.slave, os.getpid(), 20, .1, 'process-tree')
        self.assertEqual(termios.tcgetattr(self.slave), old)
        self.assertFalse(os.get_blocking(self.slave))

    def test_rss_and_pid_identity(self):
        identity = baseline.process_identity(os.getpid())
        self.assertGreater(baseline.rss_kib(os.getpid(), identity), 0)
        with self.assertRaises(RuntimeError):
            baseline.rss_kib(os.getpid(), dict(start_ticks='invalid'))

    def test_measure_plumbing_not_a_terminal_benchmark(self):
        # This fake DSR responder validates transport only. Never publish its timing.
        def responder():
            buf = bytearray()
            for _ in range(21):
                while b'\x1b[6n' not in buf:
                    buf.extend(os.read(self.master, 4096))
                _, rest = buf.split(b'\x1b[6n', 1)
                buf[:] = rest
                os.write(self.master, b'\x1b[1;1R')
        worker = threading.Thread(target=responder, daemon=True)
        worker.start()
        snapshot = dict(total_rss_kib=123, processes=[dict(pid=1, parent_pid=0, start_ticks='1', rss_kib=123)])
        with mock.patch.object(baseline, 'process_tree_rss', return_value=snapshot) as census:
            result = baseline.measure(self.slave, os.getpid(), 20, 2, 'process-tree')
        self.assertEqual(census.call_count, 40)
        self.assertEqual(result['rss_scope'], 'process-tree')
        self.assertEqual(result['rss_sampled_max_kib'], 123)
        self.assertTrue(all(s['tree_before'] == snapshot and s['tree_after'] == snapshot
                            for s in result['samples']))
        worker.join(2)
        self.assertFalse(worker.is_alive())
        self.assertEqual(len(result['samples']), 20)
        self.assertTrue(all(s['bytes'] == len(baseline.WORKLOAD) for s in result['samples']))


class TreeTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.proc = Path(self.tmp.name)
        self.write(10, 1, '100', 20)
        self.write(11, 10, '101', 30)
        self.write(12, 11, '102', 40)
        self.write(20, 1, '103', 500)

    def write(self, pid, parent, start, rss):
        directory = self.proc / str(pid)
        directory.mkdir(exist_ok=True)
        fields = ['S', str(parent)] + ['0'] * 17 + [start]
        (directory / 'stat').write_text(f'{pid} (name ) with spaces) ' + ' '.join(fields))
        (directory / 'status').write_text(f'VmRSS:\t{rss} kB\n')

    def sample(self, exclude=99):
        return baseline.process_tree_rss(10, {'start_ticks': '100'}, self.proc, exclude)

    def test_descendants_not_unrelated_and_raw_identity_retained(self):
        result = self.sample()
        self.assertEqual(result['total_rss_kib'], 90)
        self.assertEqual([p['pid'] for p in result['processes']], [10, 11, 12])
        self.assertEqual(result['processes'][2]['start_ticks'], '102')

    def test_exclude_measurement_subtree_only(self):
        self.assertEqual(self.sample(exclude=11)['total_rss_kib'], 20)
        with self.assertRaisesRegex(RuntimeError, 'measurement process'):
            self.sample(exclude=10)

    def test_reused_root_is_not_new_host(self):
        self.write(10, 1, '999', 20)
        with self.assertRaisesRegex(RuntimeError, 'reused'):
            self.sample()

    def test_child_reuse_during_status_read_is_rejected(self):
        original = Path.read_text
        def changed(path, *args, **kwargs):
            value = original(path, *args, **kwargs)
            if path == self.proc / '11' / 'status':
                self.write(11, 10, '999', 31)
            return value
        with mock.patch.object(Path, 'read_text', changed):
            with self.assertRaisesRegex(RuntimeError, 'changed during RSS'):
                self.sample()

    def test_child_appearing_during_census_is_rejected(self):
        original = Path.read_text
        def changed(path, *args, **kwargs):
            value = original(path, *args, **kwargs)
            if path == self.proc / '11' / 'status':
                self.write(13, 10, '200', 50)
            return value
        with mock.patch.object(Path, 'read_text', changed):
            with self.assertRaisesRegex(RuntimeError, 'membership changed'):
                self.sample()

    def test_child_reparent_during_census_is_rejected(self):
        original = Path.read_text
        def changed(path, *args, **kwargs):
            value = original(path, *args, **kwargs)
            if path == self.proc / '11' / 'status':
                self.write(12, 20, '102', 40)
            return value
        with mock.patch.object(Path, 'read_text', changed):
            with self.assertRaisesRegex(RuntimeError, 'changed before RSS'):
                self.sample()

    def test_missing_rss_is_failure_not_zero(self):
        (self.proc / '11' / 'status').write_text('Name: zombie\n')
        with self.assertRaisesRegex(RuntimeError, 'RSS unavailable'):
            self.sample()

    def test_child_disappearing_is_failure_not_partial_sum(self):
        original = Path.read_text
        def changed(path, *args, **kwargs):
            value = original(path, *args, **kwargs)
            if path == self.proc / '11' / 'status':
                (self.proc / '12' / 'stat').unlink()
            return value
        with mock.patch.object(Path, 'read_text', changed):
            with self.assertRaises(FileNotFoundError):
                self.sample()

    def test_real_parent_and_child_processes(self):
        code = '''import subprocess, sys, json, os
child = subprocess.Popen([sys.executable, '-c', 'import sys; sys.stdin.read()'], stdin=subprocess.PIPE)
print(json.dumps([os.getpid(), child.pid]), flush=True)
try:
    sys.stdin.read()
finally:
    child.stdin.close()
    child.wait(timeout=5)
'''
        parent = subprocess.Popen([sys.executable, '-c', code], stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, text=True)
        try:
            pids = json.loads(parent.stdout.readline())
            result = baseline.process_tree_rss(parent.pid, baseline.process_identity(parent.pid))
            self.assertEqual([p['pid'] for p in result['processes']], sorted(pids))
            self.assertTrue(all(p['rss_kib'] > 0 for p in result['processes']))
            self.assertEqual(result['total_rss_kib'], sum(p['rss_kib'] for p in result['processes']))
        finally:
            parent.stdin.close()
            parent.wait(timeout=10)
            parent.stdout.close()


if __name__ == '__main__':
    unittest.main()
