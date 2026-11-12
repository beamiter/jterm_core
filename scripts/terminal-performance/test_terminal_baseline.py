import importlib.util
import os
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
        result = baseline.measure(self.slave, os.getpid(), 20, 2)
        worker.join(2)
        self.assertFalse(worker.is_alive())
        self.assertEqual(len(result['samples']), 20)
        self.assertTrue(all(s['bytes'] == len(baseline.WORKLOAD) for s in result['samples']))


if __name__ == '__main__':
    unittest.main()
