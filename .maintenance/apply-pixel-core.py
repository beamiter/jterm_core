from pathlib import Path


def replace(path, old, new):
    p = Path(path)
    text = p.read_text()
    if text.count(old) != 1:
        raise SystemExit(f'{path}: expected one patch site: {old[:100]!r}, found {text.count(old)}')
    p.write_text(text.replace(old, new))


p = 'src/vim9.rs'
replace(p, 'const PROTOCOL: u64 = 1;', '''#[path = "vim9_pixel_scroll.rs"]
mod pixel_scroll;
pub use pixel_scroll::Vim9PixelScroll;

/// Source-bound primary-pointer phases. No arbitrary Vim keys or Ex commands.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Vim9PointerPhase { Press, Drag, Release }

const PROTOCOL: u64 = 1;''')
replace(p, '    SetRender {\n        enabled: bool,\n    },', '''    SetRender {
        enabled: bool,
    },
    /// A precision-scroll transaction bound to the sampled source and view.
    ScrollView {
        window: i64, buffer: i64, changedtick: u64,
        topline: u64, leftcol: usize, lines: i32,
    },
    /// A standard primary click/character drag using captured source bytes.
    Pointer {
        window: i64, buffer: i64, changedtick: u64,
        line: u64, byte_column: usize, gesture: u64, phase: Vim9PointerPhase,
    },''')
replace(p, '        match self {\n            Self::Scroll { window, lines }', '''        match self {
            Self::ScrollView { window, buffer, changedtick, topline, leftcol, lines }
                if *window <= 0 || *buffer <= 0 || *changedtick > i64::MAX as u64
                    || *topline == 0 || *topline > i32::MAX as u64
                    || *leftcol > i32::MAX as usize || *lines == 0 || lines.unsigned_abs() > 120 => {
                return Err(invalid("Invalid source-bound precision-scroll transaction"));
            }
            Self::Pointer { window, buffer, changedtick, line, byte_column, gesture, .. }
                if *window <= 0 || *buffer <= 0 || *changedtick > i64::MAX as u64
                    || *line == 0 || *line > i32::MAX as u64 || *byte_column > 16384
                    || *gesture == 0 || *gesture > i32::MAX as u64 => {
                return Err(invalid("Invalid source-bound pointer transaction"));
            }
            Self::Scroll { window, lines }''')
p = 'src/vim9_render.rs'
replace(p, '    pub overscan: Vec<Vim9RenderMarginRow>,', '''    pub overscan: Vec<Vim9RenderMarginRow>,
    /// Optional protocol-3 native input capability, false for legacy producers.
    pub native_pointer: bool,
    /// Last successfully applied source-bound scroll request in this split.
    pub scroll_ack: u64,
    /// Explicit visible source-line lengths, including empty and EOL targets.
    pub line_lengths: Vec<usize>,''')
replace(p, '                    && window.textoff <= window.view.columns', '''                    && window.textoff <= window.view.columns
                    && window.scroll_ack <= i64::MAX as u64
                    && window.line_lengths.len() <= window.view.rows
                    && window.line_lengths.iter().all(|length| *length <= 16384)
                    && (window.line_lengths.is_empty() || window.line_lengths.len() as u64
                        == window.botline.saturating_sub(window.topline) + 1)
                    && (!window.native_pointer || !window.line_lengths.is_empty())''')
p = 'runtime/vim9/bridge.vim'
replace(p, 'var renderLineGeneration = \'\'', '''var scrollAcknowledgements: dict<number> = {}
var pointerGesture: dict<any> = {}
var renderLineGeneration = '' ''')
# No globals are used as a temporary window-state mailbox.
runtime = Path('.maintenance/pixel-runtime.vim').read_text()
a = runtime.index('  if info.topline != top || type(left)')
b = runtime.index('  ScrollWindow(request)', a)
runtime = runtime[:a] + '''  if info.topline != top
    throw 'The native viewport changed before scrolling'
  endif
  nativeScrollLeft = -1
  win_execute(window, 'call ' .. expand('<SID>') .. 'SampleNativeScrollLeft()')
  if type(left) != v:t_number || left < 0 || nativeScrollLeft != left
    throw 'The horizontal native viewport changed before scrolling'
  endif
''' + runtime[b:]
runtime = '''var nativeScrollLeft = -1

def SampleNativeScrollLeft()
  nativeScrollLeft = winsaveview().leftcol
enddef

''' + runtime
replace(p, 'def RenderWindowFrame(margin: number = 0): dict<any>', runtime + '\ndef RenderWindowFrame(margin: number = 0): dict<any>')
replace(p, '  frame.overscan = []\n', '''  frame.overscan = []
  frame.native_pointer = renderProtocol >= 3 && NativePointerAvailable()
  frame.scroll_ack = get(scrollAcknowledgements, string(win_getid()), 0)
  frame.line_lengths = map(getline(first, last), (_, text) => strlen(text))
''')
replace(p, '      line_count: sample.line_count, overscan: sample.overscan})', '''      line_count: sample.line_count, overscan: sample.overscan,
      native_pointer: sample.native_pointer, scroll_ack: sample.scroll_ack,
      line_lengths: sample.line_lengths})''')
replace(p, "  remove(frame, 'overscan')", "  remove(frame, 'overscan')\n  remove(frame, 'line_lengths')\n  remove(frame, 'native_pointer')\n  remove(frame, 'scroll_ack')")
replace(p, "  elseif action ==# 'scroll'\n    ScrollWindow(request)", "  elseif action ==# 'scroll'\n    ScrollWindow(request)\n  elseif action ==# 'scroll_view'\n    ScrollSourceWindow(request, id)\n  elseif action ==# 'pointer'\n    SourcePointer(request)")
# Advertise the exact typed actions, independently of optional UI chrome.
replace(p, "    'snapshot', 'set_render', 'render_resync',", "    'snapshot', 'set_render', 'render_resync', 'scroll_view', 'pointer',")
# Correct a staging-only function spelling before Rust compilation.
replace('src/vim9_pixel_scroll.rs', '    fn_a_clamped_vim_viewport_does_not_leave_scroll_debt()', '    fn a_clamped_vim_viewport_does_not_leave_scroll_debt()')
print('Applied source-bound precision scroll and pointer transactions')
