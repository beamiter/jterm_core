vim9script
# Real Vim owns editing, undo, mappings, plugins, buffers and windows.  This
# optional runtime publishes metadata and GUI buffer paint data, and accepts
# a small, explicit action set. It never reconstructs text from PTY output.
# Protocol and installation details are documented beside this file.

if exists('g:jterm_vim9_bridge_loaded')
  finish
endif
if v:version < 900 || !has('vim9script') || !has('channel')
    || !has('job') || !has('timers') || !has('quickfix')
  throw 'JTerm IDE requires Vim 9 with +vim9script +channel +job +timers +quickfix'
endif

var address = $JTERM_VIM9_ADDR
var authToken = $JTERM_VIM9_TOKEN
var projectRoot = empty($JTERM_VIM9_ROOT) ? getcwd() : $JTERM_VIM9_ROOT
# Build tools and language servers started after startup must not inherit the
# bridge credential.  The remaining script-local copy is never serialized.
unlet $JTERM_VIM9_TOKEN
if empty(address) || empty(authToken)
  finish
endif
g:jterm_vim9_bridge_loaded = true
g:jterm_vim9_connected = false
g:jterm_vim9_render_protocol = 0

var bridge: channel
var authenticated = false
var workbenchConnected = false
var workbenchActionDepth = 0
var shuttingDown = false
var sending = false
var sequence = 0
var sendBatchDepth = 0
var sendBatch = ''
var pendingQuit = 0
var flushTimer = -1
var pollTimer = -1
var previousState = ''
var diagnosticsKey = ''
var diagnosticsCache: dict<any> = {items: [], total: 0}
var lspDiagnosticsTick = 0
var bufferViews: dict<dict<any>> = {}
var buildJob: job
var buildGeneration = 0
var buildRunning = false
var buildClosed = false
var buildExited = false
var buildCancelled = false
var buildCommand = ''
var buildExit: any = v:null
var buildLines: list<string> = []
var buildTail: list<string> = []
var buildDropped = 0
var buildErrorFormat = ''
var searchJob: job
var searchGeneration = 0
var searchRunning = false
var searchClosed = false
var searchExited = false
var searchQuery = ''
var searchRoot = ''
var searchExit = 0
var searchRows: list<dict<any>> = []
var searchDropped = 0
var searchErrors: list<string> = []
var searchCancelled = false
var searchOptions: dict<any> = {case: 'smart', regex: false, whole_word: false, glob: ''}
var deferredSave: dict<any> = {}
var deferredSaveTimer = -1
var deferredSaveBuffer = 0
var checkingSaveInput = false
var renderEnabled = false
var renderTimer = -1
var renderRevision = 0
var previousRender = ''
var renderProtocol = 1
var previousRenderLayout = ''
var previousRenderRows: dict<string> = {}
var renderKeyframeRevision = 0
var renderBaseKey = ''
var renderBase: list<dict<any>> = []
var renderStyles: dict<dict<any>> = {}
var renderThemeKey = ''
var renderThemeGeneration = 0
var renderNormal: dict<any> = {}
var cursorOption: any = v:null
var cursorShapes: dict<dict<any>> = {}

# Keep the same per-mode reset and ordered 'a:' overlay semantics as Vim's
# parse_shape_opt(). Sampling does not change the user's guicursor option.
def ParseCursorOption(value: string)
  var exclusive = false
  for part in split(value, ',')
    var halves = split(part, ':', true)
    if len(halves) != 2
      continue
    endif
    var modes = split(tolower(halves[0]), '-')
    exclusive = exclusive || index(modes, 've') >= 0
    for name in keys(cursorShapes)
      if index(modes, name) < 0 && index(modes, 'a') < 0
        continue
      endif
      var shape = cursorShapes[name]
      if index(modes, name) >= 0
        shape.shape = 'block'
        shape.percentage = 100
        shape.blinkwait = 700
        shape.blinkon = 400
        shape.blinkoff = 250
      endif
      for argument in split(halves[1], '-')
        var token = tolower(argument)
        if token ==# 'block'
          shape.shape = 'block'
          shape.percentage = 100
        elseif token =~# '^\(ver\|hor\)\d\+$'
          shape.shape = token[0 : 2] ==# 'ver' ? 'vertical' : 'horizontal'
          shape.percentage = min([100, max([1, str2nr(strpart(token, 3))])])
        elseif token =~# '^blink\(wait\|on\|off\)\d\+$'
          var field = matchstr(token, '^blink\a\+')
          shape[field] = min([2147483647, str2nr(strpart(token, strlen(field)))])
        elseif !empty(argument)
          shape.group = argument
        endif
      endfor
    endfor
  endfor
  if !exclusive
    cursorShapes.ve = copy(cursorShapes.v)
  endif
enddef

