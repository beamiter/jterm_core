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
        for crate in ('src-tauri', 'bridge', 'session'):
            self.crate(self.family / 'aether' / crate, crate)
        (self.family / 'aether/web').mkdir()
        self.nested = self.family / 'jterm_core/crates/jterm_engine'
        self.crate(self.nested, 'jterm_engine')
        self.crate(self.nested.with_name('jterm_runtime'), 'jterm_runtime')
        self.crate(self.nested.with_name('jterm_protocol'), 'jterm_protocol')
        self.crate(self.nested.with_name('jterm_gtk_view'), 'jterm_gtk_view')
        self.crate(self.nested.with_name('jterm_remote_client'), 'jterm_remote_client')

    def crate(self, path, name):
        path.mkdir(parents=True, exist_ok=True)
        (path / 'Cargo.toml').write_text(f'[package]\nname = "{name}"\nversion = "0.0.0"\n')
        (path / 'Cargo.lock').write_text('# private lock fixture\n')
        (path / 'src').mkdir(exist_ok=True)
        (path / 'src/lib.rs').write_text('// source fixture\n')

    def test_gtk_shared_terminal_is_private_and_opted_in(self):
        for app in ('forge', 'anvil'):
            with self.subTest(app=app):
                source = self.family / app / 'Cargo.toml'
                baseline = source.read_text() + '\n[dependencies]\n'
                source.write_text(baseline)

                def run(command, cwd, env):
                    self.assertEqual(env[f"JTERM_{app.upper()}_SHARED_TERMINAL"], "1")
                    self.assertIn('jterm_gtk_view = { path = "../jterm_core/crates/jterm_gtk_view" }',
                                  (Path(cwd) / 'Cargo.toml').read_text())
                    self.assertEqual((Path(cwd) / '../jterm_core/crates/jterm_gtk_view/src').resolve(),
                                     self.nested.with_name('jterm_gtk_view') / 'src')
                    core_crates = Path(cwd).parent / 'jterm_core' / 'crates'
                    self.assertEqual((core_crates / 'jterm_engine/src').resolve(), self.nested / 'src')
                    self.assertFalse((core_crates / 'jterm_engine/Cargo.toml').is_symlink())
                    self.assertFalse((Path(cwd).parent / 'jterm_engine').exists())
                    self.assertFalse((Path(cwd).parent / 'jterm_gtk_view').exists())
                    return type('Result', (), {'returncode': 0})()

                with patch.object(runner, '__file__', str(self.script)), \
                     patch.object(sys, 'argv', [str(RUNNER), f'--{app}-shared-terminal', app, 'check']), \
                     patch.object(runner.subprocess, 'run', side_effect=run):
                    self.assertEqual(runner.main(), 0)
                self.assertEqual(source.read_text(), baseline)

    def test_published_gtk_view_defaults_to_the_local_core_candidate(self):
        for app in ('forge', 'anvil'):
            with self.subTest(app=app):
                manifest = self.family / app / 'Cargo.toml'
                baseline = manifest.read_text() + (
                    '\n[dependencies]\n'
                    'jterm_gtk_view = { git = "https://github.com/beamiter/jterm_core.git" }\n'
                )
                manifest.write_text(baseline)

                def run(command, cwd, env):
                    core = Path(cwd).parent / 'jterm_core'
                    prefix = 'patch."https://github.com/beamiter/jterm_core.git".'
                    self.assertIn(prefix + 'jterm_core.path=' + json.dumps(str(core)), command)
                    view = core / 'crates/jterm_gtk_view'
                    self.assertIn(prefix + 'jterm_gtk_view.path=' + json.dumps(str(view)), command)
                    self.assertEqual((view / 'src').resolve(), self.nested.with_name('jterm_gtk_view') / 'src')
                    self.assertNotIn('--features', command)
                    self.assertNotIn(f'JTERM_{app.upper()}_SHARED_TERMINAL', env)
                    self.assertEqual((Path(cwd) / 'Cargo.toml').read_text(), baseline)
                    return type('Result', (), {'returncode': 0})()

                with patch.object(runner, '__file__', str(self.script)), \
                     patch.object(sys, 'argv', [str(RUNNER), app, 'check']), \
                     patch.object(runner.subprocess, 'run', side_effect=run):
                    self.assertEqual(runner.main(), 0)

    def test_aether_local_core_crates_resolve_inside_private_siblings(self):
        source = self.family / 'aether'
        for crate in ('session', 'bridge'):
            manifest = source / crate / 'Cargo.toml'
            manifest.write_text(manifest.read_text() + '\n[dependencies]\n'
                                f'jterm_engine = {{ path = "{runner.AETHER_LOCAL_ENGINE}" }}\n')
        bridge = source / 'bridge' / 'Cargo.toml'
        bridge.write_text(bridge.read_text()
                          + 'jterm_protocol = { path = "../../jterm_core/crates/jterm_protocol" }\n')
        desktop = source / 'src-tauri' / 'Cargo.toml'
        desktop.write_text(desktop.read_text() + '\n[dependencies]\n'
                           'jterm_remote_client = { path = "../../jterm_core/crates/jterm_remote_client" }\n')

        def run(command, cwd, env):
            core_crates = Path(cwd) / '../../jterm_core/crates'
            for crate in ('jterm_engine', 'jterm_runtime', 'jterm_protocol', 'jterm_remote_client'):
                path = core_crates / crate
                self.assertFalse((path / 'Cargo.toml').is_symlink())
                self.assertEqual((path / 'src').resolve(), self.nested.parent / crate / 'src')
            self.assertNotIn('--config', command)
            return type('Result', (), {'returncode': 0})()

        with patch.object(runner, '__file__', str(self.script)), \
             patch.object(sys, 'argv', [str(RUNNER), 'aether', 'check']), \
             patch.object(runner.subprocess, 'run', side_effect=run):
            self.assertEqual(runner.main(), 0)

    def test_aether_git_runtime_uses_the_same_private_engine(self):
        manifest = self.family / 'aether/session/Cargo.toml'
        manifest.write_text(
            manifest.read_text() + "\n[target.'cfg(target_os = \"linux\")'.dependencies]\n"
            'jterm_runtime = { git = "https://github.com/beamiter/jterm_core" }\n'
        )

        def run(command, cwd, env):
            root = Path(cwd).parent.parent
            for crate in ('jterm_engine', 'jterm_runtime'):
                override = f'patch."https://github.com/beamiter/jterm_core.git".{crate}.path='
                self.assertIn(override + json.dumps(str(root / crate)), command)
                self.assertEqual((root / crate / 'src').resolve(), self.nested.parent / crate / 'src')
            return type('Result', (), {'returncode': 0})()

        with patch.object(runner, '__file__', str(self.script)), \
             patch.object(sys, 'argv', [str(RUNNER), 'aether-session', 'check']), \
             patch.object(runner.subprocess, 'run', side_effect=run):
            self.assertEqual(runner.main(), 0)

    def test_aether_candidate_uses_the_selected_worktree(self):
        worktree = self.family / 'aether-candidate'
        for crate in ('src-tauri', 'bridge', 'session'):
            self.crate(worktree / crate, crate)

        def run(command, cwd, env):
            self.assertEqual((Path(cwd) / 'src').resolve(), worktree / 'src-tauri/src')
            self.assertFalse((Path(cwd) / 'Cargo.lock').is_symlink())
            return type('Result', (), {'returncode': 0})()

        with patch.object(runner, '__file__', str(self.script)), \
             patch.object(sys, 'argv', [str(RUNNER), '--aether-checkout', str(worktree), 'aether', 'check']), \
             patch.object(runner.subprocess, 'run', side_effect=run):
            self.assertEqual(runner.main(), 0)

    def test_aether_desktop_workspace_dependency_mirrors_one_core_tree(self):
        source = self.family / 'aether'
        for name in ('session', 'bridge'):
            manifest = source / name / 'Cargo.toml'
            manifest.write_text(
                manifest.read_text() + '\n[dependencies]\n'
                f'jterm_engine = {{ path = "{runner.AETHER_LOCAL_ENGINE}" }}\n'
            )
        bridge = source / 'bridge' / 'Cargo.toml'
        bridge.write_text(bridge.read_text()
                          + 'jterm_protocol = { path = "../../jterm_core/crates/jterm_protocol" }\n')
        desktop = source / 'src-tauri' / 'Cargo.toml'
        desktop.write_text(desktop.read_text() + '\n[dependencies]\n'
                           'jterm_core = { path = "../../jterm_core" }\n')

        def run(command, cwd, env):
            root = Path(cwd).parent.parent
            core = root / 'jterm_core'
            self.assertEqual((core / 'src').resolve(), self.family / 'jterm_core/src')
            for crate in ('jterm_engine', 'jterm_runtime', 'jterm_protocol'):
                path = core / 'crates' / crate
                self.assertFalse((path / 'Cargo.toml').is_symlink())
                self.assertEqual((path / 'src').resolve(), self.nested.parent / crate / 'src')
                self.assertFalse((root / crate).exists())
            self.assertIn('patch."https://github.com/beamiter/jterm_core.git".jterm_core.path='
                          + json.dumps(str(core)), command)
            return type('Result', (), {'returncode': 0})()

        with patch.object(runner, '__file__', str(self.script)), \
             patch.object(sys, 'argv', [str(RUNNER), 'aether', 'check']), \
             patch.object(runner.subprocess, 'run', side_effect=run):
            self.assertEqual(runner.main(), 0)

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
                                engine_path = root / 'jterm_core/crates/jterm_engine' if nested else root / 'jterm_engine'
                                self.assertEqual((engine_path / 'src').resolve(), engine / 'src')
                                self.assertIn('patch."https://github.com/beamiter/jterm_core.git".jterm_engine.path='
                                              + json.dumps(str(engine_path)), command)
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


    def test_aether_nested_crates_keep_all_locks_private(self):
        for selected in (*runner.AETHER_CRATES, 'all'):
            with self.subTest(selected=selected):
                calls = []
                def run(command, cwd, env):
                    cwd = Path(cwd)
                    calls.append(cwd.name)
                    if cwd.name not in runner.AETHER_CRATES.values():
                        return type('Result', (), {'returncode': 0})()
                    root = cwd.parent.parent
                    source = self.family / 'aether'
                    for crate in ('src-tauri', 'bridge', 'session'):
                        mirrored = root / 'aether' / crate
                        self.assertFalse((mirrored / 'Cargo.toml').is_symlink())
                        self.assertFalse((mirrored / 'Cargo.lock').is_symlink())
                        self.assertEqual((mirrored / 'src').resolve(), source / crate / 'src')
                        (mirrored / 'Cargo.lock').write_text('cargo rewrote nested lock')
                    self.assertEqual((cwd / '../bridge').resolve(), root / 'aether/bridge')
                    self.assertEqual((cwd / '../session').resolve(), root / 'aether/session')
                    self.assertEqual((cwd / '../web').resolve(), source / 'web')
                    engine = root / 'jterm_core/crates/jterm_engine' if selected == 'all' else root / 'jterm_engine'
                    self.assertIn('patch."https://github.com/beamiter/jterm_core.git".jterm_engine.path='
                                  + json.dumps(str(engine)), command)
                    self.assertIn('--offline', command)
                    self.assertIn(str(source / cwd.name),
                                  env['CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER'])
                    if selected in runner.AETHER_CRATES:
                        self.assertFalse((root / 'jagent').exists())
                        self.assertFalse((root / 'jterm_core').exists())
                    return type('Result', (), {'returncode': 0})()
                with patch.object(runner, '__file__', str(self.script)), \
                     patch.object(sys, 'argv', [str(RUNNER), selected, 'test']), \
                     patch.object(runner.subprocess, 'check_output', return_value='host: x86_64-unknown-linux-gnu\n'), \
                     patch.object(runner.subprocess, 'run', side_effect=run):
                    self.assertEqual(runner.main(), 0)
                aether_calls = [name for name in calls if name in runner.AETHER_CRATES.values()]
                self.assertEqual(aether_calls, list(runner.AETHER_CRATES.values()) if selected == 'all'
                                 else [runner.AETHER_CRATES[selected]])
                for crate in ('src-tauri', 'bridge', 'session'):
                    self.assertEqual((self.family / 'aether' / crate / 'Cargo.lock').read_text(),
                                     '# private lock fixture\n')


if __name__ == '__main__':
    unittest.main()
