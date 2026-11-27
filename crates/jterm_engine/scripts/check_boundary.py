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
    aether_checkout = FAMILY / 'aether'

    def test_dependency_direction(self):
        manifest = tomllib.loads((ENGINE / 'Cargo.toml').read_text())
        self.assertFalse(manifest['package']['publish'])
        self.assertEqual(set(manifest['dependencies']), {'smallvec', 'unicode-segmentation', 'unicode-width', 'unicode-normalization'})
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
            self.assertIn(f'NativeTerminal::feed_{app}(', text)
            self.assertNotIn('self.vt.parser.next(&input[at..])', text)

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

    def test_native_master_write_and_resize_have_one_runtime_owner(self):
        runtime = (ENGINE.parent / 'jterm_runtime/src/pty_io.rs').read_text()
        self.assertEqual(runtime.count('libc::write('), 1)
        self.assertEqual(runtime.count('libc::TIOCSWINSZ'), 1)
        self.assertIn('attempt(PtyWriteAttempt::new(master, bytes))', runtime)
        for name in ('ember', 'frost'):
            host = (FAMILY / name / 'src/pty.rs').read_text().split('#[cfg(not(unix))]', 1)[0]
            write = host.split('pub fn write(', 1)[1].split('\n        }', 1)[0]
            resize = host.split('pub fn resize(', 1)[1].split('\n        }', 1)[0]
            self.assertRegex(write, r'write_native_master\(\s*self\.master\.as_fd\(\),\s*data,')
            self.assertRegex(resize, r'resize_master\(\s*self\.master\.as_fd\(\),')
            self.assertNotRegex(host, r'libc::(?:write\(|TIOCSWINSZ)')
        frost = (FAMILY / 'frost/src/pty.rs').read_text()
        self.assertIn('with_native_input_write(|| attempt.run())', frost)
        self.assertIn('Self::write_admitted(PtyWriteAttempt::new', frost)

    def test_native_window_queries_have_one_reply_and_title_owner(self):
        shared = (ENGINE / 'src/native_window_ops.rs').read_text().split('#[cfg(test)]', 1)[0]
        self.assertIn('titles.apply_title_window_op(params, report_title)', shared)
        for op in (11, 13, 14, 16, 18, 19):
            self.assertRegex(shared, rf'(?m)^\s+{op} =>')
        for name, path, pixels in (
            ('ember', 'src/terminal/parser.rs', 'Cell'),
            ('frost', 'src/terminal.rs', 'Viewport'),
        ):
            host = (FAMILY / name / path).read_text()
            handler = host.split('fn handle_window_ops(', 1)[1].split('\n    }', 1)[0]
            self.assertIn('apply_native_window_op(', handler)
            self.assertIn(f'NativeWindowPixels::{pixels}', handler)
            self.assertIn('&mut self.output_buffer', handler)
            self.assertNotRegex(handler, r'format!|match op|output_buffer\.extend|\.titles\.(?:save|restore)\(')

    def test_osc_framing_notifications_and_native_dispatch_have_one_owner(self):
        engine = (ENGINE / 'src/osc.rs').read_text().split('#[cfg(test)]', 1)[0]
        self.assertIn('*reply_terminator = self.terminator()', engine)
        self.assertIn('self.marker_position(markers, end)', engine)
        self.assertIn('queue.len() >= PENDING_NOTIFICATION_LIMIT', engine)
        self.assertIn('queue.push((title, body))', engine)
        for name, path in (('ember', 'src/terminal/parser.rs'), ('frost', 'src/terminal.rs')):
            host = (FAMILY / name / path).read_text().split('\nmod tests {', 1)[0]
            handler = host.split('fn handle_osc_payload(', 1)[1].split('\n    }', 1)[0]
            self.assertIn('OscPacket::decode(payload, bel_terminated).native_event(', handler)
            self.assertIn('notification.enqueue(', handler)
            self.assertNotRegex(handler, r'from_utf8|split_once|markers\.osc\(|notifications\.push')
        terminal = (ENGINE / 'src/terminal.rs').read_text()
        self.assertEqual(terminal.count('OscPacket::decode('), 1)
        self.assertIn('packet.code() == crate::osc::OscCode::Hyperlink', terminal)

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
        self.assertIn('result.retain_latest(line, row_limit, drop)', history)

    def test_frost_osc8_interner_owns_admission_and_link_state(self):
        engine = (ENGINE / 'src/hyperlink.rs').read_text().split('pub struct FrostHyperlinkTable', 1)[1]
        self.assertIn('screen.current_hyperlink = fields', engine)
        self.assertIn('self.targets.push(Arc::clone(&target))', engine)
        self.assertIn('self.keys.insert(target, key)', engine)
        host = (FAMILY / 'frost/src/terminal.rs').read_text().split('\nmod tests {', 1)[0]
        self.assertIn('osc8: FrostHyperlinkTable', host)
        self.assertIn('self.osc8.apply_osc8(', host)
        self.assertIn('self.osc8.resolve(', host)
        self.assertIn('self.osc8.clear()', host)
        self.assertNotRegex(host, r'osc8_hyperlink_keys|struct Osc8Hyperlink')
        self.assertIn('is_openable_url(uri)', host)

    def test_vertical_motion_owns_real_physical_scroll(self):
        engine = (ENGINE / 'src/screen.rs').read_text()
        body = engine.split('pub fn vertical_motion(', 1)[1].split('    /// ED2:', 1)[0]
        self.assertIn('self.scroll_region_one(', body)
        self.assertIn('self.cursor_row == self.scroll_region_bottom', body)
        host = (FAMILY / 'ember/src/terminal/state.rs').read_text()
        wrap = host.split('pub(super) fn wrap_to_next_line(', 1)[1].split('pub(super) fn create_blank_cell', 1)[0]
        self.assertIn('apply_vertical_motion(VerticalMotion::Wrap)', wrap)
        self.assertNotIn('self.vt.screen.cursor_row +=', wrap)
        self.assertRegex(wrap, r'self\s*\.vt\s*\.screen\s*\.vertical_motion\(')
        parser = (FAMILY / 'ember/src/terminal/parser.rs').read_text()
        lf = parser.split('fn line_feed(', 1)[1].split('fn handle_esc_dispatch', 1)[0]
        self.assertIn('apply_vertical_motion(VerticalMotion::LineFeed)', lf)
        self.assertNotIn('scroll_region_up(', lf)
        self.assertNotIn('self.vt.screen.cursor_row +=', lf)
        owner = (ENGINE / 'src/vt_state.rs').read_text().split('#[cfg(test)]', 1)[0]
        nel = owner.split("b'E' => {", 1)[1].split('_ =>', 1)[0]
        self.assertLess(nel.index('begin_next_line'), nel.index('CursorReposition'))
        self.assertLess(nel.index('CursorReposition'), nel.index('NativeEffect::Vertical'))
        self.assertIn('VerticalMotion::ReverseIndex', owner)
        self.assertIn('self.apply_vertical_motion(motion)', parser)

        frost = (FAMILY / 'frost/src/terminal.rs').read_text()
        self.assertIn('self.vt.screen.frost_legacy_vertical_cursor_motion(motion)', frost)
        self.assertIn('self.move_vertical(VerticalMotion::Wrap)', frost)
        self.assertIn('FrostStateEffect::Vertical(motion) => self.move_vertical(motion)', frost)
        self.assertIn('FrostStateEffect::Vertical(VerticalMotion::LineFeed)', (ENGINE / 'src/frost_controls.rs').read_text())
        self.assertIn('FrostStateEffect::Vertical(VerticalMotion::ReverseIndex)', (ENGINE / 'src/frost_controls.rs').read_text())
        scroll = frost.split('fn scroll_region_one(', 1)[1].split('pub(crate) fn watch_generation(', 1)[0]
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
        self.assertRegex(engine, r'Effect::Reply\(decrqss_reply\(\s*screen,\s*protocol,\s*&payload\[2\.\.\]')
        for path in ('src/terminal.rs', 'src/vt_state.rs'):
            self.assertIn('semantics::restorable_decrqss_reply(', (ENGINE / path).read_text())
        frost = (FAMILY / 'frost/src/terminal.rs').read_text()
        for name in ('soft_reset', 'report_private_mode_status',
                     'decrqm_private_mode_state', 'decrqm_ansi_mode_state'):
            self.assertNotRegex(frost, rf'fn {name}\b')
        self.assertIn('decrqm_private_mode_state(screen, protocol, mode, false)', engine)
        self.assertNotIn('dispatch_local(', frost)

    def test_live_parser_has_one_owner_and_ordered_dispatch(self):
        host = (FAMILY / 'ember/src/terminal/mod.rs').read_text().split('pub struct TerminalState {', 1)[1].split('\n}', 1)[0]
        self.assertEqual(host.count('vt: jterm_engine::native_terminal::NativeTerminal,'), 1)
        for field in ('parser', 'utf8_buf', 'utf8_len', 'utf8_expected', 'pending_escape', 'pending_apc', 'pending_osc'):
            self.assertNotRegex(host, rf'(?m)^\s*(?:pub )?{field}:')
        parser = (FAMILY / 'ember/src/terminal/parser.rs').read_text()
        self.assertIn('NativeTerminal::feed_ember(', parser)
        self.assertNotIn('fn dispatch_feed_step(', parser)
        self.assertNotIn('std::mem::take(&mut self.vt.parser)', parser)
        self.assertIn('self.vt.parser.legacy_pending_control()', (FAMILY / 'ember/src/terminal/watch.rs').read_text())
        state = (FAMILY / 'ember/src/terminal/state.rs').read_text()
        self.assertIn('Parser::with_bounded_profile(', state)
        self.assertIn('ParserProfile::EmberV1', state)
        owner = (ENGINE / 'src/vt_state.rs').read_text().split('#[cfg(test)]', 1)[0]
        self.assertIn('pub parser: Parser,', owner)
        self.assertIn('let state = owner(h);', owner)
        self.assertIn('&mut state.parser', owner)
        self.assertNotIn('mem::take', owner)

    def test_frost_parser_has_one_owner_and_explicit_profile(self):
        text = (FAMILY / 'frost/src/terminal.rs').read_text()
        host = text.split('pub struct TerminalState {', 1)[1].split('\n}', 1)[0]
        self.assertEqual(host.count('vt: jterm_engine::native_terminal::NativeTerminal,'), 1)
        self.assertNotIn('parser: Parser,', host)
        self.assertIn('ParserProfile::FrostLegacyV1', text)
        self.assertIn('NativeTerminal::feed_frost(', text)
        self.assertIn('self.vt.parser.pending_kind()', text)
        self.assertNotIn('match event', text.split('fn process_input_inner(', 1)[1].split('fn handle_escape_sequence(', 1)[0])
        owner = (ENGINE / 'src/vt_state.rs').read_text().split('#[cfg(test)]', 1)[0]
        self.assertIn('state.parser.set_ascii_batching(', owner)
        self.assertIn('CaptureEnd::Complete { token, escape }', owner)
        self.assertRegex(owner, r'Event::Csi\s*\{\s*raw_params,\s*params,')

    def test_terminal_feeds_share_ordered_scanner_progress(self):
        driver = (ENGINE / 'src/feed.rs').read_text().split('#[cfg(test)]', 1)[0]
        self.assertIn('parser(terminal).begin_feed()', driver)
        self.assertIn('parser.next(&input[at..])', driver)
        self.assertLess(driver.index('at += step.consumed;'), driver.index('dispatch('))
        owner = (ENGINE / 'src/vt_state.rs').read_text().split('#[cfg(test)]', 1)[0]
        self.assertIn('VtProfile::Hosted => FeedTail::InputConsumed', owner)
        self.assertIn('FeedTail::DrainPending', owner)
        self.assertIn('step.end_offset', owner)
        hosted = (ENGINE / 'src/terminal.rs').read_text()
        self.assertIn('VtState::drive(', hosted)
        self.assertNotIn('drive_feed(', hosted)
        for app, path in [('ember', 'src/terminal/parser.rs'), ('frost', 'src/terminal.rs')]:
            host = (FAMILY / app / path).read_text()
            self.assertNotIn('drive_feed(', host)
            self.assertIn(f'NativeTerminal::feed_{app}(', host)

    def test_native_region_commands_have_one_ordered_engine_owner(self):
        for app in ('ember', 'frost'):
            dispatch = (ENGINE / f'src/{app}_csi.rs').read_text().split('#[cfg(test)]', 1)[0]
            self.assertIn('prepare_region_operation(', dispatch)
            self.assertNotRegex(dispatch, r"(?m)^\s*'[LMST]'\s*(?:\||=>)")
        state = (FAMILY / 'ember/src/terminal/state.rs').read_text()
        callback = state.split('fn apply_csi_region_step(', 1)[1].split('fn edit_grid_lines(', 1)[0]
        self.assertIn('self.apply_vertical_region_effects(', callback)
        self.assertIn('self.apply_region_identity_effects(', callback)
        transaction = (ENGINE / 'src/csi_transaction.rs').read_text()
        self.assertRegex(transaction, r'operation\s*\.step\(screen, allow_alt_archive')
        for method in ('scroll_region_up', 'scroll_region_down', 'edit_grid_lines'):
            self.assertRegex(state, rf'#\[cfg\(test\)\]\s+pub\(super\) fn {method}\(')
        shared = (ENGINE / 'src/region_ops.rs').read_text().split('#[cfg(test)]', 1)[0]
        self.assertIn('screen.edit_lines(', shared)
        self.assertIn('frost_legacy_splice_lines(', shared)
        self.assertIn('screen.scroll_region_one(', shared)
        self.assertLess(shared.index('archive_frost_legacy_row('), shared.index('screen.scroll_region_one('))

    def test_native_display_erase_admission_and_row_transactions_live_in_engine(self):
        for app, path in [('ember', 'src/terminal/parser.rs'), ('frost', 'src/terminal.rs')]:
            dispatch = (ENGINE / f'src/{app}_csi.rs').read_text().split('#[cfg(test)]', 1)[0]
            self.assertIn('prepare_display_erase(', dispatch)
            self.assertNotRegex(dispatch, r"(?m)^\s*'J'\s*(?:if|=>)")
            host = (FAMILY / app / path).read_text()
            self.assertIn('DisplayEraseStep::ScreenErased', host)
            if app == 'ember':
                self.assertIn('CsiTransactionEffect::EraseScreen', host)
                self.assertIn('CsiTransactionEffect::EraseSavedLines', host)
                self.assertIn('self.apply_screen_mutation(effect)', host)
        transaction = (ENGINE / 'src/csi_transaction.rs').read_text()
        self.assertIn('rows.step(screen)', transaction)
        self.assertIn('DisplayErase::Screen =>', transaction)
        self.assertIn('DisplayErase::SavedLines =>', transaction)
        shared = (ENGINE / 'src/display_erase.rs').read_text().split('#[cfg(test)]', 1)[0]
        prepare = shared.split('pub fn prepare_display_erase(', 1)[1].split('impl DisplayEraseRows', 1)[0]
        self.assertIn('screen: &ScreenState', prepare)
        self.assertNotIn('&mut ScreenState', prepare)
        self.assertIn('screen.erase_span(', shared)
        self.assertRegex(shared, r'screen\s*\.erase_frost_legacy_display\(')

    def test_native_token_selection_has_shared_span_and_classifier_owner(self):
        for path in ('ember/src/terminal/state.rs', 'frost/src/terminal.rs'):
            host = (FAMILY / path).read_text()
            adapter = host.split('fn select_extended_token_span(', 1)[1].split('fn copy_projected_selection(', 1)[0]
            self.assertIn('jterm_engine::selection::extended_token_span(line, start_col)', adapter)
            self.assertNotRegex(adapter, r'\b(?:for|while)\b')
        for path in ('ember/src/terminal/mod.rs', 'frost/src/terminal.rs'):
            host = (FAMILY / path).read_text()
            for helper in ('is_extended_token_char', 'is_extended_token_separator',
                           'is_token_prefix_wrapper', 'is_token_suffix_wrapper'):
                self.assertNotIn(f'fn {helper}(', host)
        shared = (ENGINE / 'src/selection.rs').read_text()
        token = shared.split('pub fn extended_token_span(', 1)[1].split('#[cfg(test)]', 1)[0]
        self.assertIn('.wide_continuation()', token)
        self.assertIn('is_extended_token_char(', token)
        self.assertIn('is_extended_token_separator(', token)

    def test_native_raw_copy_uses_shared_inclusive_selection_rules(self):
        sources = (
            ('ember/src/terminal/state.rs', 'copy_displayed_selection_with_soft_spaces'),
            ('frost/src/terminal.rs', 'copy_displayed_selection'),
        )
        for path, method in sources:
            host = (FAMILY / path).read_text()
            adapter = host.split(f'    fn {method}(', 1)[1].split('    pub fn scroll(', 1)[0]
            self.assertIn('jterm_engine::selection::', adapter)
            self.assertIn('SelectionCopy::new(', adapter)
            self.assertIn('.push_row(', adapter)
            self.assertNotIn('.wide_continuation()', adapter)
            self.assertNotIn('.trim_end_matches(', adapter)
            self.assertNotRegex(adapter, r'anchor\s*<=\s*sel\.active')
        shared = (ENGINE / 'src/selection.rs').read_text().split('#[cfg(test)]', 1)[0]
        self.assertIn('.wide_continuation()', shared)
        self.assertIn(".trim_end_matches(' ')", shared)
        self.assertIn('SoftWrapSpaces::Trim', shared)
        self.assertIn('.min(total_rows)', shared)

    def test_scalar_composition_has_one_owner_with_profiled_commit_order(self):
        screen = (ENGINE / 'src/screen.rs').read_text()
        pair = screen.split('pub fn compose_scalar_pair(', 1)[1].split('pub enum Charset', 1)[0]
        self.assertIn('[base, mark].into_iter().nfc()', pair)
        self.assertIn('normalized.next(), normalized.next()', pair)
        commit = screen.split('    fn compose_mark_profile(', 1)[1].split('    /// Commit a normalized', 1)[0]
        self.assertIn('self.replace_combining_target(col, composed)', commit)
        self.assertLess(commit.index('let cell = self.grid.get_mut(row, col)'),
                        commit.index('compose_scalar_pair(cell.character, mark)'))
        ember = (FAMILY / 'ember/src/terminal/state.rs').read_text()
        ember = ember.split('fn apply_combining_mark(', 1)[1].split('fn put_char(', 1)[0]
        self.assertIn('self.vt.screen.compose_combining_mark(mark)', ember)
        frost = (FAMILY / 'frost/src/terminal.rs').read_text()
        frost = frost.split('fn combine_with_previous(', 1)[1].split('fn save_cursor(', 1)[0]
        self.assertIn('self.vt.screen.compose_frost_legacy_mark(mark)', frost)
        ansi = (FAMILY / 'frost/src/ansi.rs').read_text()
        self.assertIn('jterm_engine::screen::compose_scalar_pair(*base, mark)', ansi)
        for host in (ember, frost, ansi):
            self.assertNotIn('.nfc()', host)
        self.assertIn('unicode-normalization = "=0.1.26"', (ENGINE / 'Cargo.toml').read_text())

    def test_ascii_storage_is_shared_with_frost_host_order_preserved(self):
        frost = (FAMILY / 'frost/src/terminal.rs').read_text()
        lane = frost.split('    fn put_ascii_run(', 1)[1].split('    fn create_blank_cell(', 1)[0]
        owner = (ENGINE / 'src/vt_state.rs').read_text().split('#[cfg(test)]', 1)[0]
        self.assertIn('NativeTerminal::ascii_frost(', lane)
        self.assertIn('NativeAsciiWrite::new(', owner)
        self.assertIn('op.step(', owner)
        self.assertIn('NativeAsciiStep::ObserveIdle', lane)
        self.assertIn('NativeAsciiStep::Written', lane)
        self.assertLess(lane.index('self.apply_content_mutation(mutation)'), lane.index('self.mark_command_echo_extent('))
        self.assertNotIn('while pos <', lane)
        self.assertNotIn('.write_frost_legacy_ascii_in_row(', lane)
        shared_batch = (ENGINE / 'src/ascii_write.rs').read_text().split('#[cfg(test)]', 1)[0]
        self.assertIn('screen.last_printed_char = Some(last as char)', shared_batch)
        self.assertIn('screen.take_pending_wrap(self.autowrap)', shared_batch)
        self.assertRegex(shared_batch, r'screen\s*\.write_frost_legacy_ascii_in_row\(')
        self.assertIn('self.pos += consumed', shared_batch)
        self.assertIn('screen.step_ascii_write(', shared_batch)
        self.assertIn('self.scalar_fallback', shared_batch)
        self.assertNotIn('.overwrite_ascii(', lane)
        self.assertNotRegex(lane, r'self\.screen\.(?:cursor_col|pending_wrap)\s*[+]?=')
        self.assertNotIn('.grid.get_mut(', lane)
        screen = (ENGINE / 'src/screen.rs').read_text()
        shared = screen.split('    pub fn write_ascii_in_row(', 1)[1].split('    pub fn combining_target(', 1)[0]
        self.assertIn('.overwrite_ascii(', shared)
        self.assertIn('AsciiOverwritePolicy::SharedScreen', shared)
        self.assertIn('WriteProfile::FrostLegacy => AsciiOverwritePolicy::FrostLegacy', shared)
        self.assertIn('self.cursor_col += count', shared)
        self.assertIn('if profile == WriteProfile::Ember', shared)

    def test_live_screen_has_one_explicit_owner(self):
        for path in ('ember/src/terminal/mod.rs', 'frost/src/terminal.rs'):
            host = (FAMILY / path).read_text().split('pub struct TerminalState {', 1)[1].split('\n}', 1)[0]
            self.assertIn('pub vt: jterm_engine::native_terminal::NativeTerminal,', host)
            self.assertNotIn('screen: ScreenState,', host)
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
        for operation in ('save_cursor_state', 'restore_cursor_state'):
            self.assertIn(f'.{operation}(', state)
        self.assertIn('NativeAsciiWrite::new(', (ENGINE / 'src/vt_state.rs').read_text())
        self.assertIn('self.apply_screen_mutation(effect)', state)
        self.assertIn('if let Some(printed) = effect.printed', state)

    def test_aether_uses_the_shared_pty_and_terminal_owners(self):
        adapter = (self.aether_checkout / 'session/src/lib.rs').read_text()
        self.assertIn('pub use jterm_engine::terminal::{HistoryPage, Terminal};', adapter)
        self.assertIn('pub use jterm_runtime::pty_session::Session;', adapter)
        self.assertNotRegex(adapter, r'\b(?:struct (?:Terminal|Session)|openpty|waitpid)\b')
        runtime = (ENGINE.parent / 'jterm_runtime/src/pty_session.rs').read_text()
        self.assertIn('terminal: Emulator,', runtime)
        self.assertIn('terminal: Emulator { engine: terminal },', runtime)
        self.assertIn('self.terminal.apply(&bytes[..n])?', runtime)
        emulator = (ENGINE.parent / 'jterm_runtime/src/terminal.rs').read_text()
        self.assertIn('engine: Terminal,', emulator)
        self.assertIn('self.engine.feed_bounded_at(chunk,', emulator)
        self.assertIn('SessionActor::start_with_owner_writes(', runtime)
        bridge = (FAMILY / 'aether/bridge/src/live.rs').read_text()
        self.assertIn('Session::spawn_family_with_epoch(rows, cols, epoch, integrated)', bridge)
        self.assertIn('let mut session = spawn(frame_epoch)?;', bridge)

    def test_ember_shell_owns_one_shared_io_actor(self):
        adapter = (FAMILY / 'ember/src/shell.rs').read_text().split('#[cfg(test)]', 1)[0]
        self.assertIn('actor: SessionHandle<Pty>,', adapter)
        self.assertIn('SessionHandle::start_ordered(pty, repaint_ctx)', adapter)
        self.assertNotRegex(adapter, r'\b(?:fn (?:io_loop|write_to_pty)|struct WriteQueue)\b')
        actor = (ENGINE.parent / 'jterm_runtime/src/session_actor.rs').read_text()
        self.assertIn('pub type SessionHandle<T> = SessionActor<T, OrderedOutput>;', actor)
        source = (FAMILY / 'ember/src/output_batch.rs').read_text()
        self.assertIn('pub use jterm_core::output_batch::*;', source)

    def test_local_pty_launches_share_the_runtime_owner(self):
        for frontend in ('ember', 'frost'):
            with self.subTest(frontend=frontend):
                source = (FAMILY / frontend / 'src/pty.rs').read_text()
                pty = source.split('mod lifecycle_tests {', 1)[0]
                self.assertIn('launch_pty(PtyLaunch {', pty)
                self.assertIn('master: OwnedFd,', pty)
                self.assertIn('child: NativePtyChild,', pty)
                self.assertNotRegex(pty, r'libc::(?:fork|openpty|execve|waitpid)\(')
        session = (ENGINE.parent / 'jterm_runtime/src/pty_session.rs').read_text()
        self.assertIn('launch_standalone_pty(spec, forwarded)', session)
        self.assertIn('launch_bundled_bash(BundledBash {', session)
        self.assertEqual(session.count('Self::register_program(pending, lease, terminal)'), 2)
        self.assertNotIn('portable_pty', session)

    def test_region_mutations_execute_in_engine_without_host_policy(self):
        state = (FAMILY / 'ember/src/terminal/state.rs').read_text()
        parser = (FAMILY / 'ember/src/terminal/parser.rs').read_text()
        region = (ENGINE / 'src/region_ops.rs').read_text()
        self.assertIn('screen.scroll_region_one(', region)
        self.assertIn('screen.edit_lines(', region)
        self.assertIn('self.apply_region_identity_effects(&effect)', state)
        self.assertIn('self.push_scrollback_transferred_with_options(line, allow_alt_scrollback)', state)
        self.assertIn('jterm_engine::ember_csi::execute(', parser)
        self.assertNotIn('self.edit_grid_lines(', parser)
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
        self.assertIn('!self.vt.screen.use_alt_buffer', resize)
        for old in ('.grid.resize(', '.alt_grid.resize(', '.grid.scroll_up_by(',
                    '.clear_dangling_wide_at_row_end(', 'self.vt.screen.pending_wrap =',
                    'self.vt.screen.tab_stops.resize(', 'self.fill_untracked_grid_row_ids('):
            self.assertNotIn(old, resize)
        screen = (ENGINE / 'src/screen.rs').read_text()
        geometry = screen.split('pub fn resize_geometry(', 1)[1].split('\n    }', 1)[0]
        self.assertIn('self.grid.resize(rows, cols, blank)', geometry)
        self.assertIn('self.alt_grid.resize(rows, cols, hidden_blank)', geometry)
        self.assertNotIn('self.use_alt_buffer', geometry)
        self.assertNotIn('self.saved_primary_screen_state', geometry)
        frost = (FAMILY / 'frost/src/terminal.rs').read_text()
        frost_resize = frost.split('pub fn on_resize(', 1)[1].split('\n    }', 1)[0]
        self.assertIn('self.vt.screen.resize_frost_legacy(rows, cols)', frost_resize)
        self.assertIn('for line in geometry.transferred', frost_resize)
        self.assertNotIn('.grid.resize', frost_resize)

    def test_native_alternate_state_and_order_live_in_engine(self):
        shared = (ENGINE / 'src/native_alternate.rs').read_text().split('#[cfg(test)]', 1)[0]
        for function, next_function, milestones in [
            ('enter_ember', 'leave_ember', [
                'NativeAlternateHook::PrepareEntry', '.enter_alternate_screen(',
                'NativeAlternateHook::GridSwapped', '.protocol.switch_screen()',
                'NativeAlternateHook::OwnershipChanged', 'NativeAlternateHook::EntryCleared',
            ]),
            ('leave_ember', 'enter_frost', [
                '.leave_alternate_screen(', 'NativeAlternateHook::GridSwapped',
                '.protocol.switch_screen()', 'NativeAlternateHook::OwnershipChanged',
                'exit.global_background', 'exit.restore_autowrap', 'NativeAlternateHook::Repaint',
            ]),
            ('enter_frost', 'leave_frost', [
                '*state.saved_colors', 'NativeAlternateHook::PrepareEntry', '.enter(true, autowrap)',
                '.protocol.switch_screen()', 'NativeAlternateHook::OwnershipChanged',
                'current_hyperlink = HyperlinkId::NONE', 'NativeAlternateHook::ClearSelection',
                'NativeAlternateHook::ClearAlternate', 'modes_mut().insert(mode)',
            ]),
            ('leave_frost', None, [
                '.leave(AlternateRestorePolicy::Frost)', '.protocol.switch_screen()',
                'NativeAlternateHook::OwnershipChanged', 'NativeAlternateHook::FinalizeAlternate',
                'modes_mut().remove(&mode)', 'NativeAlternateHook::ClearSelection',
                '*state.colors', 'NativeAlternateHook::Repaint',
            ]),
        ]:
            section = shared.split(f'pub fn {function}(', 1)[1]
            if next_function:
                section = section.split(f'pub fn {next_function}(', 1)[0]
            with self.subTest(function=function):
                indices = [section.index(milestone) for milestone in milestones]
                self.assertEqual(indices, sorted(indices))

        for name, path, entry, leave in [
            ('ember', 'src/terminal/parser.rs', 'enter_alt_screen', 'exit_alt_screen'),
            ('frost', 'src/terminal.rs', 'enter_alternate_screen', 'leave_alternate_screen'),
        ]:
            front = (FAMILY / name / path).read_text()
            for prefix, wrapper in [('enter', entry), ('leave', leave)]:
                body = front.split(f'fn {wrapper}(', 1)[1].split('\n    }', 1)[0]
                self.assertIn(f'native_alternate::{prefix}_{name}(self,', body)
                self.assertNotIn('switch_screen()', body)
            hooks = (FAMILY / name / 'src/terminal/native_alternate.rs').read_text()
            self.assertIn('fn alternate_hook(', hooks)
            self.assertNotIn('protocol.switch_screen()', hooks)
            self.assertNotIn('alternate_screen_controls()', hooks)
            self.assertNotIn('modes_mut()', hooks)
        screen = (ENGINE / 'src/screen.rs').read_text()
        swaps = screen.split("impl<Grid> AlternateScreenControls<'_, Grid> {", 1)[1].split('/// Tab stops', 1)[0]
        self.assertEqual(swaps.count('std::mem::swap(self.grid, self.alt_grid)'), 2)

    def test_native_osc52_admission_and_queue_have_one_owner(self):
        shared = (ENGINE / 'src/clipboard.rs').read_text().split('#[cfg(test)]', 1)[0]
        self.assertIn('self.pending_queries.push_back(terminator)', shared)
        self.assertIn('self.pending_queries.pop_front()', shared)
        self.assertIn('self.pending_set = Some(text)', shared)
        self.assertIn('self.pending_set.take()', shared)
        for app, path in [('ember', 'src/terminal/state.rs'), ('frost', 'src/terminal.rs')]:
            host = (FAMILY / app / path).read_text()
            body = host.split('fn handle_osc_52(', 1)[1].split('\n    }', 1)[0]
            self.assertIn('self.osc52.admit(', body)
            self.assertNotIn('MAX_BYTES', body)
            self.assertNotIn('push_back', body)
            self.assertRegex(host, r'#\[cfg\(test\)\]\s+(?:pub\(super\) )?fn original_handle_osc_52\(')

    def test_local_semantics_are_live_and_not_duplicated(self):
        host = (FAMILY / 'ember/src/terminal/parser.rs').read_text()
        owner = (ENGINE / 'src/vt_state.rs').read_text().split('#[cfg(test)]', 1)[0]
        self.assertIn('semantics::dispatch_local(', owner)
        self.assertIn('self.apply_local_semantic_effects(effects)', host)
        feed_step = owner.split('fn dispatch_ember<', 1)[1].split('pub fn feed_frost<', 1)[0]
        self.assertLess(feed_step.index('semantics::dispatch_local('), feed_step.index('match event {'))
        self.assertNotIn('fn handle_sgr(', host)
        self.assertNotIn('fn parse_sgr_extended_color(', host)
        self.assertIn('Effect::Row(mutation) => self.apply_screen_mutation(mutation)', host)
        self.assertIn('Effect::GlobalBackground(color) => self.global_bg = color', host)
        self.assertIn('prepare_native_mode_command(', (ENGINE / 'src/ember_csi.rs').read_text())
        semantic = (ENGINE / 'src/semantics.rs').read_text()
        self.assertIn('if !local {', semantic)
        self.assertIn('return Dispatch::Unhandled;', semantic)
        frost = (FAMILY / 'frost/src/terminal.rs').read_text()
        csi = (ENGINE / 'src/frost_csi.rs').read_text().split('#[cfg(test)]', 1)[0]
        self.assertIn('dispatch_frost_legacy_csi(', csi)
        self.assertIn('Effect::Reply(bytes)', frost)
        self.assertIn('Effect::GlobalBackground(color)', frost)
        self.assertIn('Effect::Row(mutation) => self.apply_content_mutation(mutation)', frost)
        mutation = frost.split('    fn apply_content_mutation(', 1)[1].split('\n    fn ', 1)[0]
        self.assertIn('self.mark_row_dirty(mutation.row)', mutation)
        self.assertIn('mutation.changed', mutation)
        fallback = csi.split('RepeatGraphics::prepare(', 1)[1]
        for final_byte in ('A', 'B', 'H', 'f', 'g', 'm', 'c', 'p', 'r', 's', 'u', 'K', '@', 'P', 'X'):
            self.assertNotRegex(fallback, rf"(?m)^\s*'{final_byte}'\s*(?:=>|if)")
        frost_semantics = semantic.split('pub fn dispatch_frost_legacy_csi(', 1)[1]
        self.assertIn('frost_legacy_sgr(', frost_semantics.split('fn set_xterm_modify_other_keys(', 1)[0])
        self.assertNotIn('fn handle_sgr(', frost)
        self.assertNotIn('fn parse_sgr_groups(', frost)
        graphic = frost.split('    fn put_char(', 1)[1].split('    fn put_ascii_run(', 1)[0]
        self.assertIn('NativeTerminal::write_frost(', graphic)
        self.assertIn('GraphicWrite::new_translated(', (ENGINE / 'src/vt_state.rs').read_text())
        shared = (ENGINE / 'src/graphic_write.rs').read_text().split('#[cfg(test)]', 1)[0]
        self.assertIn('screen.take_pending_wrap(self.autowrap)', shared)
        self.assertIn('screen.prepare_character_width(self.width, self.autowrap)', shared)
        self.assertIn('screen.write_frost_legacy_char_in_row(', shared)
        self.assertNotIn('.overwrite_graphic(', graphic)
        self.assertNotRegex(graphic, r'self\.screen\.(?:cursor_col|pending_wrap)\s*[+]?=')
        screen = (ENGINE / 'src/screen.rs').read_text()
        shared_graphic = screen.split('    fn write_char_profile(', 1)[1].split('    pub fn step_ascii_write(', 1)[0]
        self.assertIn('self.insert_frost_legacy_blanks(width)', shared_graphic)
        self.assertIn('WriteProfile::FrostLegacy => WideContinuationProtection::FromBlank', shared_graphic)
        self.assertIn('self.cursor_col += width', shared_graphic)
        self.assertIn('self.last_printed_char = Some(ch)', shared_graphic)
        self.assertIn('screen.insert_frost_legacy_blanks(count)', semantic)
        self.assertNotIn('fn shift_cells_right(', frost)

        # Native h/l admission and pure mutations advance one parameter at a
        # time; only ordered platform/archive hooks remain in the adapters.
        pure = (ENGINE / 'src/mode_mutation.rs').read_text()
        self.assertIn('1004 | 47 | 1047 | 1048 | 1049 | 2026 | 5522 => return false', pure)
        self.assertNotIn('screen.pending_wrap =', pure.split('pub enum FrostModeEffect', 1)[0])
        for app in ('ember', 'frost'):
            dispatch = (ENGINE / f'src/{app}_csi.rs').read_text().split('#[cfg(test)]', 1)[0]
            self.assertIn('prepare_native_mode_command(', dispatch)
            self.assertNotRegex(dispatch, r"(?m)^\s*'[hl]'\s*(?:\||=>)")
        ember = (FAMILY / 'ember/src/terminal/parser.rs').read_text()
        adapter = ember.split('fn apply_mode_effect(', 1)[1].split('pub(super) fn set_mode(', 1)[0]
        self.assertIn('NativeModeEffect::RevokePasteGrant => self.osc5522.revoke()', adapter)
        for mode in (1004, 1048, 5522):
            self.assertNotIn(f'modes_mut().insert({mode})', adapter)
            self.assertNotIn(f'modes_mut().remove(&{mode})', adapter)
        parameter = pure.split('pub fn apply_native_mode_parameter(', 1)[1].split('#[cfg(test)]', 1)[0]
        self.assertIn('screen.save_cursor_state(protocol)', parameter)
        self.assertIn('screen.restore_cursor_state(protocol)', parameter)
        self.assertIn('NativeModeEffect::Reply(b"\\x1b[I")', parameter)
        frost_modes = frost.split('fn apply_mode_effect(', 1)[1].split('fn enter_alternate_screen(', 1)[0]
        self.assertRegex((ENGINE / 'src/csi_transaction.rs').read_text(), r'operation\s*\.step\(screen, protocol, focused\)')
        self.assertRegex(frost_modes, r'NativeModeEffect::MouseReportingChanged\s*=>\s*\{\s*self\.projection_scroll_guard\.revoke\(\);\s*self\.retire_native_wheel\(\);\s*\}')
        self.assertIn('self.finish_synchronized_output()', frost_modes)

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
            self.assertIn('self.vt.screen.' + method + '();', body)
            self.assertNotIn('iter_mut()', body)
            self.assertNotIn('TerminalCell {', body)
            self.assertLess(body.index('invalidate_grid_mutation_spans'), body.index('self.vt.screen.' + method + '();'))
            self.assertLess(body.index('self.vt.screen.' + method + '();'), body.index('self.dirty_region.mark_all'))
        # ED2's pre-invalidation latch clear is retained for host hook prestate;
        # the engine independently guarantees its standalone clear contract.
        self.assertLess(ed.index('self.vt.screen.pending_wrap = false'), ed.index('invalidate_grid_mutation_spans'))
        self.assertLess(ed.index('mark_rows_dirty'), ed.index('kitty_graphics.clear_placements'))
        self.assertLess(dec.index('screen_alignment_test();'), dec.index('note_output_cursor_reposition(true)'))
        self.assertLess(dec.index('note_output_cursor_reposition(true)'), dec.index('self.dirty_region.mark_all'))
        self.assertNotIn('kitty_graphics', dec)
        self.assertNotIn('archive_visible_screen', dec)

    def test_native_ris_state_has_shared_owner(self):
        engine = (ENGINE / 'src/native_reset.rs').read_text()
        self.assertIn('ScreenState::new(rows, cols)', engine)
        self.assertIn('*protocol.modes_mut() = frost_initial_modes()', engine)
        self.assertIn('screen.tab_stops = default_tab_stops(', engine)
        self.assertEqual(engine.count('protocol.reset_keyboard_enhancement();'), 2)
        ember = (FAMILY / 'ember/src/terminal/state.rs').read_text()
        ctor = ember.split('pub fn new_with_host_context_and_sync_output_clock(', 1)[1].split('pub(super) fn decode_base64', 1)[0]
        self.assertIn('native_reset::fresh_ember_state(rows, cols)', ctor)
        self.assertNotIn('TerminalModes::default()', ctor)
        frost = (FAMILY / 'frost/src/terminal.rs').read_text()
        reset = frost.split('    fn full_reset(', 1)[1].split('    fn clear_screen(', 1)[0]
        phases = ['self.leave_alternate_screen(1049)', 'self.record_reset_interrupted_agent_command()',
                  'native_reset::reset_frost_attributes(', 'self.scrollback.clear()',
                  'native_reset::reset_frost_protocol_and_cursor(', 'self.vt.parser.reset()',
                  'self.clear_screen()']
        offsets = [reset.index(phase) for phase in phases]
        self.assertEqual(offsets, sorted(offsets))
        self.assertNotIn('self.vt.screen.current_flags =', reset)
        self.assertNotIn('self.vt.protocol.reset_keyboard_enhancement()', reset)
        for app in ('ember', 'frost'):
            oracle = (FAMILY / app / 'src/terminal/ris_oracles.rs').read_text()
            self.assertNotIn('jterm_engine::native_reset', oracle)
            self.assertIn('fn frozen_ris(', oracle)

    def test_native_graphic_and_rep_transactions_have_one_owner(self):
        shared = (ENGINE / 'src/graphic_write.rs').read_text().split('#[cfg(test)]', 1)[0]
        self.assertIn('screen.translate_char(original)', shared)
        self.assertIn('translate_graphic(ch)', (ENGINE / 'src/vt_state.rs').read_text())
        self.assertIn('take_graphic_charset(ch)', (ENGINE / 'src/screen.rs').read_text())
        self.assertIn('screen.compose_combining_mark(self.translated)', shared)
        self.assertIn('screen.compose_frost_legacy_mark(self.translated)', shared)
        self.assertIn('screen.last_printed_char = Some(self.original)', shared)
        self.assertIn('original: screen.last_printed_char', shared)
        self.assertIn('self.remaining -= 1', shared)
        for app, path in [('ember', 'src/terminal/state.rs'), ('frost', 'src/terminal.rs')]:
            host = (FAMILY / app / path).read_text()
            body = host.split('fn put_char(', 1)[1].split('fn put_ascii_run(', 1)[0]
            self.assertIn(f'NativeTerminal::write_{app}(', body)
            self.assertIn('GraphicWrite::new_translated(', (ENGINE / 'src/vt_state.rs').read_text())
            self.assertNotIn('prepare_character_width(', body)
            self.assertNotIn('last_printed_char =', body)
            dispatch = (ENGINE / f'src/{app}_csi.rs').read_text().split('#[cfg(test)]', 1)[0]
            self.assertIn('RepeatGraphics::prepare(', dispatch)
            self.assertNotRegex(dispatch, r"(?m)^\s*'b'\s*=>")
        transaction = (ENGINE / 'src/csi_transaction.rs').read_text()
        self.assertIn('repeat.next(', transaction)
        self.assertIn('write.step(screen)', transaction)

    def test_native_osc5522_capabilities_and_queues_have_shared_owners(self):
        shared = (ENGINE / 'src/osc5522.rs').read_text().split('#[cfg(test)]', 1)[0]
        for owner in ('EmberOsc5522', 'FrostOsc5522'):
            self.assertIn(f'pub struct {owner}', shared)
        self.assertIn('self.pending_paste_grant.take()', shared)
        self.assertIn('self.pending_paste_password = None', shared)
        self.assertIn('now() >= grant.expires_at', shared)
        self.assertIn('self.pending_clipboard_requests.len() >= MAX_PENDING_CLIPBOARD_REQUESTS', shared)
        self.assertIn('self.pending_clipboard_requests.len() < MAX_PENDING_CLIPBOARD_REQUESTS', shared)
        for app, path in [('ember', 'src/terminal/state.rs'), ('frost', 'src/terminal.rs')]:
            host = (FAMILY / app / path).read_text()
            production = host.split('\nmod tests {', 1)[0]
            self.assertNotRegex(production, r'\.osc5522\s*\.pending_(?:paste|clipboard)')
            handler = host.split('fn handle_osc_5522(', 1)[1].split('\n    }', 1)[0]
            self.assertIn('self.osc5522.admit(', handler)
            self.assertNotIn('metadata.split(', handler)
            self.assertNotIn('pending_clipboard_requests', handler)
            issue = host.split('pub fn build_paste_event(', 1)[1].split('pub fn take_clipboard_read_requests(', 1)[0]
            self.assertRegex(issue, r'self\.osc5522\s*\.issue_paste\(')
            self.assertNotIn('status=DATA', issue)
            self.assertIn('self.osc5522.take_requests()', host)
            fixture = (FAMILY / app / 'src/terminal/osc5522_oracles.rs').read_text()
            self.assertIn('fn frozen_osc5522(', fixture)
            self.assertNotIn('.admit(', fixture)
            self.assertNotIn('.issue_paste(', fixture)

    def test_frost_csi_executes_shared_transactions_before_ordered_host_effects(self):
        shared = (ENGINE / 'src/frost_csi.rs').read_text().split('#[cfg(test)]', 1)[0]
        execute = shared.split('pub fn execute<', 1)[1]
        admission = ['dispatch_frost_legacy_csi(', 'prepare_display_erase(',
                     'prepare_region_operation(', 'prepare_native_mode_command(',
                     'RepeatGraphics::prepare(', "match command"]
        offsets = [execute.index(name) for name in admission]
        self.assertEqual(offsets, sorted(offsets))
        self.assertIn('execute_transaction(host, CsiTransaction::', shared)
        self.assertIn('transaction.step(', shared)
        self.assertIn('CsiTransactionEffect::Mode(step) => FrostCsiEffect::Mode(step)', shared)
        self.assertIn('RawRowId::allocate_process_unique', shared)
        frost = (FAMILY / 'frost/src/terminal.rs').read_text()
        dispatch = frost.split('fn handle_escape_sequence(', 1)[1].split('fn decaln(', 1)[0]
        self.assertIn('jterm_engine::frost_csi::execute(', dispatch)
        self.assertNotIn('prepare_display_erase(', dispatch)
        self.assertNotIn('match cmd', dispatch)
        for old in ('apply_region_operation', 'apply_display_erase', 'apply_mode_command'):
            self.assertNotIn(f'fn {old}(', frost)
        self.assertIn('impl jterm_engine::frost_csi::FrostCsiHost for TerminalState', frost)
        fixture = (FAMILY / 'frost/src/terminal/frost_csi_oracles.rs').read_text()
        self.assertIn('fn frozen_csi_dispatch(', fixture)
        self.assertNotIn('frost_csi::execute(', fixture)

    def test_ember_csi_fallback_has_shared_transaction_execution_and_postamble(self):
        shared = (ENGINE / 'src/ember_csi.rs').read_text().split('#[cfg(test)]', 1)[0]
        execute = shared.split('pub fn execute<', 1)[1].split('fn execute_inner', 1)[0]
        self.assertLess(execute.index('execute_inner('), execute.index('EmberCsiEffect::Completed'))
        self.assertEqual(execute.count('EmberCsiEffect::Completed'), 1)
        self.assertIn('H::allocate_row_id(next)', shared)
        self.assertIn('transaction.step(', shared)
        parser = (FAMILY / 'ember/src/terminal/parser.rs').read_text()
        dispatch = parser.split('fn handle_escape_sequence(', 1)[1].split('pub(super) fn erase_screen(', 1)[0]
        self.assertIn('jterm_engine::ember_csi::execute(', dispatch)
        self.assertNotIn('prepare_display_erase(', dispatch)
        self.assertNotIn('match cmd', dispatch)
        self.assertIn('EmberCsiEffect::Completed => self.observe_chat_native_takeover()', parser)
        self.assertNotIn('fn apply_mode_command(', parser)
        self.assertNotIn('fn apply_display_erase(', parser)
        state = (FAMILY / 'ember/src/terminal/state.rs').read_text()
        self.assertNotIn('fn apply_region_operation(', state)
        fixture = (FAMILY / 'ember/src/terminal/ember_csi_oracles.rs').read_text()
        self.assertIn('fn frozen_ember_dispatch(', fixture)
        self.assertNotIn('ember_csi::execute(', fixture)

    def test_frost_c0_escape_state_mutations_have_engine_authority(self):
        shared = (ENGINE / 'src/frost_controls.rs').read_text().split('#[cfg(test)]', 1)[0]
        for operation in ('screen.move_cursor_backward_with_wrap(1, protocol)',
                          'screen.cursor_controls().tab_forward(1)',
                          'screen.save_cursor(protocol.modes().contains(&7))',
                          'screen.restore_cursor()', 'screen.begin_next_line()',
                          'screen.invoke_charset(true)', 'screen.invoke_charset(false)',
                          "screen.designate_charset(bank != b'(', charset)",
                          'protocol.modes_mut().insert(66)', 'protocol.modes_mut().remove(&66)'):
            self.assertIn(operation, shared)
        self.assertIn('Operation::CommitDesignation', shared)
        frost = (FAMILY / 'frost/src/terminal.rs').read_text()
        feed = frost.split('fn process_input_inner(', 1)[1].split('fn handle_escape_sequence(', 1)[0]
        owner = (ENGINE / 'src/vt_state.rs').read_text().split('#[cfg(test)]', 1)[0]
        self.assertIn('FrostStateCommand::control(byte)', owner)
        self.assertIn('FrostStateCommand::escape(', owner)
        self.assertIn('NativeTerminal::feed_frost(', feed)
        self.assertNotIn('ParserEvent::Control(byte) => match byte', feed)
        self.assertNotRegex(feed, r'self\.screen\.active_charset\s*=(?!=)')
        self.assertNotIn('modes_mut().insert(66)', feed)
        self.assertNotIn('fn handle_nf_escape(', frost)
        self.assertIn('command.step(&mut t.screen, &mut t.protocol)', (ENGINE / 'src/vt_state.rs').read_text())
        self.assertIn('FrostStateEffect::Vertical(motion) => self.move_vertical(motion)', frost)
        oracle = (FAMILY / 'frost/src/terminal/frost_control_oracles.rs').read_text()
        self.assertIn('fn frozen_control(', oracle)
        self.assertIn('fn frozen_escape(', oracle)

    def test_synchronized_output_state_and_policy_have_one_shared_owner(self):
        shared = (ENGINE / 'src/synchronized_output.rs').read_text().split('#[cfg(test)]', 1)[0]
        for profile in ('Ember', 'FrostLegacy', 'Hosted'):
            self.assertIn(f'SyncProfile::{profile}', shared)
        self.assertIn('age > Duration::from_secs(1)', shared)
        self.assertIn('age >= Duration::from_secs(1)', shared)
        self.assertIn('Duration::from_millis(250)', shared)
        self.assertIn('self.started.get_or_insert_with(now)', shared)
        self.assertIn('self.started = Some(now())', shared)
        end = shared.split('impl NativeSyncEnd', 1)[1]
        self.assertLess(end.index('SyncEndEffect::Archive'), end.index('state.active = false'))
        self.assertLess(end.index('state.active = false'), end.index('SyncEndEffect::Dirty'))
        for app, path in [('ember', 'src/terminal/state.rs'), ('frost', 'src/terminal.rs')]:
            host = (FAMILY / app / path).read_text()
            timeout = host.split('pub fn check_sync_output_timeout(', 1)[1].split('fn apply_sync_end(', 1)[0]
            self.assertNotIn('Duration::from_secs(1)', timeout)
            self.assertIn('self.vt.sync_output.timeout', timeout)
            self.assertIn('command.step(&mut self.vt.sync_output, &mut self.vt.protocol)', host)
            self.assertIn('SyncEndEffect::Archive', host)
            self.assertIn('SyncEndEffect::Dirty', host)
        ember = (FAMILY / 'ember/src/terminal/mod.rs').read_text()
        frost = (FAMILY / 'frost/src/terminal.rs').read_text()
        for host in (ember, frost):
            self.assertIn('pub vt: jterm_engine::native_terminal::NativeTerminal', host)
            self.assertNotIn('pub sync_output:', host)
            self.assertNotIn('pub sync_output_active: bool', host)
            self.assertNotIn('sync_output_start: Option<', host)
        hosted = (ENGINE / 'src/terminal.rs').read_text()
        self.assertNotIn('sync_started:', hosted)
        for method in ('begin_hosted', 'end_hosted', 'reflect_query', 'finish_query'):
            self.assertRegex(hosted, rf'\bself\s*\.vt\s*\.sync_output\s*\.{method}\(')

    def test_native_terminal_is_the_only_active_vt_state_and_event_executor(self):
        owner = (ENGINE / 'src/vt_state.rs').read_text().split('#[cfg(test)]', 1)[0]
        state = owner.split('pub struct VtState {', 1)[1].split('\n}', 1)[0]
        for field, typ in [('screen', 'ScreenState'), ('protocol', 'ProtocolState'), ('parser', 'Parser'), ('sync_output', 'SynchronizedOutput')]:
            self.assertIn(f'pub {field}: {typ}', state)
        self.assertIn('fn dispatch_ember<', owner)
        self.assertIn('fn dispatch_frost<', owner)
        self.assertIn('crate::ember_csi::execute(', owner)
        self.assertIn('crate::frost_csi::execute(', owner)
        self.assertIn('Self::frost_control(', owner)
        self.assertIn('Self::write_ember(', owner)
        self.assertIn('Self::write_frost(', owner)
        self.assertNotIn('Deref', owner)
        for app, path in [('ember', 'src/terminal/mod.rs'), ('frost', 'src/terminal.rs')]:
            host = (FAMILY / app / path).read_text().split('pub struct TerminalState {', 1)[1].split('\n}', 1)[0]
            for field in ('screen', 'protocol', 'parser', 'sync_output'):
                self.assertNotRegex(host, rf'(?m)^\s*(?:pub )?{field}:')

    def test_hosted_and_native_use_identical_vt_storage_and_feed_entrypoint(self):
        owner = (ENGINE / 'src/vt_state.rs').read_text().split('#[cfg(test)]', 1)[0]
        self.assertIn('pub struct VtState {', owner)
        for field, typ in [('screen', 'ScreenState'), ('protocol', 'ProtocolState'), ('parser', 'Parser'), ('sync_output', 'SynchronizedOutput')]:
            self.assertIn(f'pub {field}: {typ}', owner)
        shim = (ENGINE / 'src/native_terminal.rs').read_text()
        self.assertIn('VtState as NativeTerminal', shim)
        self.assertNotIn('struct NativeTerminal', shim)
        hosted = (ENGINE / 'src/terminal.rs').read_text()
        body = hosted.split('pub struct Terminal {', 1)[1].split('\n}', 1)[0]
        self.assertIn('vt: crate::vt_state::VtState,', body)
        for field in ('screen', 'protocol', 'parser', 'sync_output'):
            self.assertNotRegex(body, rf'(?m)^\s*{field}:')
        self.assertIn('VtState::adopt_hosted(', hosted)
        self.assertIn('VtState::drive(', hosted)
        self.assertEqual(owner.count('drive_feed('), 1)
        self.assertIn('Self::drive(', owner)
        self.assertIn('VtProfile::Hosted => FeedTail::InputConsumed', owner)
        self.assertIn('VtProfile::Native(NativeProfile::FrostLegacy)', owner)

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
    parser.add_argument('--aether-checkout', type=Path, default=FAMILY / 'aether',
                        help='use an isolated Aether worktree for consumer checks')
    args = parser.parse_args()
    BoundaryTests.aether_checkout = args.aether_checkout.resolve()
    if args.engine_only:
        suite = unittest.TestSuite(BoundaryTests(name) for name in (
            'test_dependency_direction', 'test_storage_vectors_remain_private'))
    else:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(BoundaryTests)
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