def RenderCursorAppearance(): dict<any>
  var option = exists('+guicursor') ? &guicursor : ''
  if type(cursorOption) != v:t_string || cursorOption !=# option
    if empty(cursorShapes)
      for name in ['n', 'v', 've', 'o', 'i', 'r', 'c', 'ci', 'cr', 'sm']
        cursorShapes[name] = {shape: 'block', percentage: 100, group: '',
          blinkwait: 700, blinkon: 400, blinkoff: 250}
      endfor
      ParseCursorOption(has('gui')
        ? 'n-v-c:block-Cursor/lCursor,ve:ver35-Cursor,o:hor50-Cursor,i-ci:ver25-Cursor/lCursor,r-cr:hor20-Cursor/lCursor,sm:block-Cursor-blinkwait175-blinkoff150-blinkon175'
        : 'n-v-c:block,o:hor50,i-ci:hor15,r-cr:hor30,sm:block')
    endif
    ParseCursorOption(option)
    cursorOption = option
  endif
  var current = mode(1)
  var name = current =~# '^no' ? 'o' : (current =~# '^i' ? 'i' : (current =~# '^R' ? 'r' : 'n'))
  if index(['v', 'V', "\<C-V>"], current) >= 0
    name = &selection ==# 'exclusive' ? 've' : 'v'
  endif
  var result = copy(cursorShapes[name])
  var groups = split(remove(result, 'group'), '/', true)
  var mapped = (name ==# 'i' || name ==# 'r') && &iminsert == 1
  var group = get(groups, mapped && len(groups) > 1 ? 1 : 0, '')
  var id = hlID(group)
  result.style = copy(RenderStyle(id))
  if id == 0
    result.style.reverse = true
  endif
  return result
enddef
# Each real Vim window has its own viewport and syntax context, even when
# several windows display the same buffer. Keep the expensive base paint
# cache per window; cursor/selection overlays are composed on every sample.
var renderWindowCaches: dict<dict<any>> = {}
var renderWindowSample: dict<any> = {}
var renderSamplingWindow = 0
var scrollAcknowledgements: dict<number> = {}
var pointerGesture: dict<any> = {}
var renderLineGeneration = ''
var renderLineCache: dict<dict<any>> = {}

def Send(payload: dict<any>)
  if ch_status(bridge) !=# 'open'
    return
  endif
  sequence += 1
  payload.protocol = 1
  payload.seq = sequence
  var encoded = json_encode(payload) .. "\n"
  if sendBatchDepth > 0
    sendBatch ..= encoded
  else
    ch_sendraw(bridge, encoded)
  endif
enddef

def FlushSendBatch()
  var encoded = sendBatch
  sendBatch = ''
  if !empty(encoded) && ch_status(bridge) ==# 'open'
    ch_sendraw(bridge, encoded)
  endif
enddef

def Reply(id: number, ok: bool, error: string = '')
  Send({event: 'response', id: id, ok: ok, error: strcharpart(error, 0, 2048)})
enddef

# This is an adapter for the user's already-loaded vimrc, not an Ex/RPC
# extension point. Both directions have a closed vocabulary and bounded data.
const WORKBENCH_ACTIONS = ['simple_files', 'simple_buffers', 'simple_grep',
  'simple_outline', 'simple_rename', 'simple_code_action', 'simple_git_status',
  'simple_git_diff', 'simple_git_hunk', 'simple_markdown_preview', 'simple_keys',
  'simple_workbench', 'simple_diagnostics', 'simple_next_diagnostic',
  'simple_previous_diagnostic', 'simple_recent', 'simple_tree_toggle',
  'simple_tree_reveal', 'simple_terminal_toggle', 'simple_terminal_new',
  'simple_terminal_next', 'simple_terminal_previous']
const WORKBENCH_SEMANTICS = ['definition', 'hover', 'references', 'format']

def WorkbenchCapabilities(): list<string>
  var result: list<string> = []
  if !exists('*g:VimrcWorkbenchCapabilities') || !exists('*g:VimrcWorkbenchAction')
    return result
  endif
  try
    var provided: any = call('g:VimrcWorkbenchCapabilities', [])
    if type(provided) != v:t_list
      return result
    endif
    for action in provided[: 63]
      if type(action) == v:t_string
          && (index(WORKBENCH_ACTIONS, action) >= 0 || index(WORKBENCH_SEMANTICS, action) >= 0)
          && index(result, action) < 0
        add(result, action)
      endif
    endfor
  catch
    # Optional user hooks may be reloading or their plugins may be absent.
  endtry
  return result
enddef

def WorkbenchText(value: dict<any>, field: string, limit: number): string
  var item: any = get(value, field, '')
  return type(item) == v:t_string
    ? substitute(strcharpart(item, 0, limit), '[[:cntrl:]]', '', 'g') : ''
enddef

def WorkbenchCount(value: dict<any>, field: string): number
  var item: any = get(value, field, 0)
  return type(item) == v:t_number ? min([1000000, max([0, item])]) : 0
enddef

def WorkbenchBool(value: dict<any>, field: string): bool
  var item: any = get(value, field, false)
  return type(item) == v:t_bool ? item : false
enddef

def WorkbenchDict(value: dict<any>, field: string): dict<any>
  var item: any = get(value, field, {})
  return type(item) == v:t_dict ? item : {}
enddef

def WorkbenchState(): dict<any>
  if !exists('*g:VimrcWorkbenchState')
    return {}
  endif
  try
    var source = [bufnr(), win_getid(), b:changedtick]
    var provided: any = call('g:VimrcWorkbenchState', [])
    if type(provided) != v:t_dict || empty(provided)
        || source != [bufnr(), win_getid(), b:changedtick]
      return {}
    endif
    var git = WorkbenchDict(provided, 'git')
    var diagnostics = WorkbenchDict(provided, 'diagnostics')
    var remote = WorkbenchDict(provided, 'remote')
    # Never serialize the returned dictionary itself. A provider could carry
    # unrelated secrets, functions, channels or cyclic plugin state.
    return {buffer: bufnr(), window: win_getid(), changedtick: b:changedtick,
      provider: WorkbenchText(provided, 'provider', 64),
      root: WorkbenchText(provided, 'root', 4096),
      native: WorkbenchBool(provided, 'native'),
      large_file: WorkbenchBool(provided, 'large_file'),
      git: {head: WorkbenchText(git, 'head', 128),
        ahead: WorkbenchCount(git, 'ahead'), behind: WorkbenchCount(git, 'behind'),
        added: WorkbenchCount(git, 'added'), changed: WorkbenchCount(git, 'changed'),
        removed: WorkbenchCount(git, 'removed')},
      diagnostics: {error: WorkbenchCount(diagnostics, 'error'),
        warning: WorkbenchCount(diagnostics, 'warning'), info: WorkbenchCount(diagnostics, 'info'),
        hint: WorkbenchCount(diagnostics, 'hint')},
      remote: {connected: WorkbenchBool(remote, 'connected'),
        label: WorkbenchText(remote, 'label', 256), root: WorkbenchText(remote, 'root', 4096)}}
  catch
    return {}
  endtry
enddef

def WorkbenchLifecycle(connected: bool)
  if workbenchConnected == connected
    return
  endif
  workbenchConnected = connected
  g:jterm_vim9_connected = connected
  g:jterm_vim9_render_protocol = connected ? renderProtocol : 0
  try
    if connected
      doautocmd <nomodeline> User JTermVim9Ready
    else
      doautocmd <nomodeline> User JTermVim9Disconnected
    endif
  catch
    # A user autocommand must not break authenticated channel processing.
  endtry
enddef

def Capabilities(): list<string>
  var result = ['open', 'save', 'save_all', 'buffer', 'build', 'cancel_build',
    'goto', 'close_buffer', 'close_current_buffer', 'next_buffer', 'previous_buffer', 'split', 'save_and_build',
    'navigate_back', 'navigate_forward', 'cancel_search',
    'snapshot', 'set_render', 'render_resync', 'scroll_view', 'pointer', 'next_diagnostic', 'previous_diagnostic', 'quit']
  if executable('rg')
    add(result, 'search')
    add(result, 'search_with_options')
  endif
  if exists(':LspGotoDefinition') == 2 || !empty(tagfiles()) || !empty(&tagfunc)
    add(result, 'definition')
  endif
  for [action, command] in items({hover: 'LspHover', references: 'LspShowReferences',
      rename: 'LspRename', format: 'LspFormat'})
    if exists(':' .. command) == 2
      add(result, action)
    endif
  endfor
  return uniq(sort(result + WorkbenchCapabilities()))
enddef

# Native buffer rendering uses Vim's buffer bytes, syntax engine and GUI
# highlight attributes. No text or color is recovered from the PTY screen.
# Unsupported display features publish an explicit fallback instead of lying
# about Vim's visible editing state. The first renderer deliberately keeps
# Vim's exact cell geometry so existing mouse/IME input remains authoritative.
def RenderColor(id: number, part: string): any
  var value = tolower(synIDattr(id, part .. '#', 'gui'))
  if value ==# 'fg' || value ==# 'foreground'
    return get(renderNormal, 'foreground', v:null)
  elseif value ==# 'bg' || value ==# 'background'
    return get(renderNormal, 'background', v:null)
  endif
  if value !~# '^#[0-9a-f]\{6}$'
    value = get(v:colornames, value, '')
  endif
  return value =~# '^#[0-9a-fA-F]\{6}$' ? str2nr(strpart(value, 1), 16) : v:null
enddef

def RenderStyle(id: number): dict<any>
  var resolved = synIDtrans(id)
  var key = string(resolved)
  if has_key(renderStyles, key)
    return renderStyles[key]
  endif
  var style: dict<any> = {foreground: RenderColor(resolved, 'fg'),
    background: RenderColor(resolved, 'bg'), special: RenderColor(resolved, 'sp')}
  for attribute in ['bold', 'italic', 'underline', 'undercurl', 'strikethrough', 'reverse']
    style[attribute] = synIDattr(resolved, attribute, 'gui') ==# '1'
  endfor
  renderStyles[key] = style
  return style
enddef

def MergeRenderStyle(base: dict<any>, overlay: dict<any>): dict<any>
  var result = copy(base)
  for name in ['foreground', 'background', 'special']
    if type(get(overlay, name, v:null)) == v:t_number
      result[name] = overlay[name]
    endif
  endfor
  for name in ['bold', 'italic', 'underline', 'undercurl', 'strikethrough', 'reverse']
    result[name] = get(base, name, false) || get(overlay, name, false)
  endfor
  return result
enddef

def RefreshRenderTheme()
  if !renderEnabled
    return
  endif
  # :highlight may run without ColorScheme. The low-rate reconciliation and
  # command-mode transitions also catch those changes, including linked groups.
  var key = string([&background, hlget(), v:colornames])
  if key !=# renderThemeKey
    renderThemeKey = key
    renderThemeGeneration += 1
    renderBaseKey = ''
    renderStyles = {}
    renderNormal = {}
    renderNormal = copy(RenderStyle(hlID('Normal')))
    if type(renderNormal.foreground) != v:t_number
      renderNormal.foreground = &background ==# 'light' ? 0x24292f : 0xd8dee9
    endif
    if type(renderNormal.background) != v:t_number
      renderNormal.background = &background ==# 'light' ? 0xffffff : 0x20242c
    endif
  endif
enddef

def NativeTextBuffer(type: string): bool
  # These buffers still own ordinary Vim text, byte positions and undo. Their
  # write/readonly/autocommand semantics remain entirely Vim's responsibility.
  return index(['', 'nofile', 'nowrite', 'acwrite'], type) >= 0
enddef

def RenderableTextBuffer(type: string): bool
  # Help and quickfix own special Enter/mouse semantics. They may publish
  # source paint, but never enter the semantic pointer/scroll input path.
  return NativeTextBuffer(type) || type ==# 'help' || type ==# 'quickfix'
enddef

def ListCharGlyphs(value: string): list<string>
  var glyphs: list<string> = []
  var offset = 0
  while offset < strlen(value)
    var tail = strpart(value, offset)
    var glyph = strcharpart(tail, 0, 1)
    var bytes = strlen(glyph)
    if glyph ==# '\'
      var kind = strpart(tail, 1, 1)
      var digits = kind ==# 'x' ? 2 : (kind ==# 'u' ? 4 : (kind ==# 'U' ? 8 : 0))
      var hex = strpart(tail, 2, digits)
      if digits == 0 || strlen(hex) != digits || hex !~# '^\x\+$'
        throw 'This listchars escape uses the compatibility view'
      endif
      var codepoint = str2nr(hex, 16)
      if codepoint > 0x10ffff || (codepoint >= 0xd800 && codepoint <= 0xdfff)
        throw 'This listchars codepoint uses the compatibility view'
      endif
      glyph = nr2char(codepoint)
      bytes = digits + 2
    endif
    if strdisplaywidth(glyph) != 1 || glyph =~# '[\x00-\x1f\x7f]'
      throw 'Native listchars requires single-cell printable glyphs'
    endif
    add(glyphs, glyph)
    offset += bytes
  endwhile
  return glyphs
enddef

def RenderListChars(): dict<list<string>>
  var result: dict<list<string>> = {}
  if !&list
    return result
  endif
  for entry in split(&listchars, ',')
    var colon = stridx(entry, ':')
    var name = strpart(entry, 0, colon)
    if colon < 0 || index(['tab', 'space', 'lead', 'trail', 'nbsp', 'eol', 'extends', 'precedes', 'conceal'], name) < 0
      throw 'This listchars field uses the compatibility view'
    endif
    var glyphs = ListCharGlyphs(strpart(entry, colon + 1))
    if (name ==# 'tab' && len(glyphs) != 2 && len(glyphs) != 3)
        || (name !=# 'tab' && len(glyphs) != 1)
      throw 'This listchars glyph sequence uses the compatibility view'
    endif
    result[name] = glyphs
  endfor
  return result
enddef

def QuickfixCurrentLine(info: dict<any>): number
  if &buftype !=# 'quickfix'
    return 0
  endif
  if !get(info, 'quickfix', 0)
    return -1
  endif
  var list = get(info, 'loclist', 0)
    ? getloclist(0, {idx: 0, qfbufnr: 0}) : getqflist({idx: 0, qfbufnr: 0})
  # A plugin may construct a lookalike special buffer or retire a list. Never
  # decorate it with another list's current entry.
  if get(list, 'qfbufnr', 0) != bufnr()
    return -1
  endif
  return get(list, 'idx', 0)
enddef

def WindowNormalGroup(): dict<any>
  if !exists('+winhighlight')
    return {supported: true, name: &wincolor}
  endif
  # New Vim versions implement wincolor using a Normal-only winhighlight
  # mapping. Read the effective mapping: wincolor itself can be stale after
  # :set winhighlight=. General highlight remapping needs a separate adapter.
  var mapping = getwinvar(0, '&winhighlight')
  if empty(mapping)
    return {supported: true, name: ''}
  endif
  var pair = split(mapping, ':', true)
  if len(pair) != 2 || index(['!(', 'Normal'], pair[0]) < 0
      || empty(pair[1]) || pair[1] =~# '[!,]'
    return {supported: false, name: ''}
  endif
  return {supported: true, name: pair[1]}
enddef

def WindowNormalStyle(): dict<any>
  var group = WindowNormalGroup()
  if empty(group.name)
    return renderNormal
  endif
  var id = synIDtrans(hlID(group.name))
  var style = RenderStyle(id)
  if style == RenderStyle(0) && synIDattr(id, 'nocombine', 'gui') !=# '1'
    return renderNormal
  endif
  # wincolor replaces Normal's attributes, while unspecified colors retain
  # the global Normal colors. Never rebind RenderColor's fg/bg references or
  # the globally cached raw syntax styles to the last sampled window.
  var result = copy(style)
  for color in ['foreground', 'background', 'special']
    if type(result[color]) != v:t_number
      result[color] = renderNormal[color]
    endif
  endfor
  return result
enddef

def VisiblePopups(): list<number>
  return filter(popup_list(), (_, id) => get(popup_getpos(id), 'visible', 0) != 0)
enddef

def RenderFallback(): string
  if !RenderableTextBuffer(&buftype)
    return 'This special Vim buffer uses the compatibility view'
  elseif mode(1) !~# '^[nivVR\x16]'
    return 'Vim command, prompt or terminal input uses the compatibility view'
  elseif (pumvisible() && renderProtocol < 5) || (renderProtocol < 6 && !empty(VisiblePopups()))
    return 'Vim completion and popup windows use the compatibility view'
  elseif &diff || &rightleft || &arabic || &spell || &conceallevel > 0
    return 'This Vim display option needs the compatibility view'
  elseif !WindowNormalGroup().supported
    return 'General Vim window highlight remapping uses the compatibility view'
  elseif get(winsaveview(), 'coladd', 0) > 0
      || (index(['v', 'V', "\<C-V>"], mode(1)) >= 0 && getpos('v')[3] > 0)
    return 'Vim virtual-space cursor and selections use the compatibility view'
  elseif &foldcolumn != 0
    return 'Vim fold columns use the compatibility view'
  elseif &hlsearch && v:hlsearch && @/ =~# '\\[_%zn]'
    return 'Multiline and positional Vim searches use the compatibility view'
  endif
  return RenderMatchFallback(getmatches())
enddef

def RenderMatchFallback(matches: list<dict<any>>): string
  if len(matches) > 64
    return 'Vim matches exceed the bounded native decoration budget'
  endif
  var positions = 0
  for decoration in matches
    if has_key(decoration, 'conceal') || synIDtrans(hlID(decoration.group)) == hlID('Conceal')
      return 'Vim conceal matches use the compatibility view'
    endif
    if has_key(decoration, 'pattern')
      # String matching has no buffer/mark/cursor context. Be conservative
      # about multiline atoms, position atoms and changed match boundaries.
      # In particular, do not interpret \%l relative to this sampled string.
      if strlen(decoration.pattern) > 4096 || decoration.pattern =~# '\\[_%zn]' || decoration.pattern =~# "[\r\n]"
        return 'Multiline and positional Vim match patterns use the compatibility view'
      endif
    else
      positions += len(filter(keys(decoration), (_, key) => key =~# '^pos\d\+$'))
      if positions > 2048
        return 'Vim match positions exceed the bounded native decoration budget'
      endif
    endif
  endfor
  return ''
enddef

# getmatches() is in ascending priority/insertion order. Like Vim, select one
# winning match rather than merging every overlapping match's attributes.
# Search has priority zero and wins ties with a priority-zero plugin match.
def RenderMatchRanges(text: string, lineNumber: number, matches: list<dict<any>>): list<dict<any>>
  var ranges: list<dict<any>> = []
  for decoration in matches
    var style = RenderStyle(hlID(decoration.group))
    if style == RenderStyle(0)
      continue
    endif
    if has_key(decoration, 'pattern')
      var offset = 0
      while offset <= strlen(text)
        # matchadd() always starts case-sensitive and magic, independent of
        # ignorecase/smartcase/magic. Explicit pattern switches still win.
        var found = matchstrpos(text, '\m\C' .. decoration.pattern, offset)
        if found[1] < 0
          break
        endif
        if found[1] == found[2]
          throw 'Zero-width Vim match patterns use the compatibility view'
        endif
        add(ranges, {start: found[1], end: found[2], priority: decoration.priority, style: style})
        offset = found[2]
        if len(ranges) > 512
          throw 'Vim matches exceed the bounded native line decoration budget'
        endif
      endwhile
    else
      for [key, position] in items(decoration)
        if key !~# '^pos\d\+$' || position[0] != lineNumber
          continue
        endif
        var start = len(position) == 1 ? 0 : position[1] - 1
        var end = len(position) == 1 ? strlen(text) : start + position[2]
        # Positional matches highlight existing text, never the empty EOL
        # cell. A byte inside a multi-byte character is not its start.
        if end > start && end > 0 && start < strlen(text)
          add(ranges, {start: start, end: end, priority: decoration.priority, style: style})
          if len(ranges) > 512
            throw 'Vim matches exceed the bounded native line decoration budget'
          endif
        endif
      endfor
    endif
  endfor
  var search: list<dict<any>> = []
  var searchStyle = RenderStyle(hlID('Search'))
  if searchStyle != RenderStyle(0)
    for span in RenderSearchRanges(text)
      add(search, {start: span[0], end: span[1], priority: 0, style: searchStyle})
    endfor
  endif
  if len(ranges) + len(search) > 512
    throw 'Vim matches exceed the bounded native line decoration budget'
  endif
  return filter(copy(ranges), (_, span) => span.priority <= 0) + search
    + filter(ranges, (_, span) => span.priority > 0)
enddef

def RenderSigns(first: number, last: number): dict<any>
  var result: dict<any> = {}
  var placed = get(sign_getplaced(bufnr(), {group: '*'}), 0, {signs: []}).signs
  for sign in placed
    if sign.lnum < first || sign.lnum > last
      continue
    endif
    var definition = get(sign_getdefined(sign.name), 0, {})
    var key = string(sign.lnum)
    if !has_key(result, key) || sign.priority > result[key].priority
      result[key] = extend(copy(definition), {priority: sign.priority})
    elseif sign.priority == result[key].priority
      # Vim combines equal-priority text and line-highlight signs.
      extend(result[key], definition, 'keep')
    endif
  endfor
  return result
enddef

def RenderSearchRanges(text: string): list<list<number>>
  var ranges: list<list<number>> = []
  if !&hlsearch || !v:hlsearch || empty(@/)
    return ranges
  endif
  # matchstrpos() uses the same Vim regex engine. Explicit \c/\C inside the
  # pattern retain their usual precedence over the search option defaults.
  var pattern = (&ignorecase && !(&smartcase && @/ =~# '\u') ? '\c' : '\C') .. @/
  var offset = 0
  while offset <= strlen(text)
    var found = matchstrpos(text, pattern, offset)
    if found[1] < 0
      break
    endif
    if found[1] == found[2]
      throw 'Zero-width Vim search highlights use the compatibility view'
    endif
    add(ranges, [found[1], max([found[1] + 1, found[2]])])
    if len(ranges) > 512
      throw 'Vim search exceeds the bounded native line decoration budget'
    endif
    offset = found[2] > found[1] ? found[2] : found[1] + 1
  endwhile
  return ranges
enddef

def AddRenderRun(runs: list<dict<any>>, glyph: dict<any>, style: dict<any>, previousPlain: bool): bool
  # ASCII shaping may be batched. A non-ASCII grapheme always keeps its own
  # Vim-owned display advance, preventing fallback fonts from moving neighbors.
  # A coalesced run must preserve a one-byte/one-cell source mapping. Expanded
  # tabs remain separate so a native hover can recover the exact buffer byte.
  # Extraction already classifies printable single-byte, non-tab glyphs.
  # Selection segments preserve that classification and the byte/cell ratio.
  # Track the previous classification separately: a one-cell fragment of an
  # expanded tab must not merge with neighboring ASCII despite its short text.
  var ascii = glyph.plain
  if !empty(runs) && ascii && previousPlain
    var previous = runs[-1]
    if previous.row == glyph.row && previous.column + previous.cells == glyph.column
        && previous.line == glyph.line && previous.byte_end == glyph.byte_start
        && previous.style == style
      previous.text ..= glyph.text
      previous.cells += glyph.cells
      previous.byte_end = glyph.byte_end
      return true
    endif
  endif
  # Materialize only wire fields. Copying every cached paint/selection field
  # and deleting it again was paid for every visible glyph on every frame.
  add(runs, {row: glyph.row, column: glyph.column, text: glyph.text,
    cells: glyph.cells, line: glyph.line, byte_start: glyph.byte_start,
    byte_end: glyph.byte_end, style: style})
  return ascii
enddef

def RenderSelectionSegments(glyph: dict<any>, visual: string, anchor: list<number>, current: list<number>, block: list<number>): list<dict<any>>
  if empty(visual) || visual ==# 'V' || !glyph.plain || glyph.cells <= 1
    return [glyph]
  endif
  var cuts = [0, glyph.cells]
  if visual ==# "\<C-V>"
    for column in block
      add(cuts, max([0, min([glyph.cells, column - glyph.virtual_column])]))
    endfor
  else
    for position in [anchor, current]
      if position[1] == glyph.line
        var column = position[2] - 1 - glyph.byte_start
        add(cuts, max([0, min([glyph.cells, column])]))
        add(cuts, max([0, min([glyph.cells, column + 1])]))
      endif
    endfor
  endif
  cuts = uniq(sort(cuts, 'n'))
  var result: list<dict<any>> = []
  for index in range(len(cuts) - 1)
    var start = cuts[index]
    var count = cuts[index + 1] - start
    if count > 0
      var part = copy(glyph)
      part.text = strpart(glyph.text, start, count)
      part.column += start
      part.virtual_column += start
      part.cells = count
      part.byte_start += start
      part.byte_end = part.byte_start + count
      add(result, part)
    endif
  endfor
  return result
enddef

# Column guides are window-local display overlays, not source-line syntax.
# Split only ASCII spans and expanded tabs; a wide/combining glyph keeps its
# atomic source range and takes the style at its first display cell, like Vim.
def RenderColumnSegments(glyph: dict<any>, columns: list<number>, leftcol: number): list<dict<any>>
  if empty(columns) || (!glyph.plain && !get(glyph, 'tab', false))
    return [glyph]
  endif
  var first = max([glyph.virtual_column, leftcol])
  var cuts = [0, glyph.cells]
  for column in columns
    if column >= first && column < first + glyph.cells
      add(cuts, column - first)
      add(cuts, column - first + 1)
    endif
  endfor
  cuts = uniq(sort(cuts, 'n'))
  if len(cuts) == 2
    return [glyph]
  endif
  var result: list<dict<any>> = []
  for index in range(len(cuts) - 1)
    var start = cuts[index]
    var count = cuts[index + 1] - start
    var part = copy(glyph)
    part.text = strpart(glyph.text, start, count)
    part.column += start
    part.virtual_column = first + start
    part.cells = count
    if glyph.plain
      part.byte_start += start
      part.byte_end = part.byte_start + count
    endif
    add(result, part)
  endfor
  return result
enddef

def RenderColorColumns(leftcol: number, width: number): list<number>
  var columns: list<number> = []
  # Vim accepts at most 256 entries, with signed entries relative to textwidth.
  for entry in split(&colorcolumn, ',')[0 : 255]
    var relative = entry =~# '^[+-]'
    if relative && &textwidth == 0
      continue
    endif
    var column = str2nr(entry) + (relative ? &textwidth : 0) - 1
    if column >= leftcol && column < leftcol + width
      add(columns, column)
    endif
  endfor
  return uniq(sort(columns, 'n'))
enddef

def RenderSelected(glyph: dict<any>, visual: string, anchor: list<number>, current: list<number>, block: list<number>): bool
  if empty(visual) || glyph.line == 0
    return false
  endif
  var lowLine = min([anchor[1], current[1]])
  var highLine = max([anchor[1], current[1]])
  if glyph.line < lowLine || glyph.line > highLine
    return false
  endif
  if visual ==# 'V'
    return true
  elseif visual ==# "\<C-V>"
    return glyph.virtual_column < block[1]
      && glyph.virtual_column + glyph.cells > block[0]
  endif
  var forward = anchor[1] < current[1] || (anchor[1] == current[1] && anchor[2] <= current[2])
  var first = forward ? anchor : current
  var last = forward ? current : anchor
  if (glyph.line == first[1] && glyph.byte_end <= first[2] - 1)
      || (glyph.line == last[1] && glyph.byte_start > last[2] - 1)
    return false
  endif
  if &selection ==# 'exclusive' && glyph.line == current[1]
      && glyph.byte_start <= current[2] - 1 && glyph.byte_end > current[2] - 1
    return false
  endif
  return true
enddef

var nativeScrollLeft = -1

def SampleNativeScrollLeft()
  nativeScrollLeft = winsaveview().leftcol
enddef

# Standard primary-pointer gestures only. Custom mappings, Select-mode and
# virtual columns remain owned by the compatibility input path.
def NativePointerAvailable(): bool
  const current = mode(1)
  # 'block' only permits virtual space in blockwise Visual mode, which is
  # already excluded here. Normal/characterwise source targets remain exact.
  if !NativeTextBuffer(&buftype) || pumvisible() || !empty(VisiblePopups()) || index(['n', 'v'], current) < 0
      || (!empty(&virtualedit) && &virtualedit !=# 'block')
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

def NativeScrollAvailable(): bool
  # Scrolling does not synthesize a pointer gesture. Mouse mappings, mouse=
  # and virtualedit=block must not disable direct touchpad pixels. Keep busy
  # modes and completion under Vim's compatibility input ownership.
  return NativeTextBuffer(&buftype) && index(['n', 'i'], mode(1)) >= 0
    && !pumvisible() && empty(VisiblePopups())
enddef

def NativeSourceWindow(request: dict<any>): number
  const window = PositiveNumber(request, 'window')
  const buffer = PositiveNumber(request, 'buffer')
  const tick: any = get(request, 'changedtick', -1)
  const info = getwininfo(window)
  if win_id2win(window) == 0 || empty(info) || info[0].bufnr != buffer
      || type(tick) != v:t_number || tick < 0 || getbufvar(buffer, 'changedtick') != tick
      || !NativeTextBuffer(getbufvar(buffer, '&buftype'))
    throw 'The native source window or buffer generation changed'
  endif
  return window
enddef

def ScrollSourceWindow(request: dict<any>, id: number)
  SafeToChange()
  if !NativeScrollAvailable() || SaveInputPending()
    throw 'Precision scrolling requires idle Normal or Insert mode'
  endif
  const window = NativeSourceWindow(request)
  const info = getwininfo(window)[0]
  const top = PositiveNumber(request, 'topline')
  const left: any = get(request, 'leftcol', -1)
  if info.topline != top
    throw 'The native viewport changed before scrolling'
  endif
  nativeScrollLeft = -1
  win_execute(window, 'call ' .. expand('<SID>') .. 'SampleNativeScrollLeft()')
  if type(left) != v:t_number || left < 0 || nativeScrollLeft != left
    throw 'The horizontal native viewport changed before scrolling'
  endif
  # Batch only the validated scroll transaction. In particular, getchar(1)
  # above can run nested callbacks while a save waits for SafeState; those
  # unrelated input/save callbacks retain their existing immediate delivery.
  sendBatchDepth += 1
  try
    ScrollWindow(request)
    scrollAcknowledgements = filter(scrollAcknowledgements, (key, _) => win_id2win(str2nr(key)) > 0)
    scrollAcknowledgements[string(window)] = id
    # Keep semantic acknowledgement before response, but in one channel write.
    # Separate small writes can wait for TCP's delayed ACK/Nagle timer and
    # stall the next source-bound request by roughly 40ms even on loopback.
    EmitRender()
    Reply(id, true)
  finally
    sendBatchDepth -= 1
    if sendBatchDepth == 0
      FlushSendBatch()
    endif
  endtry
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
      && previous.target !=# [lineNumber, byte]
    normal! v
  endif
  cursor(lineNumber, byte + 1)
  if phase !=# 'release'
    pointerGesture = {gesture: gesture, window: window, buffer: bufnr(),
      tick: b:changedtick, mode: mode(1), cursor: getcurpos()[1 : 2], target: [lineNumber, byte]}
  endif
  Changed()
  EmitRender()
enddef

def RenderWindowFrame(margin: number = 0): dict<any>
  var windowNormal = WindowNormalStyle()
  var listChars = RenderListChars()
  var info = getwininfo(win_getid())[0]
  var view = {row: info.winrow - 1, column: info.wincol - 1,
    rows: info.height, columns: info.width}
  # Keep a statically typed local for the append hot path. Passing frame.runs
  # (dict<any> lookup) into AddRenderRun makes Vim recursively infer/check the
  # growing list on every glyph, even when its parameter is list<any>.
  var paintRuns: list<dict<any>> = []
  var frame: dict<any> = {event: 'render', supported: true, fallback_reason: '',
    rows: &lines, columns: &columns, window: win_getid(), buffer: bufnr(),
    changedtick: b:changedtick, mode: mode(1), view: view,
    default_style: renderNormal, cursor: {row: 0, column: 0, width: 1, visible: false},
    runs: paintRuns, completion: v:null, popups: []}
  var reason = RenderFallback()
  var quickfixLine = QuickfixCurrentLine(info)
  if quickfixLine < 0 || quickfixLine > line('$')
    reason = 'This quickfix window has no authoritative list identity'
  endif
  if !empty(reason)
    frame.supported = false
    frame.fallback_reason = reason
    return frame
  endif
  var savedView = winsaveview()
  var first = line('w0')
  var last = line('w$')
  frame.topline = first
  frame.botline = last
  frame.leftcol = savedView.leftcol
  frame.textoff = info.textoff
  frame.line_count = line('$')
  frame.overscan = []
  frame.native_pointer = renderProtocol >= 3 && NativePointerAvailable()
  frame.native_scroll = renderProtocol >= 3 && NativeScrollAvailable()
  frame.scroll_ack = get(scrollAcknowledgements, string(win_getid()), 0)
  frame.line_lengths = map(getline(first, last), (_, text) => strlen(text))
  var visibleFirst = first
  if margin > 0
    first = max([1, first - margin])
    last = min([line('$'), last + margin])
  endif
  if view.rows > 200 || view.columns > 400 || last - first > 200 + 2 * margin
    frame.supported = false
    frame.fallback_reason = 'This viewport exceeds the bounded native paint budget'
    return frame
  endif
  var textWidth = view.columns - info.textoff
  if textWidth < 1
    frame.supported = false
    frame.fallback_reason = 'This Vim window has no visible text columns'
    return frame
  endif
  var numberWidth = (&number || &relativenumber)
    ? max([&numberwidth, strlen(string(line('$'))) + 1]) : 0
  var signWidth = info.textoff - numberWidth
  var signs = RenderSigns(first, last)
  if !empty(signs) && (signWidth > 2 || signWidth < 0)
    frame.supported = false
    frame.fallback_reason = 'Multiple Vim sign columns use the compatibility view'
    return frame
  endif
  for sign in values(signs)
    var signText = get(sign, 'text', '')
    if !empty(get(sign, 'icon', '')) || strdisplaywidth(signText) > 2
        || signText =~# '[\x00-\x1f\x7f]'
      frame.supported = false
      frame.fallback_reason = 'This image or complex Vim sign uses the compatibility view'
      return frame
    endif
  endfor
  # Opening/closing a fold does not change b:changedtick. Check its display
  # state independently of the buffer/syntax cache on every paint attempt.
  for lineNumber in range(first, last)
    if foldclosed(lineNumber) >= 0
      frame.supported = false
      frame.fallback_reason = 'Closed Vim folds use the compatibility view'
      return frame
    endif
  endfor
  var properties = prop_list(first, {end_lnum: last})
  var matches = getmatches()
  if len(properties) > 2048
    frame.supported = false
    frame.fallback_reason = 'This property list exceeds the bounded native paint budget'
    return frame
  endif
  for property in properties
    if has_key(property, 'text') || property.col <= 0
      frame.supported = false
      frame.fallback_reason = 'Vim virtual text uses the compatibility view'
      return frame
    endif
    var propertyType = get(property, 'type', '')
    var typeBuffer = get(property, 'type_bufnr', 0)
    property.definition = empty(propertyType) ? {}
      : (typeBuffer > 0 ? prop_type_get(propertyType, {bufnr: typeBuffer}) : prop_type_get(propertyType))
  endfor
  # A scroll changes screen rows, not the semantic paint of every surviving
  # buffer line. A buffer edit still invalidates all lines: syntax regions can
  # propagate arbitrarily far beyond the edited line.
  var generation = string([bufnr(), b:changedtick, savedView.leftcol,
    view.column, view.columns, info.textoff, &tabstop, &vartabstop, &wrap, &syntax, &synmaxcol,
    &number, &relativenumber, &numberwidth, &ambiwidth, &list, &listchars, renderThemeGeneration, windowNormal,
    &hlsearch, v:hlsearch, @/, &ignorecase, &smartcase, &magic, &iskeyword, &isprint, matches])
  if generation !=# renderLineGeneration
    renderLineGeneration = generation
    renderLineCache = {}
  endif
  var baseKey = string([generation, first, last, view, properties])
  if baseKey !=# renderBaseKey
    renderBase = []
    var nextLines: dict<dict<any>> = {}
    var noListStyle: dict<any> = {}
    for lineNumber in range(first, last)
      var text = getline(lineNumber)
      var row = view.row + lineNumber - first
      var lineProperties = filter(copy(properties), (_, item) => item.lnum == lineNumber)
      var lineKey = string([text, lineProperties])
      var cached = get(renderLineCache, string(lineNumber), {})
      if !empty(cached) && cached.key ==# lineKey
        for stored in cached.base
          var glyph = copy(stored)
          glyph.row = row
          add(renderBase, glyph)
        endfor
        nextLines[string(lineNumber)] = cached
        continue
      endif
      if foldclosed(lineNumber) >= 0
        reason = 'Closed Vim folds use the compatibility view'
        break
      elseif strlen(text) > 16384 || strchars(text) > 4096
        reason = 'This long line exceeds the bounded native paint budget'
        break
      elseif &wrap && strdisplaywidth(text) > textWidth
        reason = 'Wrapped Vim continuation rows use the compatibility view'
        break
      elseif &list && &wrap && has_key(listChars, 'eol') && strdisplaywidth(text) >= textWidth
        reason = 'A wrapped listchars EOL marker uses the compatibility view'
        break
      elseif &list && stridx(text, "\t") >= 0 && !has_key(listChars, 'tab')
        reason = 'Default ^I listchars tab geometry uses the compatibility view'
        break
      elseif &list && ((has_key(listChars, 'precedes') && savedView.leftcol > 0 && strdisplaywidth(text) > savedView.leftcol)
          || (has_key(listChars, 'extends') && !&wrap && strdisplaywidth(text) > savedView.leftcol + textWidth))
        reason = 'This active listchars horizontal-edge marker uses the compatibility view'
        break
      elseif text =~# '[\x00-\x08\x0a-\x1f\x7f\u0590-\u08ff\u200b-\u200f\u202a-\u202e\u2066-\u2069\ufeff]'
        reason = 'Control and bidirectional text use the compatibility view'
        break
      endif
      var matchRanges = RenderMatchRanges(text, lineNumber, matches)
      if len(lineProperties) > 128
        reason = 'This line exceeds the native property paint budget'
        break
      endif
      var runStart = len(renderBase)
      var byteColumn = 0
      var virtualColumn = 0
      var asciiLine = strlen(text) == strchars(text)
      var leadEnd = &list ? match(text, '[^ \t]') : 0
      var trailStart = &list ? match(text, '[ \t]*$') : strlen(text)
      if leadEnd < 0
        leadEnd = strlen(text)
      endif
      for charIndex in range(strchars(text, true))
        var cluster = asciiLine ? strpart(text, charIndex, 1) : strcharpart(text, charIndex, 1, true)
        var plain = strlen(cluster) == 1 && cluster !=# "\t"
        var cells = plain ? 1 : strdisplaywidth(cluster, virtualColumn)
        var display = cluster
        var listStyle = noListStyle
        var listedTab = &list && cluster ==# "\t"
        if &list && cluster ==# ' '
          var field = byteColumn >= trailStart && has_key(listChars, 'trail') ? 'trail'
            : (byteColumn < leadEnd && has_key(listChars, 'lead') ? 'lead' : 'space')
          if has_key(listChars, field)
            display = listChars[field][0]
            listStyle = RenderStyle(hlID('SpecialKey'))
            plain = false
          endif
        elseif &list && (cluster ==# "\u00a0" || cluster ==# "\u202f") && has_key(listChars, 'nbsp')
          if cells != 1
            reason = 'This nonbreaking-space listchars width uses the compatibility view'
            break
          endif
          display = listChars.nbsp[0]
          listStyle = RenderStyle(hlID('SpecialKey'))
          plain = false
        elseif listedTab
          listStyle = RenderStyle(hlID('SpecialKey'))
        endif
        var nextByte = byteColumn + strlen(cluster)
        var start = virtualColumn - savedView.leftcol
        var finish = start + cells
        if finish > 0 && start < textWidth
          if cells <= 0 || ((start < 0 || finish > textWidth) && cluster !=# "\t")
            reason = 'A clipped or zero-width Vim glyph uses the compatibility view'
            break
          endif
          var visibleStart = max([0, start])
          var visibleEnd = min([textWidth, finish])
          var style = RenderStyle(synID(lineNumber, byteColumn + 1, true))
          var propertyStyle: dict<any> = {}
          var propertyPriority = -2147483648
          var propertyOverride = false
          var propertyCombine = true
          for property in lineProperties
            var definition = property.definition
            var priority = get(definition, 'priority', 0)
            if byteColumn < property.col - 1 + property.length && nextByte > property.col - 1
                && priority > propertyPriority && !empty(get(definition, 'highlight', ''))
              propertyStyle = RenderStyle(hlID(definition.highlight))
              propertyPriority = priority
              propertyOverride = !!get(definition, 'override', false)
              propertyCombine = !!get(definition, 'combine', true)
            endif
          endfor
          if !empty(propertyStyle)
            style = propertyCombine ? MergeRenderStyle(style, propertyStyle) : copy(propertyStyle)
          endif
          var matchStyle: dict<any> = {}
          for matchRange in matchRanges
            if byteColumn >= matchRange.start && byteColumn < matchRange.end
              matchStyle = matchRange.style
            endif
          endfor
          var previous = empty(renderBase) ? {} : renderBase[-1]
          if plain && !empty(previous) && previous.plain && previous.row == row
              && previous.byte_end == byteColumn && previous.style == style
              && previous.column + previous.cells == view.column + info.textoff + visibleStart
              && previous.override_style == (propertyOverride ? propertyStyle : {})
              && previous.match_style == matchStyle
            previous.text ..= cluster
            previous.cells += 1
            previous.byte_end = nextByte
          elseif listedTab
            # One explicit glyph per display cell keeps Unicode marker bytes
            # out of ASCII selection/column slicing. Every cell still names
            # the same original one-byte Tab, never the replacement text.
            var markers = listChars.tab
            for visibleColumn in range(visibleStart, visibleEnd - 1)
              var cell = visibleColumn - start
              var marker = len(markers) == 3 && cell == cells - 1 ? markers[2]
                : (cell == 0 ? markers[0] : markers[1])
              add(renderBase, {row: row, column: view.column + info.textoff + visibleColumn,
                text: marker, cells: 1, line: lineNumber, byte_start: byteColumn, byte_end: nextByte,
                virtual_column: virtualColumn + cell, plain: false, tab: true, list_style: listStyle,
                style: style, override_style: propertyOverride ? propertyStyle : {}, match_style: matchStyle})
            endfor
          else
            add(renderBase, {row: row, column: view.column + info.textoff + visibleStart,
              text: cluster ==# "\t" ? repeat(' ', visibleEnd - visibleStart) : display,
              cells: visibleEnd - visibleStart, line: lineNumber,
              byte_start: byteColumn, byte_end: nextByte,
              virtual_column: virtualColumn, plain: plain, tab: cluster ==# "\t",
              style: style, list_style: listStyle, override_style: propertyOverride ? propertyStyle : {}, match_style: matchStyle})
          endif
        endif
        byteColumn = nextByte
        virtualColumn += cells
        if virtualColumn >= savedView.leftcol + textWidth
          break
        endif
      endfor
      if !empty(reason)
        break
      endif
      var lineBase = runStart < len(renderBase) ? renderBase[runStart :] : []
      # Default/syntax/property/match composition is source-line data, not
      # viewport data. Surviving lines reuse it while cursor/Visual overlays
      # are composed from current Vim state below.
      for glyph in lineBase
        var paintStyle = MergeRenderStyle(windowNormal, glyph.style)
        if &list && !empty(glyph.list_style) && empty(glyph.match_style)
          paintStyle = MergeRenderStyle(paintStyle, glyph.list_style)
        endif
        if !empty(glyph.match_style)
          paintStyle = MergeRenderStyle(paintStyle, glyph.match_style)
        endif
        if !empty(glyph.override_style)
          paintStyle = MergeRenderStyle(paintStyle, glyph.override_style)
        endif
        glyph.paint_style = paintStyle
      endfor
      nextLines[string(lineNumber)] = {key: lineKey, base: lineBase}
    endfor
    if empty(reason)
      renderBaseKey = baseKey
      # Keep only the visible lines. Scrolling one row reuses every surviving
      # row without accumulating a second copy of a large Vim document.
      renderLineCache = nextLines
    endif
  endif
  if !empty(reason)
    renderBaseKey = ''
    frame.supported = false
    frame.fallback_reason = reason
    return frame
  endif
  if windowNormal != renderNormal
    # This existing run shape carries per-window Normal without a protocol
    # extension. It covers gutters and blank/EOF cells before their own groups
    # and source glyphs, and never advertises a fabricated buffer position.
    var lastBackground = max([view.row + last - first,
      view.row + view.rows + visibleFirst - first - 1])
    for row in range(view.row, lastBackground)
      add(paintRuns, {row: row, column: view.column, cells: view.columns,
        text: '', line: 0, byte_start: 0, byte_end: 0, style: windowNormal})
    endfor
  endif
  var visual = mode(1)
  if index(['v', 'V', "\<C-V>"], visual) < 0
    visual = ''
  endif
  var anchor = getpos('v')
  var current = getpos('.')
  var block = [0, 0]
  if visual ==# "\<C-V>"
    var a = virtcol('v', true)
    var c = virtcol('.', true)
    block = [min([a[0], c[0]]) - 1, max([a[1], c[1]])]
    if &selection ==# 'exclusive' && c[0] > a[0]
      block[1] = c[0] - 1
    endif
  endif
  var visualStyle = RenderStyle(hlID('Visual'))
  var cursorLineStyle = RenderStyle(hlID('CursorLine'))
  var position = screenpos(win_getid(), line('.'), col('.'))
  # virtcol('.') reports the last cell of a wide character. CursorColumn
  # follows Vim's actual cursor cell: first for wide text, last for a Normal
  # mode tab, and first for an Insert mode tab. Reuse the cursor paint oracle.
  var cursorColumn = &cursorcolumn
    ? (get(position, 'curscol', 0) > 0
      ? position.curscol - 1 - view.column - info.textoff + savedView.leftcol
      : virtcol('.', true)[0] - 1) : -1
  var colorColumns = empty(&colorcolumn) ? [] : RenderColorColumns(savedView.leftcol, textWidth)
  var guideEnabled = !empty(colorColumns)
    || (cursorColumn >= savedView.leftcol && cursorColumn < savedView.leftcol + textWidth)
  # With both guides disabled, retain the ordinary single paint loop: no
  # per-line guide dictionaries and no per-glyph guide segmentation calls.
  var colorStyle = RenderStyle(hlID('ColorColumn'))
  var cursorGuideStyle = RenderStyle(hlID('CursorColumn'))
  var guideRows: dict<dict<any>> = {}
  var rowStyles: dict<dict<any>> = {}
  for lineNumber in range(first, last)
    var sign = get(signs, string(lineNumber), {})
    var lineStyle: dict<any> = {}
    if !empty(get(sign, 'linehl', ''))
      lineStyle = RenderStyle(hlID(sign.linehl))
    endif
    var currentQuickfix = quickfixLine > 0 && lineNumber == quickfixLine
    if currentQuickfix
      # Vim applies the current quickfix item independently of its cursor or
      # changedtick. It replaces sign linehl before CursorLine composition.
      lineStyle = RenderStyle(hlID('QuickFixLine'))
    endif
    var lineCursor = &cursorline && lineNumber == line('.') && &cursorlineopt !=# 'number'
    if lineCursor
      var lineWins = !empty(get(sign, 'linehl', '')) ? get(sign, 'priority', 0) >= 100 : currentQuickfix
      lineStyle = lineWins
        ? MergeRenderStyle(cursorLineStyle, lineStyle) : MergeRenderStyle(lineStyle, cursorLineStyle)
    endif
    if !empty(lineStyle)
      rowStyles[string(lineNumber)] = lineStyle
      # A whole-window Normal backdrop is identified by line=0/empty text.
      # QuickFixLine is one source row, not a new Normal for the entire pane.
      add(frame.runs, {row: view.row + lineNumber - first, column: view.column + info.textoff,
        cells: textWidth, text: repeat(' ', textWidth), line: lineNumber,
        byte_start: 0, byte_end: 0, style: MergeRenderStyle(windowNormal, lineStyle)})
    endif
    if guideEnabled
      var guides: dict<any> = {}
      for column in colorColumns
        guides[string(column)] = colorStyle
      endfor
      if cursorColumn >= savedView.leftcol && cursorColumn < savedView.leftcol + textWidth
          && lineNumber != current[1]
        guides[string(cursorColumn)] = cursorGuideStyle
      endif
      guideRows[string(lineNumber)] = {styles: guides, columns: map(keys(guides), (_, column) => str2nr(column))}
      # Source-free runs paint guides beyond EOL. A one-column pane uses a
      # space to avoid the empty-text/full-width Normal backdrop signature.
      for [column, guideStyle] in items(guides)
        add(frame.runs, {row: view.row + lineNumber - first,
          column: view.column + info.textoff + str2nr(column) - savedView.leftcol,
          text: view.columns == 1 ? ' ' : '', cells: 1, line: 0, byte_start: 0, byte_end: 0,
          style: MergeRenderStyle(MergeRenderStyle(windowNormal, lineStyle), guideStyle)})
      endfor
    endif
  endfor
  var previousPlain = false
  for baseGlyph in renderBase
    if visual ==# "\<C-V>" && strpart(getline(baseGlyph.line), baseGlyph.byte_start, baseGlyph.byte_end - baseGlyph.byte_start) ==# "\t"
      frame.supported = false
      frame.fallback_reason = 'Block selections through Vim tabs use the compatibility view'
      frame.runs = []
      return frame
    endif
    var glyphs = empty(visual) ? [baseGlyph] : RenderSelectionSegments(baseGlyph, visual, anchor, current, block)
    var paintGuide = guideEnabled && empty(baseGlyph.match_style)
      && (empty(visual) || baseGlyph.line < min([anchor[1], current[1]])
        || baseGlyph.line > max([anchor[1], current[1]]))
    if paintGuide
      var columns = guideRows[string(baseGlyph.line)].columns
      var pieces: list<dict<any>> = []
      for selected in glyphs
        extend(pieces, RenderColumnSegments(selected, columns, savedView.leftcol))
      endfor
      glyphs = pieces
    endif
    for glyph in glyphs
      var style = glyph.paint_style
      var selectedCell = !empty(visual) && RenderSelected(glyph, visual, anchor, current, block)
      if !empty(visual) || has_key(rowStyles, string(glyph.line))
        style = MergeRenderStyle(windowNormal, glyph.style)
        if has_key(rowStyles, string(glyph.line))
          style = MergeRenderStyle(style, rowStyles[string(glyph.line)])
        endif
        if &list && !empty(glyph.list_style) && empty(glyph.match_style) && !selectedCell
          style = MergeRenderStyle(style, glyph.list_style)
        endif
        if !empty(glyph.match_style)
          style = MergeRenderStyle(style, glyph.match_style)
        endif
        if selectedCell
          style = MergeRenderStyle(style, visualStyle)
        endif
        if !empty(glyph.override_style)
          style = MergeRenderStyle(style, glyph.override_style)
        endif
      endif
      if paintGuide
        var guide = get(guideRows[string(glyph.line)].styles, string(max([glyph.virtual_column, savedView.leftcol])), {})
        if !empty(guide)
          style = MergeRenderStyle(style, guide)
        endif
      endif
      previousPlain = AddRenderRun(paintRuns, glyph, style, previousPlain)
    endfor
  endfor
  if has_key(listChars, 'eol')
    for lineNumber in range(first, last)
      var column = strdisplaywidth(getline(lineNumber)) - savedView.leftcol
      if column >= 0 && column < textWidth
        var style = MergeRenderStyle(windowNormal, get(rowStyles, string(lineNumber), {}))
        style = MergeRenderStyle(style, RenderStyle(hlID('NonText')))
        var visualLine = !empty(visual) && lineNumber >= min([anchor[1], current[1]])
          && lineNumber <= max([anchor[1], current[1]])
        if guideEnabled && !visualLine
          var guide = get(guideRows[string(lineNumber)].styles, string(column + savedView.leftcol), {})
          if !empty(guide)
            style = MergeRenderStyle(style, guide)
          endif
        endif
        add(paintRuns, {row: view.row + lineNumber - first,
          column: view.column + info.textoff + column, text: listChars.eol[0], cells: 1,
          line: 0, byte_start: 0, byte_end: 0, style: style})
      endif
    endfor
  endif
  # Vim selects one display cell for the newline, including an empty buffer
  # line. Painting only existing glyphs would silently erase that selection.
  if visual ==# 'V' || visual ==# 'v'
    var lowLine = min([anchor[1], current[1]])
    var highLine = max([anchor[1], current[1]])
    for lineNumber in range(max([first, lowLine]), min([last, highLine]))
      var text = getline(lineNumber)
      var endColumn = strdisplaywidth(text) - savedView.leftcol
      if endColumn >= 0 && endColumn < textWidth
          && (visual ==# 'V' || lineNumber < highLine
            || (!has_key(listChars, 'eol') && max([anchor[2], current[2]]) > strlen(text)))
        var endStyle = MergeRenderStyle(windowNormal, visualStyle)
        add(frame.runs, {row: view.row + lineNumber - first,
          column: view.column + info.textoff + endColumn,
          text: has_key(listChars, 'eol') ? listChars.eol[0] : ' ', cells: 1, line: has_key(listChars, 'eol') ? 0 : lineNumber,
          byte_start: strlen(text), byte_end: strlen(text), style: endStyle})
      endif
    endfor
  endif
  for lineNumber in range(first, last)
    var sign = get(signs, string(lineNumber), {})
    var signText = get(sign, 'text', '')
    var signHighlight = &cursorline && lineNumber == line('.') && !empty(get(sign, 'culhl', ''))
      ? sign.culhl : get(sign, 'texthl', '')
    if signWidth > 0
      var columnStyle = MergeRenderStyle(windowNormal,
        RenderStyle(hlID(&cursorline && lineNumber == line('.') ? 'CursorLineSign' : 'SignColumn')))
      var label = signText .. repeat(' ', max([0, signWidth - strdisplaywidth(signText)]))
      add(frame.runs, {row: view.row + lineNumber - first, column: view.column,
        text: label, cells: signWidth, line: 0, byte_start: 0, byte_end: 0,
        style: empty(signHighlight) ? columnStyle : MergeRenderStyle(columnStyle, RenderStyle(hlID(signHighlight)))})
    endif
    if &number || &relativenumber
      var number = &relativenumber && (lineNumber != line('.') || !&number)
        ? abs(lineNumber - line('.')) : lineNumber
      var label = printf('%*d ', max([1, numberWidth - 1]), number)
      var style = MergeRenderStyle(windowNormal,
        RenderStyle(hlID(lineNumber == line('.') ? 'CursorLineNr' : 'LineNr')))
      if !empty(get(sign, 'numhl', ''))
        style = MergeRenderStyle(style, RenderStyle(hlID(sign.numhl)))
      endif
      if &signcolumn ==# 'number' && !empty(signText)
        label = repeat(' ', max([0, numberWidth - strdisplaywidth(signText) - 1])) .. signText .. ' '
        if !empty(signHighlight)
          style = MergeRenderStyle(style, RenderStyle(hlID(signHighlight)))
        endif
      endif
      add(frame.runs, {row: view.row + lineNumber - first, column: view.column + max([0, signWidth]),
        text: label, cells: strdisplaywidth(label), line: 0, byte_start: 0, byte_end: 0,
        style: style})
    endif
  endfor
  for row in range(view.row + last - first + 1, max([view.row + last - first, view.row + view.rows + visibleFirst - first - 1]))
    add(frame.runs, {row: row, column: view.column, text: '~', cells: 1,
      line: 0, byte_start: 0, byte_end: 0,
      style: MergeRenderStyle(windowNormal, RenderStyle(hlID('EndOfBuffer')))})
  endfor
  if margin > 0
    var marginRows: dict<dict<any>> = {}
    for lineNumber in range(first, last)
      var relative = lineNumber - visibleFirst
      if relative < 0 || relative >= view.rows
        var edge = {row: relative, line: lineNumber, byte_length: strlen(getline(lineNumber)), runs: []}
        marginRows[string(relative)] = edge
        add(frame.overscan, edge)
      endif
    endfor
    var visibleRuns: list<dict<any>> = []
    for run in frame.runs
      var relative = run.row - view.row - (visibleFirst - first)
      if relative >= 0 && relative < view.rows
        run.row = view.row + relative
        add(visibleRuns, run)
      elseif has_key(marginRows, string(relative))
        run.row = 0
        add(marginRows[string(relative)].runs, run)
      endif
    endfor
    frame.runs = visibleRuns
  endif
  if get(position, 'row', 0) > 0 && get(position, 'curscol', 0) > 0
    frame.cursor = {row: position.row - 1, column: position.curscol - 1,
      width: strpart(getline('.'), col('.') - 1, 1) ==# "\t" ? 1 : max([1, position.endcol - position.curscol + 1]), visible: true,
      line: line('.'), byte_column: col('.') - 1, appearance: RenderCursorAppearance()}
  endif
  return frame
enddef

# win_execute() switches only Vim's evaluation context: no WinEnter/BufEnter
# and no directory changes. The command itself is explicitly noautocmd and
# this callback only reads buffer/window state. Do not use win_gotoid(),
# :normal, cursor(), or a text widget to manufacture a second editor state.
def SampleRenderWindow()
  var key = string(win_getid())
  var cache = get(renderWindowCaches, key, {})
  renderBaseKey = get(cache, 'key', '')
  renderBase = get(cache, 'base', [])
  renderLineGeneration = get(cache, 'generation', '')
  renderLineCache = get(cache, 'lines', {})
  try
    renderWindowSample = RenderWindowFrame(renderProtocol >= 4 ? 8 : (renderProtocol >= 3 ? 2 : 0))
    # An unsupported hidden row must not disable a supported visible viewport.
    # Retry without margins; never downgrade a real visible failure silently.
    if renderProtocol >= 3 && !renderWindowSample.supported
      renderBaseKey = ''
      renderBase = []
      renderWindowSample = RenderWindowFrame()
    endif
    renderWindowCaches[key] = renderWindowSample.supported
      ? {key: renderBaseKey, base: renderBase, generation: renderLineGeneration, lines: renderLineCache}
      : {key: '', base: [], generation: '', lines: {}}
    if win_getid() != renderSamplingWindow
      renderWindowSample.cursor.visible = false
    endif
    for run in renderWindowSample.runs
      run.window = win_getid()
    endfor
    for edge in get(renderWindowSample, 'overscan', [])
      for run in edge.runs
        run.window = win_getid()
      endfor
    endfor
  finally
    renderBaseKey = ''
    renderBase = []
    renderLineGeneration = ''
    renderLineCache = {}
  endtry
enddef

# Completion remains Vim-owned. Public APIs do not expose pum_first, so never
# guess which candidates a scrolling/fuzzy menu is showing. Render only a
# complete, unclipped ordinary list with exact Vim row/column geometry.
def CompletionText(runs: list<dict<any>>, text: string, row: number, column: number, style: dict<any>)
  var col = column
  for index in range(strchars(text, true))
    var cluster = strcharpart(text, index, 1, true)
    var cells = strdisplaywidth(cluster)
    if cells <= 0 || cells > 2
      throw 'Complex Vim completion text uses the compatibility view'
    endif
    if strlen(cluster) == 1 && !empty(runs) && runs[-1].row == row
        && runs[-1].column + runs[-1].cells == col && runs[-1].style == style
        && strlen(runs[-1].text) == strchars(runs[-1].text)
        && runs[-1].text =~# '^[\x20-\x7e]*$'
      runs[-1].text ..= cluster
      runs[-1].cells += cells
    else
      add(runs, {window: 0, row: row, column: col, text: cluster, cells: cells,
        line: 0, byte_start: 0, byte_end: 0, style: style})
    endif
    col += cells
  endfor
enddef

def RenderCompletion(): dict<any>
  var position = pum_getpos()
  if mode(1) !~# '^i' || empty(position) || position.size != position.height
      || position.scrollbar || position.height <= 0 || position.height > 64
      || position.width <= 0 || position.width > 511
    throw 'Scrolling Vim completion menus use the compatibility view'
  endif
  for option in ['pumborder', 'pumshadow', 'pumopt']
    if exists('+' .. option) && !empty(eval('&' .. option))
      throw 'Decorated Vim completion menus use the compatibility view'
    endif
  endfor
  if (exists('+pumopacity') && index([0, 100], eval('&pumopacity')) < 0)
      || (exists('+completeitemalign') && index(['', 'abbr,kind,menu'], eval('&completeitemalign')) < 0)
      || &completeopt =~# '\<fuzzy\>'
    throw 'Custom Vim completion layout uses the compatibility view'
  endif
  for pair in [['PmenuMatch', 'Pmenu'], ['PmenuMatchSel', 'PmenuSel']]
    var matchStyle = RenderStyle(hlID(pair[0]))
    if matchStyle != RenderStyle(0) && matchStyle != RenderStyle(hlID(pair[1]))
      throw 'Vim completion match highlighting uses the compatibility view'
    endif
  endfor
  var info = complete_info(['items', 'selected'])
  var candidates = get(info, 'items', [])
  if len(candidates) != position.size || info.selected < -1 || info.selected >= len(candidates)
    throw 'Filtered Vim completion menus use the compatibility view'
  endif
  var widths = [0, 0, 0]
  var labels: list<list<string>> = []
  for item in candidates
    var word = empty(get(item, 'abbr', '')) ? item.word : item.abbr
    var fields = [word, get(item, 'kind', ''), get(item, 'menu', '')]
    for field in range(3)
      var text = fields[field]
      if strlen(text) > 2048 || text =~# '[\x00-\x1f\x7f\u0590-\u08ff\u200b-\u200f\u202a-\u202e\u2066-\u2069\ufeff]'
        throw 'Complex Vim completion labels use the compatibility view'
      endif
      widths[field] = max([widths[field], strdisplaywidth(text) + (field > 0 && !empty(text) ? 1 : 0)])
    endfor
    add(labels, fields)
  endfor
  var starts = [position.col, position.col + widths[0] + 1,
    position.col + widths[0] + 1 + widths[1]]
  for field in range(3)
    var needed = widths[field] - (field > 0 && widths[field] > 0 ? 1 : 0)
    if needed > 0 && starts[field] + needed > position.col + position.width
      throw 'Clipped Vim completion labels use the compatibility view'
    endif
  endfor
  var left = position.col > 0 ? 1 : 0
  var view = {row: position.row, column: position.col - left,
    rows: position.height, columns: position.width + left}
  if view.row < 0 || view.column < 0 || view.row + view.rows > &lines
      || view.column + view.columns > &columns
    throw 'Vim completion geometry uses the compatibility view'
  endif
  var windowNormal = WindowNormalStyle()
  var normal = MergeRenderStyle(windowNormal, RenderStyle(hlID('Pmenu')))
  var runs: list<dict<any>> = []
  for index in range(len(candidates))
    var selected = index == info.selected
    var rowStyle = selected ? MergeRenderStyle(windowNormal, RenderStyle(hlID('PmenuSel'))) : normal
    var row = position.row + index
    CompletionText(runs, repeat(' ', view.columns), row, view.column, rowStyle)
    for field in range(3)
      var label = labels[index][field]
      if empty(label)
        continue
      endif
      var group = field == 0 ? (selected ? 'PmenuSel' : 'Pmenu')
        : (field == 1 ? (selected ? 'PmenuKindSel' : 'PmenuKind') : (selected ? 'PmenuExtraSel' : 'PmenuExtra'))
      var style = MergeRenderStyle(rowStyle, RenderStyle(hlID(group)))
      var custom = get(candidates[index], field == 0 ? 'abbr_hlgroup' : (field == 1 ? 'kind_hlgroup' : ''), '')
      if !empty(custom)
        style = MergeRenderStyle(style, RenderStyle(hlID(custom)))
      endif
      CompletionText(runs, label, row, starts[field], style)
    endfor
  endfor
  if len(runs) > 4096
    throw 'Vim completion exceeds the native paint budget'
  endif
  return {view: view, default_style: normal, runs: runs}
enddef

# Vim does not expose popup masks through popup_getoptions(). Only a provider
# that owns an opaque, rectangular, read-only text popup may opt in by setting
# w:jterm_native_popup = 1. It must remove that marker before adding a mask or
# any other transparency. Unregistered plugins retain Vim compatibility paint.
# Exposed geometry/options are still validated on every sample; this marker is
# a presentation contract, never permission to synthesize editor input.
var renderPopupSample: dict<any> = {}

def PopupBox(options: dict<any>, key: string): list<number>
  if !has_key(options, key)
    return [0, 0, 0, 0]
  endif
  var values: any = options[key]
  if type(values) != v:t_list
    throw 'Invalid Vim popup decoration uses the compatibility view'
  endif
  if empty(values)
    return [1, 1, 1, 1]
  endif
  if len(values) != 4
    throw 'Complex Vim popup decoration uses the compatibility view'
  endif
  var result: list<number> = []
  for value in values
    if type(value) != v:t_number || value < 0 || value > 64
      throw 'Invalid Vim popup decoration uses the compatibility view'
    endif
    add(result, key ==# 'border' ? (value > 0 ? 1 : 0) : value)
  endfor
  return result
enddef

def PopupNormalStyle(): dict<any>
  var group = WindowNormalGroup()
  if !group.supported
    throw 'Vim popup highlight remapping uses the compatibility view'
  endif
  # An empty popup wincolor means Pmenu, not the parent split's Normal.
  var style = copy(RenderStyle(hlID(empty(group.name) ? 'Pmenu' : group.name)))
  for color in ['foreground', 'background', 'special']
    if type(style[color]) != v:t_number
      style[color] = renderNormal[color]
    endif
  endfor
  return style
enddef

def SampleRenderPopup()
  var id = win_getid()
  var registered: any = getwinvar(id, 'jterm_native_popup', 0)
  if type(registered) != v:t_number || registered != 1
    throw 'Unregistered Vim popups use the compatibility view'
  endif
  var position = popup_getpos(id)
  var options = popup_getoptions(id)
  if empty(position) || empty(options) || !get(position, 'visible', 0)
    throw 'A Vim popup changed during native sampling'
  endif
  if has_key(options, 'filter') || !empty(get(options, 'mask', []))
      || !empty(get(options, 'title', '')) || get(options, 'close', 'none') ==# 'button'
      || get(options, 'cursorline', 0) || get(options, 'drag', 0)
      || get(options, 'dragall', 0) || get(options, 'resize', 0)
      || (has_key(options, 'opacity') && options.opacity != 100)
    throw 'Interactive or decorated Vim popups use the compatibility view'
  endif
  var view = {row: position.line - 1, column: position.col - 1,
    rows: position.height, columns: position.width}
  var zindex = get(options, 'zindex', 50)
  if view.row < 0 || view.column < 0 || view.rows < 1 || view.rows > 64
      || view.columns < 1 || view.columns > 512 || view.row + view.rows > &lines
      || view.column + view.columns > &columns || zindex < 1 || zindex > 32000
    throw 'Vim popup geometry exceeds the native paint budget'
  endif
  if position.scrollbar || position.firstline != 1 || position.core_height < line('$')
      || position.core_width < 1 || position.core_height < 1
    throw 'Scrolling or clipped Vim popups use the compatibility view'
  endif
  var border = PopupBox(options, 'border')
  var padding = PopupBox(options, 'padding')
  if position.core_line != position.line + border[0] + padding[0]
      || position.core_col != position.col + border[3] + padding[3]
      || position.width != position.core_width + border[1] + border[3] + padding[1] + padding[3]
      || position.height != position.core_height + border[0] + border[2] + padding[0] + padding[2]
    throw 'Clipped Vim popup decorations use the compatibility view'
  endif
  if (&buftype !=# 'popup' && !NativeTextBuffer(&buftype))
      || &rightleft || &arabic || &spell || &conceallevel > 0 || &list || &diff
      || &number || &relativenumber || &foldcolumn != 0
      || getwininfo(id)[0].textoff != 0 || !empty(getmatches())
    throw 'This Vim popup display option uses the compatibility view'
  endif
  var normal = PopupNormalStyle()
  var runs: list<dict<any>> = []
  # Opaque background includes padding and empty/minheight rows.
  for row in range(view.row, view.row + view.rows - 1)
    CompletionText(runs, repeat(' ', view.columns), row, view.column, normal)
  endfor
  var chars: list<string> = &encoding ==# 'utf-8' && &ambiwidth ==# 'single'
    ? ['═', '║', '═', '║', '╔', '╗', '╝', '╚']
    : ['-', '|', '-', '|', '+', '+', '+', '+']
  var supplied: any = get(options, 'borderchars', [])
  if !empty(supplied)
    if type(supplied) != v:t_list || len(supplied) != 8
      throw 'Complex Vim popup border characters use the compatibility view'
    endif
    for glyph in supplied
      if type(glyph) != v:t_string || strchars(glyph, true) != 1
          || strdisplaywidth(glyph) != 1 || glyph =~# '[\x00-\x1f\x7f]'
        throw 'Complex Vim popup border characters use the compatibility view'
      endif
    endfor
    chars = supplied
  endif
  var borderStyles: list<dict<any>> = []
  var groups: any = get(options, 'borderhighlight', ['', '', '', ''])
  if type(groups) != v:t_list || len(groups) != 4
    throw 'Complex Vim popup border highlights use the compatibility view'
  endif
  for group in groups
    if type(group) != v:t_string
      throw 'Invalid Vim popup border highlight uses the compatibility view'
    endif
    add(borderStyles, empty(group) ? normal : MergeRenderStyle(normal, RenderStyle(hlID(group))))
  endfor
  var bottom = view.row + view.rows - 1
  var right = view.column + view.columns - 1
  for side in [0, 2]
    if border[side] != 0
      var row = side == 0 ? view.row : bottom
      CompletionText(runs, repeat(chars[side], view.columns), row, view.column, borderStyles[side])
    endif
  endfor
  for row in range(view.row + border[0], bottom - border[2])
    if border[3] != 0
      CompletionText(runs, chars[3], row, view.column, borderStyles[3])
    endif
    if border[1] != 0
      CompletionText(runs, chars[1], row, right, borderStyles[1])
    endif
  endfor
  for corner in [[0, 3, view.row, view.column, 4], [0, 1, view.row, right, 5],
      [2, 1, bottom, right, 6], [2, 3, bottom, view.column, 7]]
    if border[corner[0]] && border[corner[1]]
      CompletionText(runs, chars[corner[4]], corner[2], corner[3], borderStyles[corner[0]])
    endif
  endfor
  var properties = prop_list(1, {end_lnum: line('$')})
  if len(properties) > 2048
    throw 'Vim popup properties exceed the native paint budget'
  endif
  for property in properties
    if has_key(property, 'text') || property.col <= 0
      throw 'Vim popup virtual text uses the compatibility view'
    endif
    var name = get(property, 'type', '')
    var typeBuffer = get(property, 'type_bufnr', 0)
    property.definition = empty(name) ? {} : (typeBuffer > 0
      ? prop_type_get(name, {bufnr: typeBuffer}) : prop_type_get(name))
  endfor
  for lnum in range(1, line('$'))
    var text = getline(lnum)
    if strlen(text) > 16384 || strdisplaywidth(text) > position.core_width
        || foldclosed(lnum) >= 0 || text =~# '[\x00-\x08\x0a-\x1f\x7f\u0590-\u08ff\u200b-\u200f\u202a-\u202e\u2066-\u2069\ufeff]'
      throw 'Wrapped, clipped or complex Vim popup text uses the compatibility view'
    endif
    var lineProperties = filter(copy(properties), (_, prop) => prop.lnum == lnum)
    if len(lineProperties) > 128
      throw 'Vim popup line properties exceed the native paint budget'
    endif
    var byteColumn = 0
    var column = 0
    for index in range(strchars(text, true))
      var glyph = strcharpart(text, index, 1, true)
      var width = strdisplaywidth(glyph, column)
      if width <= 0 || (width > 2 && glyph !=# "\t")
        throw 'Complex Vim popup glyphs use the compatibility view'
      endif
      var style = RenderStyle(synID(lnum, byteColumn + 1, true))
      var propertyStyle: dict<any> = {}
      var priority = -2147483648
      var combine = true
      for property in lineProperties
        var definition = property.definition
        if byteColumn < property.col - 1 + property.length
            && byteColumn + strlen(glyph) > property.col - 1
            && get(definition, 'priority', 0) > priority && !empty(get(definition, 'highlight', ''))
          propertyStyle = RenderStyle(hlID(definition.highlight))
          priority = get(definition, 'priority', 0)
          combine = !!get(definition, 'combine', true)
        endif
      endfor
      if !empty(propertyStyle)
        style = combine ? MergeRenderStyle(style, propertyStyle) : propertyStyle
      endif
      CompletionText(runs, glyph ==# "\t" ? repeat(' ', width) : glyph,
        position.core_line + lnum - 2, position.core_col + column - 1,
        MergeRenderStyle(normal, style))
      byteColumn += strlen(glyph)
      column += width
    endfor
  endfor
  if len(runs) > 4096
    throw 'Vim popup runs exceed the native paint budget'
  endif
  renderPopupSample = {id: id, zindex: zindex, view: view, default_style: normal, runs: runs}
enddef

def RenderPopups(): list<dict<any>>
  var ids = VisiblePopups()
  if len(ids) > 8
    throw 'Vim popups exceed the native surface budget'
  endif
  var popups: list<dict<any>> = []
  var count = 0
  for id in ids
    renderPopupSample = {}
    win_execute(id, 'noautocmd call ' .. expand('<SID>') .. 'SampleRenderPopup()')
    if empty(renderPopupSample)
      throw 'A Vim popup changed during native sampling'
    endif
    count += len(renderPopupSample.runs)
    if count > 4096
      throw 'Vim popups exceed the native paint budget'
    endif
    add(popups, renderPopupSample)
  endfor
  # Vim paints the older popup above a newer popup at the same zindex.
  return sort(popups, (a, b) => a.zindex == b.zindex ? b.id - a.id : a.zindex - b.zindex)
enddef

def RenderFrame(): dict<any>
  if workbenchActionDepth > 0
    return {event: 'render', supported: false,
      fallback_reason: 'A Vim workbench plugin is using the compatibility view',
      buffer: bufnr(), window: win_getid(), changedtick: b:changedtick, mode: mode(1),
      rows: &lines, columns: &columns, runs: [], completion: v:null, popups: []}
  endif
  var active = win_getid()
  var activeMode = mode(1)
  var activeView = winsaveview()
  var activeBuffer = bufnr()
  renderSamplingWindow = active
  # Sampling the active window directly is essential: win_execute() makes
  # an inactive window Normal temporarily while preserving the caller's
  # Visual mode. This also prevents Visual selections leaking into siblings.
  SampleRenderWindow()
  var frame = renderWindowSample
  frame.windows = []
  if !frame.supported
    return frame
  endif
  var ids = gettabinfo(tabpagenr())[0].windows
  if len(ids) > 16
    frame.supported = false
    frame.fallback_reason = 'This Vim layout exceeds the bounded native window budget'
    frame.runs = []
    return frame
  endif
  var caches: dict<dict<any>> = {}
  var runs: list<dict<any>> = []
  var sampleCommand = 'noautocmd call ' .. expand('<SID>') .. 'SampleRenderWindow()'
  for id in ids
    if id == active
      renderWindowSample = frame
    else
      renderWindowSample = {}
      win_execute(id, sampleCommand)
    endif
    if empty(renderWindowSample) || !renderWindowSample.supported
      frame.supported = false
      frame.fallback_reason = empty(renderWindowSample)
        ? 'A Vim window changed during native sampling'
        : 'Window ' .. id .. ': ' .. renderWindowSample.fallback_reason
      frame.runs = []
      frame.windows = []
      return frame
    endif
    var sample = renderWindowSample
    add(frame.windows, {window: sample.window, buffer: sample.buffer,
      changedtick: sample.changedtick, view: sample.view, cursor: sample.cursor,
      topline: sample.topline, botline: sample.botline,
      textoff: sample.textoff, leftcol: sample.leftcol,
      line_count: sample.line_count, overscan: sample.overscan,
      native_pointer: sample.native_pointer, native_scroll: sample.native_scroll,
      scroll_ack: sample.scroll_ack,
      line_lengths: sample.line_lengths})
    extend(runs, sample.runs)
    caches[string(id)] = renderWindowCaches[string(id)]
  endfor
  renderWindowCaches = caches
  frame.runs = runs
  remove(frame, 'overscan')
  remove(frame, 'line_lengths')
  remove(frame, 'native_pointer')
  remove(frame, 'native_scroll')
  remove(frame, 'scroll_ack')
  if win_getid() != active || bufnr() != activeBuffer || mode(1) !=# activeMode || winsaveview() != activeView
    frame.supported = false
    frame.fallback_reason = 'Vim changed its active view during native sampling'
    frame.runs = []
    frame.windows = []
  endif
  if frame.supported
    try
      if pumvisible()
        frame.completion = RenderCompletion()
      endif
      if renderProtocol >= 6
        frame.popups = RenderPopups()
      endif
    catch
      frame.supported = false
      frame.fallback_reason = v:exception
      frame.runs = []
      frame.windows = []
      frame.completion = v:null
      frame.popups = []
    endtry
  endif
  return frame
enddef

def RenderLayoutKey(frame: dict<any>): string
  var windows = map(copy(get(frame, 'windows', [])), (_, item) => [item.window, item.buffer, item.view])
  return string([frame.rows, frame.columns, frame.window, frame.buffer,
    frame.view, frame.default_style, windows])
enddef

def EmitRender(force: bool = false)
  if !renderEnabled || !authenticated || shuttingDown || ch_status(bridge) !=# 'open'
    return
  endif
  # Source-bound scrolling publishes before its response. That paint already
  # consumes the work queued by Changed()/WinScrolled; leaving the 16ms timer
  # alive would sample and compose the same viewport again after every ack.
  if renderTimer >= 0
    timer_stop(renderTimer)
    renderTimer = -1
  endif
  try
    var frame = RenderFrame()
    var encoded = json_encode(frame)
    if strlen(encoded) > 900000
      for window in get(frame, 'windows', [])
        window.overscan = []
      endfor
      encoded = json_encode(frame)
    endif
    if strlen(encoded) > 900000
      frame.supported = false
      frame.fallback_reason = 'Native paint data exceeds the bounded frame budget'
      frame.runs = []
      frame.completion = v:null
      frame.popups = []
      encoded = json_encode(frame)
    endif
    if force || encoded !=# previousRender
      previousRender = encoded
      var baseRevision = renderRevision
      renderRevision += 1
      frame.revision = renderRevision
      var layout = frame.supported ? RenderLayoutKey(frame) : ''
      var rowRuns: dict<list<dict<any>>> = {}
      for run in frame.runs
        var key = string(run.row)
        if !has_key(rowRuns, key)
          rowRuns[key] = []
        endif
        add(rowRuns[key], run)
      endfor
      var rows: dict<string> = {}
      var updates: list<dict<any>> = []
      # Replace complete screen rows, preserving within-row paint order.
      # This includes siblings in a vertical split and explicit empty clears.
      for row in range(frame.rows)
        var key = string(row)
        var runs = get(rowRuns, key, [])
        var rowData = json_encode(runs)
        if !empty(runs)
          rows[key] = rowData
        endif
        if rowData !=# get(previousRenderRows, key, '[]')
          add(updates, {row: row, runs: runs})
        endif
      endfor
      var sentDelta = false
      if renderProtocol >= 2 && !force && frame.supported && layout ==# previousRenderLayout
          && renderRevision - renderKeyframeRevision < 120
        var delta = copy(frame)
        remove(delta, 'runs')
        delta.event = 'render_delta'
        delta.base_revision = baseRevision
        delta.row_updates = updates
        # Scrolling a whole viewport can be cheaper as a complete keyframe.
        if strlen(json_encode(delta)) < strlen(encoded)
          Send(delta)
          sentDelta = true
        endif
      endif
      if !sentDelta
        Send(frame)
        renderKeyframeRevision = renderRevision
      endif
      previousRenderLayout = layout
      previousRenderRows = rows
    endif
  catch
    # Rendering must never break actual editing or disconnect metadata/actions.
    renderBaseKey = ''
    previousRender = ''
    previousRenderLayout = ''
    previousRenderRows = {}
    renderRevision += 1
    Send({event: 'render', revision: renderRevision, supported: false,
      rows: &lines, columns: &columns, completion: v:null, popups: [],
      fallback_reason: 'Native paint unavailable: ' .. strcharpart(v:exception, 0, 512)})
  endtry
enddef

def FlushRender(_: number)
  renderTimer = -1
  EmitRender()
enddef

def RenderChanged()
  if renderEnabled && authenticated && !shuttingDown && renderTimer < 0
    renderTimer = timer_start(16, FlushRender)
  endif
enddef

def RenderThemeChanged()
  # :syntax clear/match can change syntax without changing b:changedtick,
  # &syntax or any highlight group. A command-line transition is therefore
  # a semantic cache boundary, including commands executed in another window.
  if get(v:event, 'old_mode', '') =~# '^c' || get(v:event, 'new_mode', '') =~# '^c'
    renderWindowCaches = {}
  endif
  RefreshRenderTheme()
  RenderChanged()
enddef

def InvalidateRender()
  renderWindowCaches = {}
  renderBaseKey = ''
  renderBase = []
  renderLineGeneration = ''
  renderLineCache = {}
  RefreshRenderTheme()
  RenderChanged()
enddef

def Kind(value: string): string
  var kind = toupper(value)
  if kind ==# 'W'
    return 'warning'
  elseif kind ==# 'I'
    return 'info'
  elseif kind ==# 'N' || kind ==# 'H'
    return 'hint'
  endif
  return 'error'
enddef

# LSP's default character unit is UTF-16; Vim cursor columns are UTF-8 bytes.
# This avoids depending on the newer three-argument byteidx() API.
def Utf16ByteColumn(text: string, units: number): number
  var index = 0
  var consumed = 0
  var bytes = 0
  while index < strchars(text) && consumed < units
    var point = strgetchar(text, index)
    var width = point > 0xffff ? 2 : 1
    if consumed + width > units
      break
    endif
    consumed += width
    bytes += strlen(nr2char(point))
    index += 1
  endwhile
  return bytes + 1
enddef

def WorkbenchDiagnostics(): dict<any>
  var result: dict<any> = {items: [], total: 0}
  if !exists('*g:VimrcWorkbenchDiagnostics') || empty(expand('%:p'))
    return result
  endif
  try
    var provided: any = call('g:VimrcWorkbenchDiagnostics', [])
    if type(provided) != v:t_dict || type(get(provided, 'items', 0)) != v:t_list
      return result
    endif
    # Only current-buffer positions and display text may cross this boundary.
    # The bridge owns the filename; hooks cannot publish another file's cache
    # or serialize arbitrary plugin state, callbacks or recursive dictionaries.
    var entries: list<any> = provided.items
    for entry in entries[: 255]
      if type(entry) != v:t_dict
        continue
      endif
      var lnum: any = get(entry, 'line', 0)
      var column: any = get(entry, 'column', 0)
      var kind: any = get(entry, 'kind', '')
      if type(lnum) != v:t_number || lnum <= 0
          || type(column) != v:t_number || column <= 0
          || type(kind) != v:t_string || index(['error', 'warning', 'info', 'hint'], kind) < 0
          || type(get(entry, 'text', 0)) != v:t_string
        continue
      endif
      add(result.items, {line: lnum, column: column, kind: kind,
        text: WorkbenchText(entry, 'text', 2048)})
    endfor
    # Invalid sampled entries are ignored. Uninspected/omitted provider rows
    # still count against the shared diagnostic budget, with a bounded total.
    result.total = len(result.items) + min([1000000,
      max([len(entries), WorkbenchCount(provided, 'total')])]) - min([256, len(entries)])
  catch
    # A missing/reloading provider must also expire its previous cached rows.
    return {items: [], total: 0}
  endtry
  return result
enddef

def Diagnostics(): dict<any>
  var workbench = WorkbenchDiagnostics()
  var keyNow = string([getqflist({id: 0, changedtick: 0}),
    getloclist(0, {id: 0, changedtick: 0}), bufnr(), b:changedtick, lspDiagnosticsTick, workbench])
  if keyNow ==# diagnosticsKey
    return diagnosticsCache
  endif
  var result: list<dict<any>> = []
  var seen: dict<bool> = {}
  var total = 0
  var entries: list<dict<any>> = getqflist()
  # A Vim LSP or other plugin may use the current window's location list.
  extend(entries, getloclist(0))
  for entry in entries
    if !get(entry, 'valid', 0)
      continue
    endif
    var number: number = get(entry, 'bufnr', 0)
    var bufferInfo = number > 0 ? getbufinfo(number) : []
    var filename: string = empty(bufferInfo) ? '' : bufferInfo[0].name
    var lineNumber: number = max([1, get(entry, 'lnum', 1)])
    var column: number = max([1, get(entry, 'col', 1)])
    if get(entry, 'vcol', 0)
      var windows = number > 0 ? win_findbuf(number) : []
      # Convert using Vim's own rendering rules, including tabs and wide text.
      # An undisplayed buffer has no rendering context; navigate to its line
      # beginning until the user opens it, rather than sending a wrong byte.
      column = empty(windows) ? 1 : max([1, virtcol2col(windows[0], lineNumber, column)])
    endif
    var message: string = strcharpart(get(entry, 'text', ''), 0, 2048)
    var key = string([filename, lineNumber, column, message])
    if has_key(seen, key)
      continue
    endif
    seen[key] = true
    total += 1
    if len(result) < 256
      add(result, {filename: filename, line: lineNumber, column: column,
        text: message, kind: Kind(get(entry, 'type', '')), column_kind: 'byte'})
    endif
  endfor
  # Yegappan's Vim9 LSP exposes diagnostics without opening its location list.
  # The hook is optional; an unavailable or failing plugin must not stop Vim.
  if exists('*lsp#diag#GetDiagsForBuf')
    try
      var filename = expand('%:p')
      var lspEntries: list<dict<any>> = call('lsp#diag#GetDiagsForBuf', [bufnr()])
      for entry in lspEntries
        var position: dict<any> = get(get(entry, 'range', {}), 'start', {})
        var lineNumber: number = get(position, 'line', 0) + 1
        var lines = getbufline(bufnr(), lineNumber)
        var column = Utf16ByteColumn(empty(lines) ? '' : lines[0], get(position, 'character', 0))
        var message: string = strcharpart(get(entry, 'message', ''), 0, 2048)
        var key = string([filename, lineNumber, column, message])
        if has_key(seen, key)
          continue
        endif
        seen[key] = true
        total += 1
        if len(result) < 256
          var severity: number = get(entry, 'severity', 1)
          add(result, {filename: filename, line: lineNumber, column: column,
            text: message, kind: get(['error', 'warning', 'info', 'hint'], severity - 1, 'error')})
        endif
      endfor
    catch
      # Plugins own their initialization and server lifecycle.
    endtry
  endif
  # Simple workbench diagnostics do not create/replace a quickfix or location
  # list. All four hosts consume the same existing native problem-row schema.
  var filename = expand('%:p')
  for entry in workbench.items
    var key = string([filename, entry.line, entry.column, entry.text])
    if has_key(seen, key)
      continue
    endif
    seen[key] = true
    total += 1
    if len(result) < 256
      add(result, {filename: filename, line: entry.line, column: entry.column,
        text: entry.text, kind: entry.kind, column_kind: 'byte'})
    endif
  endfor
  total += workbench.total - len(workbench.items)
  diagnosticsKey = keyNow
  diagnosticsCache = {items: result, total: total}
  return diagnosticsCache
enddef

def Navigation(): dict<bool>
  # Vim owns this bounded, per-window history, including plugin and keyboard
  # jumps. The native controls never maintain a second stale cursor stack.
  var [jumps, current] = getjumplist()
  var back = false
  var forward = false
  for index in range(len(jumps))
    var entry: dict<any> = jumps[index]
    if get(entry, 'lnum', 0) <= 0
        || (get(entry, 'bufnr', 0) == bufnr() && entry.lnum == line('.')
          && get(entry, 'col', 0) == col('.') - 1)
      continue
    endif
    back = back || index < current
    forward = forward || index > current
  endfor
  return {can_go_back: back, can_go_forward: forward}
enddef

def State(): dict<any>
  var infos = getbufinfo({buflisted: 1})
  # Keep the selected tab discoverable even when the list reaches its cap.
  # Preserve Vim's buffer ordering, replacing only the final retained entry.
  if len(infos) > 256
    var current = filter(copy(infos), (_, info) => info.bufnr == bufnr())
    if !empty(current) && index(mapnew(infos[0 : 255], (_, info) => info.bufnr), bufnr()) < 0
      infos[255] = current[0]
    endif
  endif
  var buffers: list<dict<any>> = []
  for info in infos
    if len(buffers) >= 256
      break
    endif
    # Never serialize getbufinfo() wholesale: variables may contain Funcrefs,
    # channels, cyclic objects or private data owned by a user's plugins.
    add(buffers, {id: info.bufnr, name: info.name, modified: info.changed != 0,
      current: info.bufnr == bufnr()})
  endfor
  var diagnostics = Diagnostics()
  var quickfix = getqflist({context: 0, title: 0, idx: 0, size: 0})
  var context: any = get(quickfix, 'context', {})
  var source = 'quickfix'
  var searchOmitted = 0
  var owned: any = type(context) == v:t_dict ? get(context, 'jterm_vim9', false) : false
  if type(owned) == v:t_bool && owned
    var origin: any = get(context, 'source', '')
    if type(origin) == v:t_string && index(['search', 'build'], origin) >= 0
      source = origin
    endif
    var omitted: any = get(context, 'omitted', 0)
    if source ==# 'search' && type(omitted) == v:t_number
      searchOmitted = max([0, omitted])
    endif
  endif
  var state: dict<any> = {event: 'state', mode: mode(1), file: expand('%:p'),
    buffer: bufnr(), window: win_getid(),
    root: projectRoot, cwd: getcwd(), line: line('.'), column: col('.'),
    line_count: line('$'), window_count: winnr('$'), waiting_for_input: !empty(deferredSave),
    modified: &modified, changedtick: b:changedtick, filetype: &filetype,
    encoding: empty(&fileencoding) ? &encoding : &fileencoding,
    buffers: buffers, buffers_truncated: len(infos) > len(buffers),
    buffers_omitted: len(infos) - len(buffers),
    diagnostics: copy(diagnostics.items),
    diagnostics_truncated: diagnostics.total > len(diagnostics.items) || searchOmitted > 0,
    diagnostics_omitted: diagnostics.total - len(diagnostics.items) + searchOmitted,
    navigation: Navigation(),
    quickfix: {source: source, title: strcharpart(get(quickfix, 'title', ''), 0, 2048),
      index: get(quickfix, 'idx', 0), count: get(quickfix, 'size', 0)},
    capabilities: Capabilities(),
    workbench: WorkbenchState(),
    build: {running: buildRunning, command: buildCommand, exit_code: buildExit,
      cancelled: buildCancelled, output: copy(buildTail),
      truncated: len(buildLines) + buildDropped - len(buildTail)},
    search: {running: searchRunning, query: searchQuery, count: len(searchRows),
      truncated: searchDropped, exit_code: searchExit, errors: searchErrors,
      options: searchOptions, cancelled: searchCancelled}}
  # Leave headroom inside the host's 1 MiB framing limit even for pathological
  # path names or error messages that require substantial JSON escaping.
  while strlen(json_encode(state)) > 900000
    if !empty(state.diagnostics)
      remove(state.diagnostics, -1)
      state.diagnostics_truncated = true
      state.diagnostics_omitted += 1
    elseif !empty(state.buffers)
      var discard = len(state.buffers) - 1
      # Prefer retaining the current tab, but the wire budget remains a hard
      # bound. A synthetic buffer name can itself occupy most of the frame.
      if discard > 0 && state.buffers[discard].current
        discard -= 1
      endif
      remove(state.buffers, discard)
      state.buffers_truncated = true
      state.buffers_omitted += 1
    else
      state.build.truncated += len(state.build.output)
      state.build.output = []
      break
    endif
  endwhile
  return state
enddef

def EmitState(force: bool = false)
  if !authenticated || shuttingDown || sending || ch_status(bridge) !=# 'open'
    return
  endif
  sending = true
  try
    var snapshot = State()
    var encoded = json_encode(snapshot)
    if force || encoded !=# previousState
      previousState = encoded
      Send(snapshot)
    endif
  finally
    sending = false
  endtry
enddef

def Flush(_: number)
  flushTimer = -1
  EmitState()
enddef

def Changed()
  RenderChanged()
  if authenticated && !shuttingDown && flushTimer < 0
    flushTimer = timer_start(40, Flush)
  endif
enddef

def Poll(_: number)
  # setqflist() itself has no QuickFixCmdPost event.  This bounded, low-rate
  # reconciliation also observes plugin capabilities loaded after VimEnter.
  EmitState()
  RefreshRenderTheme()
  EmitRender()
enddef

def LspDiagnosticsChanged()
  lspDiagnosticsTick += 1
  Changed()
enddef

def RememberView()
  if NativeTextBuffer(&buftype)
    bufferViews[string(bufnr())] = winsaveview()
  endif
enddef

def RestoreView(number: number)
  if has_key(bufferViews, string(number))
    winrestview(bufferViews[string(number)])
  endif
enddef

def ForgetView(number: number)
  if has_key(bufferViews, string(number))
    remove(bufferViews, string(number))
  endif
enddef

def SafeToChange()
  var currentMode = mode(1)
  if workbenchActionDepth > 0 || currentMode =~# '^no' || currentMode =~# '^[cr!t]'
      || !empty(state('moaSw'))
    throw 'Vim is busy with a command, operator or completion; finish it before this action'
  endif
enddef

def ClearDeferredSave()
  deferredSave = {}
  deferredSaveBuffer = 0
  if deferredSaveTimer >= 0
    timer_stop(deferredSaveTimer)
    deferredSaveTimer = -1
  endif
  augroup JTermVim9DeferredSave
    autocmd!
  augroup END
enddef

def ExpireDeferredSave(_: number)
  var request = deferredSave
  ClearDeferredSave()
  if !empty(request)
    Reply(request.id, false, 'Vim is still finishing input; finish the command and save again')
    Changed()
  endif
enddef

def SaveInputPending(): bool
  # getchar(1) can invoke queued channel callbacks before peeking. Preserve
  # this save's ordering while those callbacks run, including SafeStateAgain.
  var pending = false
  checkingSaveInput = true
  try
    pending = !empty(getchar(1))
  finally
    checkingSaveInput = false
  endtry
  return pending
enddef

def ResumeDeferredSave()
  if empty(deferredSave) || !authenticated || shuttingDown || checkingSaveInput
    return
  endif
  var currentMode = mode(1)
  if currentMode =~# '^no' || currentMode =~# '^[cr!t]' || !empty(state('moaSw'))
      || SaveInputPending()
    return
  endif
  # A callback invoked by the peek may have disconnected the bridge.
  if empty(deferredSave) || !authenticated || shuttingDown
    return
  endif
  var request = deferredSave
  var buffer = deferredSaveBuffer
  ClearDeferredSave()
  try
    if bufnr() != buffer
      throw 'The current buffer changed while waiting to save; save again in the intended buffer'
    endif
    Dispatch(request, request.id)
  catch
    Reply(request.id, false, v:exception)
  endtry
  Changed()
enddef

def DeferSaveIfNeeded(request: dict<any>, id: number): bool
  var action = RequiredString(request, 'action', 64)
  if workbenchActionDepth > 0 && index(['snapshot', 'render_resync', 'set_render', 'cancel_build', 'cancel_search'], action) < 0
    throw 'A Vim workbench plugin is waiting for input; finish it before another IDE action'
  endif
  if (!empty(deferredSave) || checkingSaveInput)
      && index(['snapshot', 'cancel_build', 'cancel_search'], action) < 0
    throw 'A save is waiting for Vim to finish input; wait for it before another IDE action'
  endif
  if index(['save', 'save_all', 'save_and_build'], action) < 0
    return false
  endif
  var currentMode = mode(1)
  var busy = state('moaSw')
  # A completed host PTY write can still be waiting in Vim's input stream.
  # Peek without consuming text, keycodes or Escape, and let SafeState resume
  # after Vim handles it. S also occurs while queued Escape is being decoded;
  # only reject S with no typeahead (an unfinished count/find awaiting a key).
  # Operators, command lines and completion remain immediate refusals.
  if currentMode =~# '^no' || currentMode =~# '^[cr!t]' || busy =~# '[oa]'
    return false
  endif
  var buffer = bufnr()
  var pendingInput = SaveInputPending()
  if !authenticated || shuttingDown
    throw 'Vim disconnected while checking input for this save'
  endif
  if bufnr() != buffer
    throw 'The current buffer changed while checking input; save again in the intended buffer'
  endif
  if !pendingInput && (empty(busy) || busy =~# 'S')
    return false
  endif
  if action ==# 'save_and_build'
    BuildCommand(request)
  endif
  deferredSave = copy(request)
  deferredSave.id = id
  deferredSaveBuffer = buffer
  deferredSaveTimer = timer_start(2000, ExpireDeferredSave)
  augroup JTermVim9DeferredSave
    autocmd!
    autocmd SafeState,SafeStateAgain * ResumeDeferredSave()
  augroup END
  Changed()
  return true
enddef

def RequiredString(request: dict<any>, key: string, limit: number = 16384): string
  var value: any = get(request, key, '')
  if type(value) != v:t_string || empty(value) || strlen(value) > limit
    throw 'A non-empty ' .. key .. ' string is required'
  endif
  return value
enddef

def PositiveNumber(request: dict<any>, key: string, fallback: number = 1): number
  var value: any = get(request, key, fallback)
  if type(value) != v:t_number || value < 1 || value > 2147483647
    throw key .. ' must be a positive integer'
  endif
  return value
enddef

def FinishBuild(generation: number)
  if generation != buildGeneration || !buildClosed || !buildExited
    return
  endif
  buildRunning = false
  # Seed Vim's errorformat directory stack so paths emitted by the build stay
  # relative to its cwd even if the user changed Vim's cwd while it was running.
  var directoryLine = 'JTERM_DIRECTORY ' .. projectRoot
  var parsed = getqflist({lines: [directoryLine] + buildLines,
    efm: '%DJTERM_DIRECTORY %f,' .. buildErrorFormat}).items
  # Vim retains directory markers as invalid quickfix rows even with %-D.
  # Remove only our seed after parsing; preserve the user's remaining output.
  if !empty(parsed) && !get(parsed[0], 'valid', 0)
      && get(parsed[0], 'text', '') ==# directoryLine
    remove(parsed, 0)
  endif
  setqflist([], ' ', {title: 'Build: ' .. buildCommand, items: parsed,
    context: {jterm_vim9: true, source: 'build'}})
  Changed()
enddef

def BuildOutput(generation: number, _: channel, message: string)
  if generation != buildGeneration
    return
  endif
  var text = strcharpart(message, 0, 2048)
  add(buildTail, text)
  if len(buildTail) > 80
    remove(buildTail, 0)
  endif
  if len(buildLines) < 10000
    add(buildLines, text)
  else
    buildDropped += 1
  endif
  Changed()
enddef

def BuildClosed(generation: number, _: channel)
  if generation == buildGeneration
    buildClosed = true
    FinishBuild(generation)
  endif
enddef

def BuildExited(generation: number, _: job, status: number)
  if generation == buildGeneration
    buildExited = true
    buildExit = status
    FinishBuild(generation)
  endif
enddef

def BuildCommand(request: dict<any>): string
  if buildRunning
    throw 'A build is already running; cancel it before starting another'
  endif
  var supplied: any = get(request, 'command', '')
  if type(supplied) != v:t_string || strlen(supplied) > 16384
    throw 'Build command must be a string of at most 16384 bytes'
  endif
  var command: string = empty(trim(supplied)) ? expandcmd(&makeprg) : supplied
  if empty(trim(command))
    throw 'Configure makeprg in Vim or enter a build command'
  endif
  return command
enddef

def StartBuild(request: dict<any>)
  var command = BuildCommand(request)
  buildGeneration += 1
  const generation = buildGeneration
  buildCommand = command
  buildLines = []
  buildTail = []
  buildDropped = 0
  buildExit = v:null
  buildClosed = false
  buildExited = false
  buildCancelled = false
  buildRunning = true
  buildErrorFormat = &errorformat
  # A build is the one explicitly authorized shell action.  File names and
  # search queries never take this path.  No implicit save is performed.
  buildJob = job_start([&shell, &shellcmdflag, command], {cwd: projectRoot,
    in_io: 'null', out_mode: 'nl', err_mode: 'nl', noblock: true,
    out_cb: (ch: channel, msg: string) => BuildOutput(generation, ch, msg),
    err_cb: (ch: channel, msg: string) => BuildOutput(generation, ch, msg),
    close_cb: (ch: channel) => BuildClosed(generation, ch),
    exit_cb: (process: job, status: number) => BuildExited(generation, process, status),
    stoponexit: 'term'})
  if job_status(buildJob) ==# 'fail'
    buildRunning = false
    buildExit = -1
    throw 'Vim could not start the build process'
  endif
  Changed()
enddef

def FinishSearch(generation: number)
  if generation != searchGeneration || !searchClosed || !searchExited
    return
  endif
  searchRunning = false
  if searchExit >= 0 && searchExit <= 1
    setqflist([], ' ', {title: 'Search: ' .. searchQuery, items: searchRows,
      context: {jterm_vim9: true, source: 'search', omitted: searchDropped}})
  else
    Send({event: 'error', error: 'Project search failed (exit ' .. searchExit .. '): '
      .. (empty(searchErrors) ? 'ripgrep returned an error' : join(searchErrors, ' '))})
  endif
  Changed()
enddef

def SearchOutput(generation: number, _: channel, message: string)
  if generation != searchGeneration
    return
  endif
  try
    var entry: dict<any> = json_decode(message)
    if get(entry, 'type', '') !=# 'match'
      return
    endif
    if len(searchRows) >= 256
      searchDropped += 1
      return
    endif
    var data: dict<any> = get(entry, 'data', {})
    var filename: string = get(get(data, 'path', {}), 'text', '')
    if empty(filename)
      searchDropped += 1
      return
    endif
    if filename !~# '^/'
      filename = fnamemodify(searchRoot .. '/' .. filename, ':p')
    endif
    var matches: list<dict<any>> = get(data, 'submatches', [])
    var column: number = empty(matches) ? 1 : get(matches[0], 'start', 0) + 1
    var text: string = get(get(data, 'lines', {}), 'text', '')
    add(searchRows, {filename: filename, lnum: get(data, 'line_number', 1),
      col: column, text: strcharpart(substitute(text, '[\r\n]\+$', '', ''), 0, 2048), type: 'I'})
  catch
    if len(searchErrors) < 8
      add(searchErrors, 'Could not decode an rg result: ' .. strcharpart(v:exception, 0, 256))
    endif
  endtry
enddef

def SearchError(generation: number, _: channel, message: string)
  if generation == searchGeneration && len(searchErrors) < 8
    add(searchErrors, strcharpart(message, 0, 1024))
  endif
enddef

def SearchClosed(generation: number, _: channel)
  if generation == searchGeneration
    searchClosed = true
    FinishSearch(generation)
  endif
enddef

def SearchExited(generation: number, _: job, status: number)
  if generation == searchGeneration
    searchExited = true
    searchExit = status
    FinishSearch(generation)
  endif
enddef

def SearchOptions(request: dict<any>): dict<any>
  var options: any = get(request, 'options', {})
  if type(options) != v:t_dict
    throw 'Project search options must be an object'
  endif
  var caseMode: any = get(options, 'case', 'smart')
  var regex: any = get(options, 'regex', false)
  var wholeWord: any = get(options, 'whole_word', false)
  var glob: any = get(options, 'glob', '')
  if type(caseMode) != v:t_string || index(['smart', 'sensitive', 'ignore'], caseMode) < 0
    throw 'Project search case must be smart, sensitive or ignore'
  endif
  if type(regex) != v:t_bool || type(wholeWord) != v:t_bool
    throw 'Project search regex and whole_word options must be Boolean'
  endif
  if type(glob) != v:t_string || strlen(glob) > 4096 || glob =~# '[\r\n]'
    throw 'Project search expects a single-line file glob of at most 4096 bytes'
  endif
  return {case: caseMode, regex: regex, whole_word: wholeWord, glob: glob}
enddef

def CancelSearch()
  if searchRunning
    # Invalidate callbacks before stopping the process. Neither a late success
    # nor partially collected rows can replace the previously visible list.
    searchGeneration += 1
    searchCancelled = true
    searchRunning = false
    searchExit = 130
    job_stop(searchJob)
    Changed()
  endif
enddef

def StartSearch(request: dict<any>)
  var query = RequiredString(request, 'query', 4096)
  var options = SearchOptions(request)
  if !executable('rg')
    throw 'Project search requires ripgrep (rg) on PATH'
  endif
  if query =~# '[\r\n]'
    throw 'Project search expects a single-line query'
  endif
  searchGeneration += 1
  const generation = searchGeneration
  if searchRunning
    job_stop(searchJob)
  endif
  searchQuery = query
  searchRoot = projectRoot
  searchRows = []
  searchErrors = []
  searchOptions = options
  searchCancelled = false
  searchDropped = 0
  searchExit = 0
  searchClosed = false
  searchExited = false
  searchRunning = true
  # Every option and query has its own argv boundary. A user ripgrep config
  # must not change the selected semantics, output framing or spawn a pre-tool.
  # Project ignore files still apply, with .git always excluded.
  var args = ['rg', '--no-config', '--json', '--line-number', '--column', '--color', 'never', '--hidden']
  if !options.regex
    add(args, '--fixed-strings')
  endif
  add(args, get({smart: '--smart-case', sensitive: '--case-sensitive', ignore: '--ignore-case'}, options.case))
  if options.whole_word
    add(args, '--word-regexp')
  endif
  if !empty(options.glob)
    extend(args, ['--glob', options.glob])
  endif
  extend(args, ['--glob', '!.git', '--', query, '.'])
  searchJob = job_start(args,
    {cwd: searchRoot, in_io: 'null', out_mode: 'nl', err_mode: 'nl', noblock: true,
      out_cb: (ch: channel, msg: string) => SearchOutput(generation, ch, msg),
      err_cb: (ch: channel, msg: string) => SearchError(generation, ch, msg),
      close_cb: (ch: channel) => SearchClosed(generation, ch),
      exit_cb: (process: job, status: number) => SearchExited(generation, process, status),
      stoponexit: 'term'})
  if job_status(searchJob) ==# 'fail'
    searchRunning = false
    searchExit = -1
    throw 'Vim could not start ripgrep'
  endif
  Changed()
enddef

def InvokeWorkbench(action: string)
  # Some real providers run getcharstr()/input() inside this channel callback.
  # They can prevent timer/channel callbacks from running until input arrives.
  # Retire the native overlay BEFORE entering that loop, and flush any batching
  # so the PTY popup is visible while the user is choosing. Timer paints must
  # not put the old overlay back during the provider's initial delay either.
  workbenchActionDepth += 1
  try
    EmitRender(true)
    FlushSendBatch()
    call('g:VimrcWorkbenchAction', [action])
  finally
    workbenchActionDepth -= 1
    renderWindowCaches = {}
    EmitRender(true)
    Changed()
  endtry
enddef

def LspAction(action: string, request: dict<any>)
  if index(WorkbenchCapabilities(), action) >= 0
    InvokeWorkbench(action)
    return
  endif
  var commands = {definition: 'LspGotoDefinition', hover: 'LspHover',
    references: 'LspShowReferences', rename: 'LspRename', format: 'LspFormat'}
  var command: string = get(commands, action, '')
  if action ==# 'definition' && exists(':' .. command) != 2
    var word = expand('<cword>')
    if empty(word)
      throw 'There is no symbol under the Vim cursor'
    endif
    execute 'tag ' .. fnameescape(word)
    return
  endif
  if empty(command) || exists(':' .. command) != 2
    throw 'This action needs a configured Vim LSP provider exposing :' .. command
  endif
  if action ==# 'rename'
    var name = RequiredString(request, 'name', 1024)
    if name =~# '[[:space:]|\r\n]'
      throw 'A symbol name must not contain whitespace or command separators'
    endif
    execute command .. ' ' .. fnameescape(name)
  else
    execute command
  endif
enddef

def RunWorkbench(request: dict<any>)
  var command = RequiredString(request, 'command', 64)
  if index(WORKBENCH_ACTIONS, command) < 0
    throw 'Unsupported workbench action: ' .. command
  endif
  var window = PositiveNumber(request, 'window')
  var buffer = PositiveNumber(request, 'buffer')
  var tick: any = get(request, 'changedtick', -1)
  if type(tick) != v:t_number || tick < 0
    throw 'Workbench actions require a valid source changedtick'
  endif
  if window != win_getid() || buffer != bufnr() || tick != b:changedtick
    throw 'The workbench source changed; reopen the command palette'
  endif
  # Recheck after preceding queued requests and all mode/source checks. An
  # action removed by reload must never execute through stale picker metadata.
  if index(WorkbenchCapabilities(), command) < 0
    throw 'This workbench action is unavailable for the current buffer'
  endif
  InvokeWorkbench(command)
enddef

def ScrollWindow(request: dict<any>)
  SafeToChange()
  const window = PositiveNumber(request, 'window')
  const count: any = get(request, 'lines', 0)
  if type(count) != v:t_number || count == 0 || count < -120 || count > 120
    throw 'Scroll requires between 1 and 120 signed lines'
  endif
  # win_id2win() only accepts a window in the active tab. A delayed wheel event
  # must never move a closed split or silently scroll the currently focused one.
  if win_id2win(window) == 0 || !NativeTextBuffer(getwinvar(window, '&buftype'))
    throw 'The requested editor window is no longer available'
  endif
  # Numeric validation makes this a fixed command, not an Ex/eval interface.
  # win_execute preserves focus/cwd and avoids WinEnter/BufEnter side effects.
  const key = count > 0 ? "\<C-E>" : "\<C-Y>"
  win_execute(window, 'noautocmd keepjumps normal! ' .. abs(count) .. key)
  Changed()
enddef

def Dispatch(request: dict<any>, id: number)
  var action = RequiredString(request, 'action', 64)
  if action ==# 'set_render'
    var enabled: any = get(request, 'enabled', v:null)
    if type(enabled) != v:t_bool
      throw 'set_render requires a Boolean enabled value'
    endif
    renderEnabled = enabled
    if renderEnabled
      # Terminal Vim may retain GUI color names instead of resolving fg# to
      # hex. Use Vim's own customizable name table; never a terminal palette.
      runtime! colors/lists/default.vim
      RefreshRenderTheme()
      EmitRender(true)
    else
      previousRender = ''
      previousRenderLayout = ''
      previousRenderRows = {}
      renderBaseKey = ''
      renderBase = []
      renderWindowCaches = {}
      if renderTimer >= 0
        timer_stop(renderTimer)
        renderTimer = -1
      endif
      Send({event: 'render_disabled'})
    endif
  elseif action ==# 'render_resync'
    # Read-only recovery after a missing/invalid host paint baseline. Do not
    # feed input or touch Vim's undo, buffer, mode, selection or view state.
    EmitRender(true)
  elseif action ==# 'snapshot'
    EmitState(true)
    RefreshRenderTheme()
    EmitRender()
  elseif action ==# 'scroll'
    ScrollWindow(request)
  elseif action ==# 'scroll_view'
    ScrollSourceWindow(request, id)
    return  # The ordered response was included with the semantic ack.
  elseif action ==# 'pointer'
    SourcePointer(request)
  elseif action ==# 'build'
    StartBuild(request)
  elseif action ==# 'cancel_build'
    if buildRunning
      buildCancelled = true
      job_stop(buildJob)
    endif
  elseif action ==# 'search' || action ==# 'search_with_options'
    StartSearch(request)
  elseif action ==# 'cancel_search'
    CancelSearch()
  else
    SafeToChange()
    if action ==# 'open'
      var path = RequiredString(request, 'path')
      if path =~# '[\r\n]'
        throw 'Opening filenames containing newlines is not supported'
      endif
      var hasPosition = has_key(request, 'line') || has_key(request, 'column')
      var lineNumber = has_key(request, 'line') ? PositiveNumber(request, 'line') : 0
      var column = has_key(request, 'column') ? PositiveNumber(request, 'column') : 1
      # Use :buffer for an exact existing file, preserving its cursor and undo
      # state.  Avoid bufnr(path)'s pattern fallback for unusual file names.
      # Both commands retain Vim's unsaved-buffer protection and the user's
      # 'hidden', autocommands, swap handling and file hooks.
      var existing = -1
      var absolute = fnamemodify(path, ':p')
      for info in getbufinfo()
        if info.name ==# absolute && getbufvar(info.bufnr, '&buftype') ==# ''
          existing = info.bufnr
          break
        endif
      endfor
      # :buffer records cross-file jumps itself. An explicit location in the
      # already-current file also needs its origin on Vim's own jumplist.
      if existing == bufnr() && hasPosition
          && ((lineNumber > 0 && lineNumber != line('.')) || column != col('.'))
        normal! m'
      endif
      if existing > 0
        var previous = bufnr()
        execute 'buffer ' .. existing
        if !hasPosition && existing != previous
          RestoreView(existing)
        endif
      else
        execute 'edit ' .. fnameescape(path)
      endif
      if hasPosition
        cursor(lineNumber > 0 ? lineNumber : line('.'), column)
      endif
    elseif action ==# 'buffer'
      var number = PositiveNumber(request, 'buffer')
      if !bufexists(number)
        throw 'The requested Vim buffer no longer exists'
      endif
      var previous = bufnr()
      execute 'buffer ' .. number
      if number != previous
        RestoreView(number)
      endif
    elseif action ==# 'goto'
      var lineNumber = PositiveNumber(request, 'line', 0)
      var column = PositiveNumber(request, 'column')
      # Keep this in the current buffer, including unnamed/modified buffers.
      # Vim clamps out-of-range positions and keeps its byte-column convention.
      if lineNumber != line('.') || column != col('.')
        normal! m'
        cursor(lineNumber, column)
      endif
    elseif action ==# 'navigate_back' || action ==# 'navigate_forward'
      var navigation = Navigation()
      var backward = action ==# 'navigate_back'
      if !(backward ? navigation.can_go_back : navigation.can_go_forward)
        throw backward ? 'No earlier position in this Vim window' : 'No later position in this Vim window'
      endif
      # Ctrl-I is a tab byte, which :normal trims as leading whitespace.
      # An explicit count preserves it as a command rather than an empty Ex.
      execute backward ? "normal! 1\<C-O>" : "normal! 1\<C-I>"
    elseif action ==# 'next_buffer' || action ==# 'previous_buffer'
      var previous = bufnr()
      if action ==# 'next_buffer'
        bnext
      else
        bprevious
      endif
      if bufnr() != previous
        RestoreView(bufnr())
      endif
    elseif action ==# 'close_buffer' || action ==# 'close_current_buffer'
      # UI snapshots can lag queued navigation. Keyboard close must resolve
      # the target here; an explicit tab close retains its requested identity.
      var number = action ==# 'close_current_buffer' ? bufnr() : PositiveNumber(request, 'buffer', 0)
      if !bufexists(number) || !buflisted(number)
        throw 'The requested Vim buffer is no longer open'
      endif
      # Refuse modified buffers even when the user's hidden option is enabled.
      # Native close never means discard, and must not silently save either.
      if getbufvar(number, '&modified')
        throw 'Unsaved changes: save this buffer before closing it'
      endif
      execute 'bdelete ' .. number
    elseif action ==# 'split'
      var vertical: any = get(request, 'vertical', v:null)
      if type(vertical) != v:t_bool
        throw 'split requires a Boolean vertical value'
      endif
      if vertical
        vsplit
      else
        split
      endif
    elseif action ==# 'save'
      update
    elseif action ==# 'save_all'
      wall
    elseif action ==# 'save_and_build'
      # Validate before any write. :wall errors (including readonly/unnamed
      # buffers and failing write hooks) prevent StartBuild from being reached.
      var command = BuildCommand(request)
      wall
      if !empty(filter(getbufinfo({buflisted: 1}), (_, info) => info.changed != 0))
        throw 'Some buffers remain modified; save them before building'
      endif
      StartBuild({command: command})
    elseif action ==# 'next_diagnostic'
      cnext
    elseif action ==# 'previous_diagnostic'
      cprevious
    elseif action ==# 'workbench'
      RunWorkbench(request)
    elseif index(['definition', 'hover', 'references', 'rename', 'format'], action) >= 0
      LspAction(action, request)
    elseif action ==# 'quit'
      pendingQuit = id
      qall
      return
    else
      throw 'Unsupported IDE action: ' .. action
    endif
    Changed()
  endif
  Reply(id, true)
enddef

def Receive(_: channel, message: string)
  var id = 0
  try
    if strlen(message) > 65536
      throw 'IDE request exceeds the 64 KiB limit'
    endif
    var request: dict<any> = json_decode(message)
    var requestId: any = get(request, 'id', 0)
    if type(requestId) != v:t_number || requestId < 0
      throw 'IDE request id must be a nonnegative integer'
    endif
    id = requestId
    if !authenticated
      if get(request, 'action', '') !=# 'auth'
        throw 'IDE authentication is required'
      endif
      var suppliedToken: any = get(request, 'token', '')
      var suppliedProtocol: any = get(request, 'protocol', 0)
      if type(suppliedToken) != v:t_string || suppliedToken !=# authToken
          || type(suppliedProtocol) != v:t_number || suppliedProtocol != 1
        Reply(id, false, 'IDE authentication failed')
        ch_close(bridge)
        return
      endif
      var requestedRender: any = get(request, 'render_protocol', 1)
      if type(requestedRender) != v:t_number || index([1, 2, 3, 4, 5, 6], requestedRender) < 0
        throw 'Unsupported native render protocol'
      endif
      renderProtocol = requestedRender
      authenticated = true
      authToken = ''
      WorkbenchLifecycle(true)
      pollTimer = timer_start(750, Poll, {repeat: -1})
      EmitState(true)
      return
    endif
    if !DeferSaveIfNeeded(request, id)
      Dispatch(request, id)
    endif
  catch
    pendingQuit = 0
    Reply(id, false, v:exception)
  endtry
enddef

def Disconnected(_: channel)
  var wasConnected = authenticated
  authenticated = false
  if wasConnected
    WorkbenchLifecycle(false)
  endif
  ClearDeferredSave()
  if flushTimer >= 0
    timer_stop(flushTimer)
    flushTimer = -1
  endif
  if pollTimer >= 0
    timer_stop(pollTimer)
    pollTimer = -1
  endif
  if renderTimer >= 0
    timer_stop(renderTimer)
    renderTimer = -1
  endif
enddef

def Leaving()
  if authenticated
    WorkbenchLifecycle(false)
  endif
  ClearDeferredSave()
  if pendingQuit > 0
    Reply(pendingQuit, true)
  endif
  Send({event: 'bye'})
  # A nested shutdown must not strand a scroll's already-encoded messages.
  FlushSendBatch()
  shuttingDown = true
  if buildRunning
    job_stop(buildJob)
  endif
  if searchRunning
    job_stop(searchJob)
  endif
enddef

augroup JTermVim9Bridge
  autocmd!
  autocmd BufEnter,BufAdd,BufDelete,BufWritePost,FileType,WinEnter,TabEnter,DirChanged * Changed()
  autocmd BufLeave * RememberView()
  autocmd BufWipeout * ForgetView(str2nr(expand('<abuf>')))
  autocmd TextChanged,TextChangedI,TextChangedP,CursorMoved,CursorMovedI,ModeChanged * Changed()
  autocmd ModeChanged,ColorScheme * RenderThemeChanged()
  autocmd Syntax,FileType * InvalidateRender()
  # Stock Vim has no universal event for a plugin changing :syntax rules
  # asynchronously. Providers can explicitly invalidate after such updates.
  autocmd User JTermVim9RenderInvalidate InvalidateRender()
  autocmd User JTermVim9PopupChanged RenderChanged()
  autocmd WinScrolled,VimResized,OptionSet * RenderChanged()
  autocmd CompleteChanged,CompleteDone * RenderChanged()
  autocmd QuickFixCmdPost * Changed()
  autocmd User LspDiagsUpdated,LspAttached,LspDetached,SimpleCCDiagnosticsChanged,VimrcWorkbenchChanged LspDiagnosticsChanged()
  autocmd User SimpleGitUpdate,SimpleRemoteConnected,SimpleRemoteDisconnected,SimpleRemoteWorkspaceChanged Changed()
  autocmd VimLeavePre * Leaving()
augroup END

defcompile
bridge = ch_open(address, {mode: 'nl', callback: Receive, close_cb: Disconnected,
  waittime: 250, timeout: 0, noblock: true})
if ch_status(bridge) ==# 'open'
  Send({event: 'hello', token: authToken, protocol: 1, version: v:version,
    vim9: has('vim9script') != 0, pid: getpid()})
endif
