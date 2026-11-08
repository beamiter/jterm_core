#!/usr/bin/env python3
"""Test candidate checkout routing only; Cargo is deliberately mocked."""
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
RUNNER = Path(__file__).with_name('family-cargo.py').resolve()
spec = importlib.util.spec_from_file_location('family_cargo', RUNNER)
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class EngineRoutingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='family-routing-')
        self.addCleanup(self.temp.cleanup)
        self.family = Path(self.temp.name)
        self.script = self.family / 'jterm_core/scripts/family-cargo.py'
        self.script.parent.mkdir(parents=True)
        for project in runner.PROJECTS:
            self.crate(self.family / project, project)
        self.nested = self.family / 'jterm_core/crates/jterm_engine'
        self.crate(self.nested, 'jterm_engine')

    def crate(self, path, name):
        path.mkdir(parents=True, exist_ok=True)
        (path / 'Cargo.toml').write_text(f'[package]\nname = "{name}"\nversion = "0.0.0"\n')
        (path / 'Cargo.lock').write_text('# private lock fixture\n')
        (path / 'src').mkdir(exist_ok=True)
        (path / 'src/lib.rs').write_text('// source fixture\n')

    def test_candidate_mirrors_and_standalone_boundary(self):
        for nested in (True, False):
            if not nested:
                (self.nested / 'Cargo.toml').unlink()
            engine = self.nested if nested else self.family / 'jterm_engine'
            for project in ('jterm_engine', 'ember', 'frost', 'forge'):
                for subcommand in ('check', 'clippy', 'test'):
                    with self.subTest(nested=nested, project=project, subcommand=subcommand):
                        def run(command, cwd, env):
                            root = Path(cwd).parent
                            source = engine if project == 'jterm_engine' else self.family / project
                            self.assertEqual((Path(cwd) / 'src').resolve(), source / 'src')
                            self.assertFalse((Path(cwd) / 'Cargo.toml').is_symlink())
                            self.assertFalse((Path(cwd) / 'Cargo.lock').is_symlink())
                            if project in ('ember', 'frost'):
                                self.assertEqual((root / 'jterm_engine/src').resolve(), engine / 'src')
                                self.assertIn('patch."https://github.com/beamiter/jterm_core.git".jterm_engine.path='
                                              + json.dumps(str(root / 'jterm_engine')), command)
                            if project == 'jterm_engine':
                                self.assertFalse((root / 'jagent').exists())
                                self.assertFalse((root / 'jterm_core').exists())
                                self.assertNotIn('--config', command)
                            else:
                                self.assertGreater(command.index('--config'), 1)
                            self.assertIn('--offline', command)
                            self.assertEqual(command[:2], ['cargo', subcommand])
                            if subcommand == 'test':
                                self.assertIn(str(source), env['CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER'])
                            # A Cargo rewrite must not alter the real lockfile.
                            (Path(cwd) / 'Cargo.lock').write_text('rewritten in mirror\n')
                            return type('Result', (), {'returncode': 0})()
                        with patch.object(runner, '__file__', str(self.script)), \
                             patch.object(sys, 'argv', [str(RUNNER), project, subcommand]), \
                             patch.object(runner.subprocess, 'check_output', return_value='host: x86_64-unknown-linux-gnu\n'), \
                             patch.object(runner.subprocess, 'run', side_effect=run):
                            self.assertEqual(runner.main(), 0)
                        source = engine if project == 'jterm_engine' else self.family / project
                        self.assertEqual((source / 'Cargo.lock').read_text(), '# private lock fixture\n')


if __name__ == '__main__':
    unittest.main()
