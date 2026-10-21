# Standard primary-pointer gestures only. Custom mappings, Select-mode and
# virtual columns remain owned by the compatibility input path.
def NativePointerAvailable(): bool
  const current = mode(1)
  if index(['n', 'v'], current) < 0 || !empty(&virtualedit)
      || &selectmode =~# 'mouse' || &mouse !~# (current ==# 'n' ? '[an]' : '[av]')
    return false
  endif
  for mappingMode in ['n', 'x']
    for key in ['<LeftMouse>', '<LeftDrag>', '<LeftRelease>']
      if !empty(maparg(key, mappingMode))
        return false
      endif
    endfor
  endfor
  return true
enddef

def NativeSourceWindow(request: dict<any>): number
  const window = PositiveNumber(request, 'window')
  const buffer = PositiveNumber(request, 'buffer')
  const tick: any = get(request, 'changedtick', -1)
  const info = getwininfo(window)
  if win_id2win(window) == 0 || empty(info) || info[0].bufnr != buffer
      || type(tick) != v:t_number || tick < 0 || getbufvar(buffer, 'changedtick') != tick
      || getbufvar(buffer, '&buftype') !=# ''
    throw 'The native source window or buffer generation changed'
  endif
  return window
enddef

def ScrollSourceWindow(request: dict<any>, id: number)
  SafeToChange()
  if mode(1) !=# 'n' || SaveInputPending()
    throw 'Precision scrolling requires idle Normal mode'
  endif
  const window = NativeSourceWindow(request)
  const info = getwininfo(window)[0]
  const top = PositiveNumber(request, 'topline')
  const left: any = get(request, 'leftcol', -1)
  if info.topline != top || type(left) != v:t_number || left < 0
      || get(getwininfo(window)[0], 'leftcol', 0) != left
    # getwininfo has no leftcol on older Vim; the window-local check below is
    # authoritative for every supported version.
    if info.topline != top
      throw 'The native viewport changed before scrolling'
    endif
  endif
  var view: dict<any> = {}
  win_execute(window, 'g:jterm_native_view = winsaveview()')
  view = g:jterm_native_view
  unlet g:jterm_native_view
  if type(left) != v:t_number || left < 0 || view.leftcol != left
    throw 'The horizontal native viewport changed before scrolling'
  endif
  ScrollWindow(request)
  scrollAcknowledgements = filter(scrollAcknowledgements, (key, _) => win_id2win(str2nr(key)) > 0)
  scrollAcknowledgements[string(window)] = id
  # Publish the acknowledged semantic view before the response. The host does
  # not have to infer completion from cursor-only or unrelated render frames.
  EmitRender()
enddef

def SourcePointer(request: dict<any>)
  const previous = pointerGesture
  pointerGesture = {}
  SafeToChange()
  if !NativePointerAvailable() || SaveInputPending()
    throw 'This input state or mouse mapping uses Vim compatibility input'
  endif
  const window = NativeSourceWindow(request)
  const gesture = PositiveNumber(request, 'gesture')
  const phase = RequiredString(request, 'phase', 16)
  if index(['press', 'drag', 'release'], phase) < 0
    throw 'Unknown native pointer phase'
  endif
  const lineNumber = PositiveNumber(request, 'line')
  const byte: any = get(request, 'byte_column', -1)
  const textRows = getbufline(request.buffer, lineNumber)
  if empty(textRows) || type(byte) != v:t_number || byte < 0
      || byte > strlen(textRows[0]) || byte > 16384
      || (byte < strlen(textRows[0]) && byteidxcomp(textRows[0], charidx(textRows[0], byte, true)) != byte)
    throw 'Native pointer position is not a valid source byte boundary'
  endif
  if phase !=# 'press'
    if empty(previous) || previous.gesture != gesture || previous.window != window
        || win_getid() != window || previous.buffer != bufnr() || previous.tick != b:changedtick
        || previous.mode !=# mode(1) || previous.cursor !=# getcurpos()[1 : 2]
      throw 'The native pointer gesture was interrupted; it will not be replayed'
    endif
  else
    if !win_gotoid(window)
      throw 'The requested native pointer window is unavailable'
    endif
    # WinEnter/BufEnter hooks are allowed to run, but cannot redirect the click
    # to a different buffer or invalidate its captured source generation.
    NativeSourceWindow(request)
    if win_getid() != window || !NativePointerAvailable() || !empty(RenderFallback())
      throw 'The target window no longer supports this native pointer gesture'
    endif
    if mode(1) ==# 'v'
      execute "normal! \<Esc>"
    endif
  endif
  if phase !=# 'press' && mode(1) ==# 'n'
      && (lineNumber != line('.') || byte + 1 != col('.'))
    normal! v
  endif
  cursor(lineNumber, byte + 1)
  if phase !=# 'release'
    pointerGesture = {gesture: gesture, window: window, buffer: bufnr(),
      tick: b:changedtick, mode: mode(1), cursor: getcurpos()[1 : 2]}
  endif
  Changed()
  EmitRender()
enddef
