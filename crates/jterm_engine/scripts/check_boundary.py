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
        self.assertEqual(set(manifest['dependencies']), {'smallvec'})
        for path in (ENGINE / 'src').glob('*.rs'):
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

    def test_ember_reset_query_has_one_production_owner(self):
        host = (FAMILY / 'ember/src/terminal/parser.rs').read_text()
        for name in ('soft_reset', 'handle_decrqss', 'decrqss_sgr_payload',
                     'sgr_color_params', 'handle_dcs_payload',
                     'decrqm_private_mode_state', 'decrqm_ansi_mode_state'):
            self.assertNotRegex(host, rf'fn {name}\b')
        engine = (ENGINE / 'src/semantics.rs').read_text()
        self.assertIn('soft_reset(screen, protocol)', engine)
        self.assertIn('decrqm_private_mode_state(screen, protocol, mode)', engine)
        self.assertIn('decrqm_ansi_mode_state(screen, mode)', engine)
        self.assertNotIn('[DECRQM]', host)
        self.assertIn('Effect::Reply(decrqss_reply(screen, &payload[2..]))', engine)
        frost = (FAMILY / 'frost/src/terminal.rs').read_text()
        self.assertIn('fn soft_reset(', frost)
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
        self.assertIn('parser: jterm_engine::parser::Parser::new(', state)
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
                    'abort_pending_control_string', 'consume_utf8_lead'):
            self.assertNotRegex(text, rf'fn {old}\b')
        self.assertIn('ParserProfile::FrostLegacyV1', text)
        self.assertIn('self.parser.begin_feed()', text)
        self.assertIn('self.parser.pending_kind()', text)
        self.assertIn('self.parser.set_ascii_batching(', text)
        self.assertIn('CaptureEnd::Complete { token, escape }', text)
        self.assertNotIn('std::mem::take(&mut self.parser)', text)

    def test_live_screen_has_one_explicit_owner(self):
        host = (FAMILY / 'ember/src/terminal/mod.rs').read_text().split('pub struct TerminalState {', 1)[1].split('\n}', 1)[0]
        self.assertIn('pub screen: ScreenState,', host)
        for field in ('grid', 'alt_grid', 'cursor_row', 'cursor_col', 'pending_wrap',
                      'current_fg', 'current_bg', 'current_flags', 'saved_state',
                      'tab_stops', 'scroll_region_top', 'scroll_region_bottom'):
            self.assertNotRegex(host, rf'(?m)^\s*(?:pub )?{field}:')
        screen = (ENGINE / 'src/screen.rs').read_text()
        live = screen.split('pub struct ScreenState {', 1)[1].split('\n}', 1)[0]
        self.assertNotRegex(live, r'(?m)^\s*(?:pub )?autowrap:')
        self.assertNotRegex(host + screen, r'impl(?:<[^>]*>)?\s+(?:std::ops::)?Deref')
        state = (FAMILY / 'ember/src/terminal/state.rs').read_text()
        for operation in ('write_char_in_row', 'write_ascii_in_row', 'erase_span',
                          'save_cursor_state', 'restore_cursor_state'):
            self.assertIn(f'.{operation}(', state)
        self.assertIn('self.apply_screen_mutation(effect)', state)
        self.assertIn('if let Some(printed) = effect.printed', state)

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
        geometry = screen.split('pub fn resize_geometry(', 1)[1].split('pub fn scroll_region_one(', 1)[0]
        self.assertIn('self.grid.resize(rows, cols, blank)', geometry)
        self.assertIn('self.alt_grid.resize(rows, cols, hidden_blank)', geometry)
        self.assertNotIn('self.use_alt_buffer', geometry)
        self.assertNotIn('self.saved_primary_screen_state', geometry)

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
        self.assertIn('std::mem::swap(&mut self.grid, &mut self.alt_grid)', screen)
        self.assertIn('self.grid.cells_mut().fill(blank)', screen)

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
