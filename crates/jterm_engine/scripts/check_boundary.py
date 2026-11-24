#!/usr/bin/env python3
"""Source-level extraction checks; these do not replace compiling Rust tests."""
from pathlib import Path
import argparse
import re
import tomllib
import unittest

ENGINE = Path(__file__).resolve().parents[1]
# The crate is independent even when distributed inside jterm_core.
FAMILY = ENGINE.parents[2] if ENGINE.parent.name == 'crates' else ENGINE.parent


class BoundaryTests(unittest.TestCase):
    def test_dependency_direction(self):
        manifest = tomllib.loads((ENGINE / 'Cargo.toml').read_text())
        self.assertFalse(manifest['package']['publish'])
        self.assertEqual(set(manifest['dependencies']), {'smallvec', 'unicode-segmentation', 'unicode-width'})
        for path in (ENGINE / 'src').rglob('*.rs'):
            text = path.read_text()
            self.assertIsNone(re.search(r'\b(?:jterm_core|jagent|egui|iced|gtk|tokio|libc|companion_core)::', text), str(path))
            self.assertIsNone(re.search(r'\bstd::(?:fs|net|process)::', text), str(path))

    def test_real_frontend_consumers(self):
        self.assertIn('pub use jterm_engine::grid::*;', (FAMILY / 'ember/src/terminal/grid.rs').read_text())
        self.assertIn('pub use jterm_engine::HyperlinkId;', (FAMILY / 'ember/src/terminal/hyperlink.rs').read_text())
        for app, parser in [('ember', 'src/terminal/parser.rs'), ('frost', 'src/terminal.rs')]:
            manifest = tomllib.loads((FAMILY / app / 'Cargo.toml').read_text())
            dependency = manifest['dependencies']['jterm_engine']
            if 'path' in dependency:
                # Explicit local candidates remain valid before immutable pinning.
                self.assertEqual((FAMILY / app / dependency['path']).resolve(), ENGINE)
            else:
                core = manifest['dependencies']['jterm_core']
                self.assertEqual(dependency['git'], 'https://github.com/beamiter/jterm_core.git')
                self.assertEqual(dependency['rev'], core['rev'])
                self.assertRegex(dependency['rev'], r'^[0-9a-f]{40}$')
            text = (FAMILY / app / parser).read_text()
            self.assertNotIn('fn control_string_abort(', text)
            self.assertIn('self.parser.next(&input[at..])', text)

    def test_raw_frame_is_actual_frost_paint_storage(self):
        host = (FAMILY / 'frost/src/terminal.rs').read_text()
        cache = host.split('type VisibleCellsCache =', 1)[1].split(';', 1)[0]
        self.assertIn('VisibleRows', cache)
        self.assertNotIn('Vec<Vec<TerminalCell>>', cache)
        capture = host.split('pub fn get_visible_cells(', 1)[1].split('pub fn get_cursor_pos', 1)[0]
        self.assertIn('jterm_engine::frame::CellFrame::new(', capture)
        self.assertIn('old.rows()[index].cells.clone()', capture)
        self.assertIn('self.materialize_scrolled_viewport(rows, cols)', capture)
        self.assertNotIn('process_input', capture)
        self.assertNotIn('parser.', capture)
        view = (FAMILY / 'frost/src/terminal/visible_rows.rs').read_text()
        self.assertIn('Raw(CellFrame)', view)
        widget = (FAMILY / 'frost/src/terminal_view.rs').read_text()
        self.assertIn("grid: crate::terminal::RowView<'a>", widget)
        self.assertIn('self.grid.iter().enumerate()', widget)
        for name in ('frame.rs', 'publication.rs'):
            source = (ENGINE / 'src' / name).read_text()
            self.assertNotRegex(source, r'\b(?:Instant|SystemTime|TerminalState|ProjectedViewport|KittyGraphics)\b')

    def test_frost_retained_history_and_reflow_have_one_owner(self):
        host = (FAMILY / 'frost/src/terminal.rs').read_text()
        self.assertRegex(host, r'pub use jterm_engine::\{[^}]*\bScrollbackLine\b')
        self.assertIn('pub scrollback: RetainedHistory<ScrollbackLine>', host)
        self.assertIn('SearchLine::from_retained(', host)
        self.assertNotRegex(host, r'(?:struct|impl) ScrollbackLine\b')
        for name in ('encode_cells', 'decode_cells', 'reflow_lines_bounded'):
            self.assertNotIn(f'fn {name}(', host)
        normalize = host.split('pub fn normalize_scrollback_width(', 1)[1]
        self.assertIn('std::mem::take(&mut self.scrollback).reflow_frost_legacy(', normalize)
        history = (ENGINE / 'src/retained_history.rs').read_text()
        self.assertIn('impl RetainedHistory<ScrollbackLine> {', history)
        self.assertIn('ScrollbackLine::compress_frost_legacy(cells, wrapped)', history)
        self.assertIn('while result.needs_room_for(&line, row_limit)', history)

    def test_vertical_motion_owns_real_physical_scroll(self):
        engine = (ENGINE / 'src/screen.rs').read_text()
        body = engine.split('pub fn vertical_motion(', 1)[1].split('    /// ED2:', 1)[0]
        self.assertIn('self.scroll_region_one(', body)
        self.assertIn('self.cursor_row == self.scroll_region_bottom', body)
        host = (FAMILY / 'ember/src/terminal/state.rs').read_text()
        wrap = host.split('pub(super) fn wrap_to_next_line(', 1)[1].split('pub(super) fn create_blank_cell', 1)[0]
        self.assertIn('apply_vertical_motion(VerticalMotion::Wrap)', wrap)
        self.assertNotIn('self.screen.cursor_row +=', wrap)
        self.assertRegex(wrap, r'self\s*\.screen\s*\.vertical_motion\(')
        parser = (FAMILY / 'ember/src/terminal/parser.rs').read_text()
        lf = parser.split('fn line_feed(', 1)[1].split('fn handle_esc_dispatch', 1)[0]
        self.assertIn('apply_vertical_motion(VerticalMotion::LineFeed)', lf)
        self.assertNotIn('scroll_region_up(', lf)
        self.assertNotIn('self.screen.cursor_row +=', lf)
        nel = parser.split("b'E' => {", 1)[1].split('}', 1)[0]
        self.assertLess(nel.index('begin_next_line'), nel.index('note_output_cursor_reposition'))
        self.assertLess(nel.index('note_output_cursor_reposition'), nel.index('line_feed'))
        self.assertIn('apply_vertical_motion(VerticalMotion::ReverseIndex)', parser)

        frost = (FAMILY / 'frost/src/terminal.rs').read_text()
        self.assertIn('self.screen.frost_legacy_vertical_cursor_motion(motion)', frost)
        self.assertIn('self.move_vertical(VerticalMotion::Wrap)', frost)
        self.assertIn('self.move_vertical(VerticalMotion::LineFeed)', frost)
        self.assertIn('self.move_vertical(VerticalMotion::ReverseIndex)', frost)
        scroll = frost.split('fn scroll_region_one(', 1)[1].split('fn handle_nf_escape(', 1)[0]
        self.assertLess(scroll.index('.archive_frost_legacy_row('), scroll.index('.scroll_region_one('))
        self.assertLess(scroll.index('.scroll_region_one('), scroll.index('.push_scrollback_compressed_with_options('))
        self.assertNotIn('.shift_rows(', scroll)

    def test_reset_query_has_one_production_owner(self):
        host = (FAMILY / 'ember/src/terminal/parser.rs').read_text()
        for name in ('soft_reset', 'handle_decrqss', 'decrqss_sgr_payload',
                     'sgr_color_params', 'handle_dcs_payload',
                     'decrqm_private_mode_state', 'decrqm_ansi_mode_state'):
            self.assertNotRegex(host, rf'fn {name}\b')
        engine = (ENGINE / 'src/semantics.rs').read_text()
        self.assertIn('soft_reset(screen, protocol)', engine)
        self.assertIn('decrqm_private_mode_state(screen, protocol, mode, true)', engine)
        self.assertIn('decrqm_ansi_mode_state(screen, mode)', engine)
        self.assertNotIn('[DECRQM]', host)
        self.assertRegex(engine, r'Effect::Reply\(decrqss_reply\(\s*screen,\s*&payload\[2\.\.\]')
        frost = (FAMILY / 'frost/src/terminal.rs').read_text()
        for name in ('soft_reset', 'report_private_mode_status',
                     'decrqm_private_mode_state', 'decrqm_ansi_mode_state'):
            self.assertNotRegex(frost, rf'fn {name}\b')
        self.assertIn('decrqm_private_mode_state(screen, protocol, mode, false)', engine)
        self.assertNotIn('dispatch_local(', frost)

    def test_live_parser_has_one_owner_and_ordered_dispatch(self):
        host = (FAMILY / 'ember/src/terminal/mod.rs').read_text().split('pub struct TerminalState {', 1)[1].split('\n}', 1)[0]
        self.assertEqual(host.count('parser: jterm_engine::parser::Parser,'), 1)
        for field in ('utf8_buf', 'utf8_len', 'utf8_expected', 'pending_escape',
                      'pending_apc', 'pending_osc', 'pending_string',
                      'discarding_oversized_apc', 'discarding_apc_prev_escape'):
            self.assertNotRegex(host, rf'(?m)^\s*(?:pub )?{field}:')
        parser = (FAMILY / 'ember/src/terminal/parser.rs').read_text()
        for old in ('resume_pending_apc', 'resume_pending_osc', 'resume_pending_string',
                    'abort_pending_control_string', 'stash_pending_escape', 'consume_utf8_lead'):
            self.assertNotRegex(parser, rf'fn {old}\b')
        self.assertIn('self.parser.next(&input[at..])', parser)
        self.assertIn('at += step.consumed;', parser)
        self.assertIn("b'c' => self.hard_reset()", parser)
        self.assertNotIn('std::mem::take(&mut self.parser)', parser)
        self.assertIn('self.parser.legacy_pending_control()',
                      (FAMILY / 'ember/src/terminal/watch.rs').read_text())
        state = (FAMILY / 'ember/src/terminal/state.rs').read_text()
        constructor = state[state.index('parser: jterm_engine::parser::Parser::'):].split('.expect(', 1)[0]
        self.assertIn('Parser::with_bounded_profile(', constructor)
        self.assertIn('ParserProfile::EmberV1', constructor)
        self.assertNotIn('fn consume_utf8_lead(', state)

    def test_frost_parser_has_one_owner_and_explicit_profile(self):
        text = (FAMILY / 'frost/src/terminal.rs').read_text()
        host = text.split('pub struct TerminalState {', 1)[1].split('\n}', 1)[0]
        self.assertEqual(host.count('parser: Parser,'), 1)
        for field in ('utf8_buf', 'utf8_len', 'utf8_expected', 'pending_escape',
                      'pending_apc', 'pending_osc', 'pending_dcs',
                      'discarding_oversized_apc', 'discarding_apc_prev_escape'):
            self.assertNotRegex(host, rf'(?m)^\s*(?:pub )?{field}:')
        for old in ('resume_pending_apc', 'resume_pending_osc', 'resume_pending_dcs',
                    'abort_pending_control_string', 'consume_utf8_lead', 'parse_csi_params'):
            self.assertNotRegex(text, rf'fn {old}\b')
        self.assertIn('ParserProfile::FrostLegacyV1', text)
        self.assertIn('self.parser.begin_feed()', text)
        self.assertIn('self.parser.pending_kind()', text)
        self.assertRegex(text, r'self\.parser\s*\.set_ascii_batching\(')
        self.assertIn('CaptureEnd::Complete { token, escape }', text)
        self.assertRegex(text, r'ParserEvent::Csi\s*\{\s*raw_params,\s*params,')
        self.assertNotIn('std::mem::take(&mut self.parser)', text)

    def test_ascii_storage_is_shared_with_frost_host_order_preserved(self):
        frost = (FAMILY / 'frost/src/terminal.rs').read_text()
        lane = frost.split('    fn put_ascii_run(', 1)[1].split('    fn create_blank_cell(', 1)[0]
        order = [
            'self.screen.last_printed_char =',
            'while pos < bytes.len()',
            'self.move_vertical(VerticalMotion::Wrap)',
            'self.note_idle_background_cells(',
            '.overwrite_ascii(',
            'self.screen.cursor_col += chunk_len',
            'self.mark_row_dirty(',
            'self.mark_command_echo_extent(',
            'self.screen.pending_wrap = true',
        ]
        self.assertEqual(sorted(order, key=lane.index), order)
        self.assertIn('AsciiOverwritePolicy::FrostLegacy', lane)
        self.assertNotIn('.grid.get_mut(', lane)
        screen = (ENGINE / 'src/screen.rs').read_text()
        shared = screen.split('    pub fn write_ascii_in_row(', 1)[1].split('    pub fn combining_target(', 1)[0]
        self.assertIn('.overwrite_ascii(', shared)
        self.assertIn('AsciiOverwritePolicy::SharedScreen', shared)

    def test_live_screen_has_one_explicit_owner(self):
        for path in ('ember/src/terminal/mod.rs', 'frost/src/terminal.rs'):
            host = (FAMILY / path).read_text().split('pub struct TerminalState {', 1)[1].split('\n}', 1)[0]
            self.assertRegex(host, r'(?m)^\s*(?:pub(?:\(crate\))? )?screen: ScreenState,')
            for field in ('grid', 'alt_grid', 'cursor_row', 'cursor_col', 'pending_wrap',
                          'current_fg', 'current_bg', 'current_flags', 'saved_state',
                          'tab_stops', 'scroll_region_top', 'scroll_region_bottom'):
                self.assertNotRegex(host, rf'(?m)^\s*(?:pub(?:\(crate\))? )?{field}:')
            self.assertNotRegex(host, r'impl(?:<[^>]*>)?\s+(?:std::ops::)?Deref')
        screen = (ENGINE / 'src/screen.rs').read_text()
        live = screen.split('pub struct ScreenState<Grid = TerminalGrid> {', 1)[1].split('\n}', 1)[0]
        self.assertIn('pub grid: Grid,', live)
        self.assertIn('pub alt_grid: Grid,', live)
        self.assertNotRegex(live, r'(?m)^\s*(?:pub )?autowrap:')
        self.assertNotRegex(screen, r'impl(?:<[^>]*>)?\s+(?:std::ops::)?Deref')
        state = (FAMILY / 'ember/src/terminal/state.rs').read_text()
        for operation in ('write_char_in_row', 'step_ascii_write', 'erase_span',
                          'save_cursor_state', 'restore_cursor_state'):
            self.assertIn(f'.{operation}(', state)
        self.assertIn('self.apply_screen_mutation(effect)', state)
        self.assertIn('if let Some(printed) = effect.printed', state)

    def test_aether_uses_the_shared_pty_and_terminal_owners(self):
        adapter = (FAMILY / 'aether/session/src/lib.rs').read_text()
        self.assertIn('pub use jterm_engine::terminal::{HistoryPage, Terminal};', adapter)
        self.assertIn('pub use jterm_runtime::pty_session::Session;', adapter)
        self.assertNotRegex(adapter, r'\b(?:struct (?:Terminal|Session)|openpty|waitpid)\b')
        runtime = (ENGINE.parent / 'jterm_runtime/src/pty_session.rs').read_text()
        self.assertIn('terminal: Emulator,', runtime)
        self.assertIn('terminal: Emulator { engine: terminal },', runtime)
        self.assertIn('self.terminal.apply(&bytes[..n])?', runtime)
        emulator = (ENGINE.parent / 'jterm_runtime/src/terminal.rs').read_text()
        self.assertIn('engine: Terminal,', emulator)
        self.assertIn('self.engine.feed(chunk)', emulator)
        self.assertIn('SessionActor::start_with_owner_writes(', runtime)
        bridge = (FAMILY / 'aether/bridge/src/live.rs').read_text()
        self.assertIn('Session::spawn_with_epoch(rows, cols, epoch)', bridge)
        self.assertIn('let mut session = spawn(frame_epoch)?;', bridge)

    def test_ember_shell_owns_one_shared_io_actor(self):
        adapter = (FAMILY / 'ember/src/shell.rs').read_text().split('#[cfg(test)]', 1)[0]
        self.assertIn('actor: SessionActor<Pty>,', adapter)
        self.assertIn('SessionActor::start(pty, repaint_ctx)', adapter)
        self.assertNotRegex(adapter, r'\b(?:fn (?:io_loop|write_to_pty)|struct WriteQueue)\b')
        source = (FAMILY / 'ember/src/output_batch.rs').read_text()
        self.assertIn('pub use jterm_core::output_batch::*;', source)

    def test_local_pty_launches_share_the_runtime_owner(self):
        for frontend in ('ember', 'frost'):
            with self.subTest(frontend=frontend):
                pty = (FAMILY / frontend / 'src/pty.rs').read_text().split('#[cfg(test)]', 1)[0]
                self.assertIn('launch_pty(PtyLaunch {', pty)
                self.assertIn('master: OwnedFd,', pty)
                self.assertNotRegex(pty, r'libc::(?:fork|openpty|execve)\(')
        session = (ENGINE.parent / 'jterm_runtime/src/pty_session.rs').read_text()
        self.assertIn('launch_standalone_pty(command.spec', session)
        self.assertIn('launch_bundled_bash(BundledBash {', session)
        self.assertEqual(session.count('Self::register_program(pending, lease, terminal)'), 2)
        self.assertNotIn('portable_pty', session)

    def test_region_mutations_execute_in_engine_without_host_policy(self):
        state = (FAMILY / 'ember/src/terminal/state.rs').read_text()
        parser = (FAMILY / 'ember/src/terminal/parser.rs').read_text()
        self.assertEqual(state.count('.scroll_region_one('), 2)
        self.assertIn('.edit_lines(', state)
        self.assertIn('self.apply_region_identity_effects(&effect)', state)
        self.assertIn('self.push_scrollback_transferred_with_options(line, allow_alt_scrollback)', state)
        self.assertIn('self.edit_grid_lines(RegionDirection::Down, count)', parser)
        self.assertIn('self.edit_grid_lines(RegionDirection::Up, count)', parser)
        for old in ('shift_row_id_region_up', 'shift_row_id_region_down',
                    'forget_output_provenance_for_grid_rows'):
            self.assertNotRegex(state, rf'fn {old}\b')
        screen = (ENGINE / 'src/screen.rs').read_text()
        for host_policy in ('inline_cli_owner_active', 'sync_output_active',
                            'row_identity_revision', 'full_screen_scroll_revision',
                            'kitty_graphics', 'scroll_offset', 'max_scrollback'):
            self.assertNotIn(host_policy, screen)

    def test_physical_resize_executes_in_engine_without_archive_policy(self):
        state = (FAMILY / 'ember/src/terminal/state.rs').read_text()
        resize = state.split('pub fn on_resize(', 1)[1].split('pub fn get_dimensions(', 1)[0]
        self.assertIn('.resize_geometry(', resize)
        self.assertIn('self.push_scrollback_transferred_with_options(line, false)', resize)
        self.assertIn('self.kitty_graphics.resize(effect.cols, effect.rows)', resize)
        self.assertIn('!self.screen.use_alt_buffer', resize)
        for old in ('.grid.resize(', '.alt_grid.resize(', '.grid.scroll_up_by(',
                    '.clear_dangling_wide_at_row_end(', 'self.screen.pending_wrap =',
                    'self.screen.tab_stops.resize(', 'self.fill_untracked_grid_row_ids('):
            self.assertNotIn(old, resize)
        screen = (ENGINE / 'src/screen.rs').read_text()
        geometry = screen.split('pub fn resize_geometry(', 1)[1].split('\n    }', 1)[0]
        self.assertIn('self.grid.resize(rows, cols, blank)', geometry)
        self.assertIn('self.alt_grid.resize(rows, cols, hidden_blank)', geometry)
        self.assertNotIn('self.use_alt_buffer', geometry)
        self.assertNotIn('self.saved_primary_screen_state', geometry)
        frost = (FAMILY / 'frost/src/terminal.rs').read_text()
        frost_resize = frost.split('pub fn on_resize(', 1)[1].split('\n    }', 1)[0]
        self.assertIn('self.screen.resize_frost_legacy(rows, cols)', frost_resize)
        self.assertIn('for line in geometry.transferred', frost_resize)
        self.assertNotIn('.grid.resize', frost_resize)

    def test_alternate_transitions_execute_in_screen_with_host_hooks(self):
        parser = (FAMILY / 'ember/src/terminal/parser.rs').read_text()
        transition = parser.split('pub(super) fn enter_alt_screen(', 1)[1].split('fn sanitized_title(', 1)[0]
        self.assertIn('.enter_alternate_screen(save_cursor, clear, &self.protocol)', transition)
        self.assertIn('.leave_alternate_screen(restore_cursor)', transition)
        self.assertEqual(transition.count('self.protocol.switch_screen()'), 2)
        self.assertEqual(transition.count('self.watch.boundary()'), 2)
        self.assertEqual(transition.count('self.kitty_graphics.switch_screen()'), 2)
        self.assertIn('self.invalidate_grid_mutation_spans(&spans)', transition)
        self.assertIn('self.kitty_graphics.clear_current_screen_placements()', transition)
        self.assertIn('if let Some(autowrap) = effect.restore_autowrap', transition)
        self.assertNotIn('std::mem::swap(', transition)
        self.assertNotIn('SavedCursorState {', transition)
        self.assertNotIn('self.screen.pending_wrap =', transition)
        self.assertNotIn('fn clear_screen(', parser)
        screen = (ENGINE / 'src/screen.rs').read_text()
        swaps = screen.split("impl<Grid> AlternateScreenControls<'_, Grid> {", 1)[1].split('/// Tab stops', 1)[0]
        self.assertEqual(swaps.count('std::mem::swap(self.grid, self.alt_grid)'), 2)
        self.assertIn('.enter(save_cursor, protocol.modes().contains(&7))', screen)
        self.assertIn('.leave(AlternateRestorePolicy::Standard', screen)
        self.assertIn('self.grid.cells_mut().fill(blank)', screen)

        frost = (FAMILY / 'frost/src/terminal.rs').read_text()
        frost_modes = frost.split('fn enter_alternate_screen(', 1)[1].split('pub fn scrollback_len(', 1)[0]
        entry, exit = frost_modes.split('fn leave_alternate_screen(', 1)
        self.assertLess(entry.index('.enter(true, autowrap)'), entry.index('self.protocol.switch_screen()'))
        self.assertLess(entry.index('self.protocol.switch_screen()'), entry.index('self.clear_screen()'))
        self.assertLess(exit.index('.leave(AlternateRestorePolicy::Frost)'), exit.index('self.protocol.switch_screen()'))
        self.assertEqual(frost_modes.count('.alternate_screen_controls()'), 2)
        self.assertIn('grid: &mut self.grid,', screen)
        self.assertIn('alt_grid: &mut self.alt_grid,', screen)
        self.assertNotIn('std::mem::swap(', frost_modes)
        self.assertNotIn('SavedCursorState {', frost_modes)

    def test_local_semantics_are_live_and_not_duplicated(self):
        host = (FAMILY / 'ember/src/terminal/parser.rs').read_text()
        self.assertIn('semantics::dispatch_local(', host)
        self.assertIn('self.apply_local_semantic_effects(effects)', host)
        self.assertLess(host.index('semantics::dispatch_local('), host.index('match event {'))
        self.assertNotIn('fn handle_sgr(', host)
        self.assertNotIn('fn parse_sgr_extended_color(', host)
        self.assertIn('Effect::Row(mutation) => self.apply_screen_mutation(mutation)', host)
        self.assertIn('Effect::GlobalBackground(color) => self.global_bg = color', host)
        self.assertIn("'h' =>", host)
        self.assertIn("'l' =>", host)
        semantic = (ENGINE / 'src/semantics.rs').read_text()
        self.assertIn('if !local {', semantic)
        self.assertIn('return Dispatch::Unhandled;', semantic)
        frost = (FAMILY / 'frost/src/terminal.rs').read_text()
        csi = frost.split('fn handle_escape_sequence(', 1)[1].split('fn decaln(', 1)[0]
        self.assertIn('dispatch_frost_legacy_csi(', csi)
        self.assertIn('Effect::Reply(bytes)', csi)
        self.assertIn('Effect::GlobalBackground(color)', csi)
        self.assertIn('Effect::Row(mutation) => self.mark_row_dirty(mutation.row)', csi)
        fallback = csi.split('let count = first.unwrap_or(1)', 1)[1]
        for final_byte in ('A', 'B', 'H', 'f', 'g', 'm', 'c', 'p', 'r', 's', 'u', 'K', '@', 'P', 'X'):
            self.assertNotRegex(fallback, rf"(?m)^\s*'{final_byte}'\s*(?:=>|if)")
        frost_semantics = semantic.split('pub fn dispatch_frost_legacy_csi(', 1)[1]
        self.assertIn('frost_legacy_sgr(', frost_semantics.split('fn set_xterm_modify_other_keys(', 1)[0])
        self.assertNotIn('fn handle_sgr(', frost)
        self.assertNotIn('fn parse_sgr_groups(', frost)
        self.assertIn('self.screen.insert_frost_legacy_blanks(width)', frost)
        self.assertIn('screen.insert_frost_legacy_blanks(count)', semantic)
        self.assertNotIn('fn shift_cells_right(', frost)

        # Pure Ember mode mutations have one production owner; admission,
        # parameter order and lifecycle hooks remain in the frontend.
        modes = host.split('pub(super) fn set_mode(', 1)[1]
        self.assertEqual(modes.count('apply_ember_mode('), 4)
        for field in ('origin_mode', 'insert_mode', 'pending_wrap'):
            self.assertNotIn('self.screen.' + field + ' =', modes)
        self.assertNotIn('modes_mut().insert(mode)', modes)
        self.assertNotIn('modes_mut().remove(&mode)', modes)
        pure = (ENGINE / 'src/mode_mutation.rs').read_text()
        self.assertIn('1004 | 47 | 1047 | 1048 | 1049 | 2026 | 5522 => return false', pure)
        self.assertNotIn('screen.pending_wrap =', pure.split('pub enum FrostModeEffect', 1)[0])
        frost_modes = frost.split('fn apply_mode(', 1)[1].split('fn enter_alternate_screen(', 1)[0]
        self.assertRegex(
            frost_modes,
            r'apply_frost_mode\(\s*&mut self\.screen,\s*&mut self\.protocol,\s*namespace,\s*mode,\s*enabled,?\s*\)',
        )
        self.assertIn('FrostModeEffect::MouseReportingChanged => self.projection_scroll_guard.revoke()', frost_modes)
        self.assertIn('self.finish_synchronized_output()', frost_modes)
        self.assertNotIn('modes_mut().insert(mode)', frost_modes)
        self.assertNotIn('modes_mut().remove(&mode)', frost_modes)

    def test_osc_color_commands_have_one_shared_owner(self):
        frost = (FAMILY / 'frost/src/terminal.rs').read_text()
        adapter = frost.split('    fn handle_osc_color(', 1)[1].split('    ///', 1)[0]
        self.assertIn('.apply_frost_osc(command, value, self.osc_reply_terminator, defaults)', adapter)
        for name in ('parse_color_spec', 'handle_osc_palette', 'append_osc_color_response'):
            self.assertNotIn(f'fn {name}(', frost)
        ember = (FAMILY / 'ember/src/terminal/state.rs').read_text()
        self.assertIn('.apply_ember_osc(command, value, self.osc_reply_terminator)', ember)
        colors = (ENGINE / 'src/colors.rs').read_text()
        self.assertIn('OscColorProfile::Ember,', colors)
        self.assertIn('OscColorProfile::Frost)', colors)

    def test_fullscreen_fill_authority_and_host_order(self):
        host = (FAMILY / 'ember/src/terminal/parser.rs').read_text()
        ed = host.split('pub(super) fn erase_screen(&mut self) {', 1)[1].split('\n    }', 1)[0]
        dec = host.split('fn screen_alignment_test(&mut self) {', 1)[1].split('\n    }', 1)[0]
        for body, method in [(ed, 'erase_screen'), (dec, 'screen_alignment_test')]:
            self.assertIn('self.screen.' + method + '();', body)
            self.assertNotIn('iter_mut()', body)
            self.assertNotIn('TerminalCell {', body)
            self.assertLess(body.index('invalidate_grid_mutation_spans'), body.index('self.screen.' + method + '();'))
            self.assertLess(body.index('self.screen.' + method + '();'), body.index('self.dirty_region.mark_all'))
        # ED2's pre-invalidation latch clear is retained for host hook prestate;
        # the engine independently guarantees its standalone clear contract.
        self.assertLess(ed.index('self.screen.pending_wrap = false'), ed.index('invalidate_grid_mutation_spans'))
        self.assertLess(ed.index('mark_rows_dirty'), ed.index('kitty_graphics.clear_placements'))
        self.assertLess(dec.index('screen_alignment_test();'), dec.index('note_output_cursor_reposition(true)'))
        self.assertLess(dec.index('note_output_cursor_reposition(true)'), dec.index('self.dirty_region.mark_all'))
        self.assertNotIn('kitty_graphics', dec)
        self.assertNotIn('archive_visible_screen', dec)

    def test_storage_vectors_remain_private(self):
        text = (ENGINE / 'src/grid.rs').read_text()
        self.assertIn('cells: Vec<TerminalCell>', text)
        self.assertIn('row_ids: Vec<RawRowId>', text)
        self.assertNotRegex(text, r'pub(?:\([^)]*\))?\s+(?:cells|row_ids):')
        self.assertIn('pub fn cells_mut(&mut self) -> &mut [TerminalCell]', text)
        self.assertIn('pub fn row_ids_mut(&mut self) -> &mut [RawRowId]', text)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--engine-only', action='store_true',
                        help='check pure crate boundaries without frontend checkouts')
    args = parser.parse_args()
    if args.engine_only:
        suite = unittest.TestSuite(BoundaryTests(name) for name in (
            'test_dependency_direction', 'test_storage_vectors_remain_private'))
    else:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(BoundaryTests)
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
