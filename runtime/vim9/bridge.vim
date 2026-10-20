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

var bridge: channel
var authenticated = false
var shuttingDown = false
var sending = false
var sequence = 0
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
var renderEnabled = false
var renderTimer = -1
var renderRevision = 0
var previousRender = ''
var renderBaseKey = ''
var renderBase: list<dict<any>> = []
var renderStyles: dict<dict<any>> = {}
var renderThemeKey = ''
var renderNormal: dict<any> = {}

def Send(payload: dict<any>)
  if ch_status(bridge) !=# 'open'
    return
  endif
  sequence += 1
  payload.protocol = 1
  payload.seq = sequence
  ch_sendraw(bridge, json_encode(payload) .. "\n")
enddef

def Reply(id: number, ok: bool, error: string = '')
  Send({event: 'response', id: id, ok: ok, error: strcharpart(error, 0, 2048)})
enddef

def Capabilities(): list<string>
  var result = ['open', 'save', 'save_all', 'buffer', 'build', 'cancel_build',
    'goto', 'close_buffer', 'close_current_buffer', 'next_buffer', 'previous_buffer', 'split', 'save_and_build',
    'navigate_back', 'navigate_forward', 'cancel_search',
    'snapshot', 'set_render', 'next_diagnostic', 'previous_diagnostic', 'quit']
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
  return result
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

def RenderFallback(): string
  if winnr('$') != 1
    return 'Vim split windows use the compatibility view'
  elseif &buftype !=# ''
    return 'This special Vim buffer uses the compatibility view'
  elseif mode(1) !~# '^[nivVR\x16]'
    return 'Vim command, prompt or terminal input uses the compatibility view'
  elseif pumvisible() || !empty(filter(popup_list(), (_, id) => get(popup_getpos(id), 'visible', 0)))
    return 'Vim completion and popup windows use the compatibility view'
  elseif &diff || &rightleft || &arabic || &spell || &list || &conceallevel > 0 || &cursorcolumn
    return 'This Vim display option needs the compatibility view'
  elseif get(winsaveview(), 'coladd', 0) > 0
      || (index(['v', 'V', "\<C-V>"], mode(1)) >= 0 && getpos('v')[3] > 0)
    return 'Vim virtual-space cursor and selections use the compatibility view'
  elseif &foldcolumn != 0
    return 'Vim fold columns use the compatibility view'
  elseif !empty(getmatches())
    return 'Vim match decorations use the compatibility view'
  elseif &hlsearch && v:hlsearch && @/ =~# '\\[_%zn]'
    return 'Multiline and positional Vim searches use the compatibility view'
  endif
  return ''
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
  while offset <= strlen(text) && len(ranges) < 512
    var found = matchstrpos(text, pattern, offset)
    if found[1] < 0
      break
    endif
    if found[1] == found[2]
      throw 'Zero-width Vim search highlights use the compatibility view'
    endif
    add(ranges, [found[1], max([found[1] + 1, found[2]])])
    offset = found[2] > found[1] ? found[2] : found[1] + 1
  endwhile
  return ranges
enddef

def AddRenderRun(runs: list<dict<any>>, glyph: dict<any>, style: dict<any>)
  var run = copy(glyph)
  run.style = style
  # ASCII shaping may be batched. A non-ASCII grapheme always keeps its own
  # Vim-owned display advance, preventing fallback fonts from moving neighbors.
  var ascii = strlen(run.text) == strchars(run.text) && run.text =~# '^[\x20-\x7e]*$'
  if !empty(runs) && ascii
    var previous = runs[-1]
    if previous.row == run.row && previous.column + previous.cells == run.column
        && previous.line == run.line && previous.byte_end == run.byte_start
        && previous.style == style && strlen(previous.text) == strchars(previous.text)
        && previous.text =~# '^[\x20-\x7e]*$'
      previous.text ..= run.text
      previous.cells += run.cells
      previous.byte_end = run.byte_end
      return
    endif
  endif
  remove(run, 'virtual_column')
  if has_key(run, 'override_style')
    remove(run, 'override_style')
  endif
  if has_key(run, 'plain')
    remove(run, 'plain')
  endif
  add(runs, run)
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

def RenderFrame(): dict<any>
  var info = getwininfo(win_getid())[0]
  var view = {row: info.winrow - 1, column: info.wincol - 1,
    rows: info.height, columns: info.width}
  var frame: dict<any> = {event: 'render', supported: true, fallback_reason: '',
    rows: &lines, columns: &columns, window: win_getid(), buffer: bufnr(),
    changedtick: b:changedtick, mode: mode(1), view: view,
    default_style: renderNormal, cursor: {row: 0, column: 0, width: 1, visible: false}, runs: []}
  var reason = RenderFallback()
  if !empty(reason)
    frame.supported = false
    frame.fallback_reason = reason
    return frame
  endif
  var savedView = winsaveview()
  var first = line('w0')
  var last = line('w$')
  if view.rows > 200 || view.columns > 400 || last - first > 200
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
  var baseKey = string([bufnr(), b:changedtick, first, last, savedView.leftcol,
    view, info.textoff, &tabstop, &vartabstop, &wrap, &syntax, &synmaxcol,
    &number, &relativenumber, &numberwidth, &ambiwidth, renderThemeKey,
    &hlsearch, v:hlsearch, @/, &ignorecase, &smartcase, properties])
  if baseKey !=# renderBaseKey
    renderBase = []
    for lineNumber in range(first, last)
      var text = getline(lineNumber)
      if foldclosed(lineNumber) >= 0
        reason = 'Closed Vim folds use the compatibility view'
        break
      elseif strlen(text) > 16384 || strchars(text) > 4096
        reason = 'This long line exceeds the bounded native paint budget'
        break
      elseif &wrap && strdisplaywidth(text) > textWidth
        reason = 'Wrapped Vim continuation rows use the compatibility view'
        break
      elseif text =~# '[\x00-\x08\x0a-\x1f\x7f\u0590-\u08ff\u200b-\u200f\u202a-\u202e\u2066-\u2069\ufeff]'
        reason = 'Control and bidirectional text use the compatibility view'
        break
      endif
      var row = view.row + lineNumber - first
      var searchRanges = RenderSearchRanges(text)
      var lineProperties = filter(copy(properties), (_, item) => item.lnum == lineNumber)
      if len(lineProperties) > 128
        reason = 'This line exceeds the native property paint budget'
        break
      endif
      var byteColumn = 0
      var virtualColumn = 0
      var asciiLine = strlen(text) == strchars(text)
      for charIndex in range(strchars(text, true))
        var cluster = asciiLine ? strpart(text, charIndex, 1) : strcharpart(text, charIndex, 1, true)
        var plain = strlen(cluster) == 1 && cluster !=# "\t"
        var cells = plain ? 1 : strdisplaywidth(cluster, virtualColumn)
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
          for matchRange in searchRanges
            if byteColumn < matchRange[1] && nextByte > matchRange[0]
              style = MergeRenderStyle(style, RenderStyle(hlID('Search')))
              break
            endif
          endfor
          var previous = empty(renderBase) ? {} : renderBase[-1]
          if plain && !empty(previous) && previous.plain && previous.row == row
              && previous.byte_end == byteColumn && previous.style == style
              && previous.column + previous.cells == view.column + info.textoff + visibleStart
              && previous.override_style == (propertyOverride ? propertyStyle : {})
            previous.text ..= cluster
            previous.cells += 1
            previous.byte_end = nextByte
          else
            add(renderBase, {row: row, column: view.column + info.textoff + visibleStart,
              text: cluster ==# "\t" ? repeat(' ', visibleEnd - visibleStart) : cluster,
              cells: visibleEnd - visibleStart, line: lineNumber,
              byte_start: byteColumn, byte_end: nextByte,
              virtual_column: virtualColumn, plain: plain,
              style: style, override_style: propertyOverride ? propertyStyle : {}})
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
    endfor
    if empty(reason)
      renderBaseKey = baseKey
    endif
  endif
  if !empty(reason)
    renderBaseKey = ''
    frame.supported = false
    frame.fallback_reason = reason
    return frame
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
  var rowStyles: dict<dict<any>> = {}
  for lineNumber in range(first, last)
    var sign = get(signs, string(lineNumber), {})
    var lineStyle: dict<any> = {}
    if !empty(get(sign, 'linehl', ''))
      lineStyle = RenderStyle(hlID(sign.linehl))
    endif
    var lineCursor = &cursorline && lineNumber == line('.') && &cursorlineopt =~# '\<\(screenline\|line\)\>'
    if lineCursor
      lineStyle = get(sign, 'priority', 0) >= 100
        ? MergeRenderStyle(cursorLineStyle, lineStyle) : MergeRenderStyle(lineStyle, cursorLineStyle)
    endif
    if !empty(lineStyle)
      rowStyles[string(lineNumber)] = lineStyle
      add(frame.runs, {row: view.row + lineNumber - first, column: view.column + info.textoff,
        cells: textWidth, text: repeat(' ', textWidth), line: lineNumber,
        byte_start: 0, byte_end: 0, style: MergeRenderStyle(renderNormal, lineStyle)})
    endif
  endfor
  for baseGlyph in renderBase
    if visual ==# "\<C-V>" && strpart(getline(baseGlyph.line), baseGlyph.byte_start, baseGlyph.byte_end - baseGlyph.byte_start) ==# "\t"
      frame.supported = false
      frame.fallback_reason = 'Block selections through Vim tabs use the compatibility view'
      frame.runs = []
      return frame
    endif
    for glyph in empty(visual) ? [baseGlyph] : RenderSelectionSegments(baseGlyph, visual, anchor, current, block)
      var style = MergeRenderStyle(renderNormal, glyph.style)
      if has_key(rowStyles, string(glyph.line))
        style = MergeRenderStyle(style, rowStyles[string(glyph.line)])
      endif
      if !empty(visual) && RenderSelected(glyph, visual, anchor, current, block)
        style = MergeRenderStyle(style, visualStyle)
      endif
      if !empty(glyph.override_style)
        style = MergeRenderStyle(style, glyph.override_style)
      endif
      AddRenderRun(frame.runs, glyph, style)
    endfor
  endfor
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
            || max([anchor[2], current[2]]) > strlen(text))
        var endStyle = MergeRenderStyle(renderNormal, visualStyle)
        add(frame.runs, {row: view.row + lineNumber - first,
          column: view.column + info.textoff + endColumn,
          text: ' ', cells: 1, line: lineNumber,
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
      var columnStyle = MergeRenderStyle(renderNormal,
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
      var style = MergeRenderStyle(renderNormal,
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
  for row in range(view.row + last - first + 1, view.row + view.rows - 1)
    add(frame.runs, {row: row, column: view.column, text: '~', cells: 1,
      line: 0, byte_start: 0, byte_end: 0,
      style: MergeRenderStyle(renderNormal, RenderStyle(hlID('EndOfBuffer')))})
  endfor
  var position = screenpos(win_getid(), line('.'), col('.'))
  if get(position, 'row', 0) > 0 && get(position, 'curscol', 0) > 0
    frame.cursor = {row: position.row - 1, column: position.curscol - 1,
      width: max([1, position.endcol - position.curscol + 1]), visible: true}
  endif
  return frame
enddef

def EmitRender(force: bool = false)
  if !renderEnabled || !authenticated || shuttingDown || ch_status(bridge) !=# 'open'
    return
  endif
  try
    var frame = RenderFrame()
    var encoded = json_encode(frame)
    if strlen(encoded) > 900000
      frame.supported = false
      frame.fallback_reason = 'Native paint data exceeds the bounded frame budget'
      frame.runs = []
      encoded = json_encode(frame)
    endif
    if force || encoded !=# previousRender
      previousRender = encoded
      renderRevision += 1
      frame.revision = renderRevision
      Send(frame)
    endif
  catch
    # Rendering must never break actual editing or disconnect metadata/actions.
    renderBaseKey = ''
    renderRevision += 1
    Send({event: 'render', revision: renderRevision, supported: false,
      rows: &lines, columns: &columns, fallback_reason: 'Native paint unavailable: ' .. strcharpart(v:exception, 0, 512)})
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

def Diagnostics(): dict<any>
  var keyNow = string([getqflist({id: 0, changedtick: 0}),
    getloclist(0, {id: 0, changedtick: 0}), bufnr(), b:changedtick, lspDiagnosticsTick])
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
      remove(state.buffers, -1)
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
  if &buftype ==# ''
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
  if currentMode =~# '^no' || currentMode =~# '^[cr!t]'
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

def ResumeDeferredSave()
  if empty(deferredSave) || !authenticated || shuttingDown
    return
  endif
  var currentMode = mode(1)
  if currentMode =~# '^no' || currentMode =~# '^[cr!t]' || !empty(state('moaSw'))
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
  if !empty(deferredSave) && index(['snapshot', 'cancel_build', 'cancel_search'], action) < 0
    throw 'A save is waiting for Vim to finish input; wait for it before another IDE action'
  endif
  if index(['save', 'save_all', 'save_and_build'], action) < 0
    return false
  endif
  var currentMode = mode(1)
  var busy = state('moaSw')
  # Escape/keycode/mapping decoding can be briefly busy after the native GUI
  # has received a Save key. Pending operators, counts/finds, command lines
  # and completion still fail immediately; no synthetic Escape is inserted.
  if empty(busy) || currentMode =~# '^no' || currentMode =~# '^[cr!t]' || busy =~# '[oaS]'
    return false
  endif
  if action ==# 'save_and_build'
    BuildCommand(request)
  endif
  deferredSave = copy(request)
  deferredSave.id = id
  deferredSaveBuffer = bufnr()
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

def LspAction(action: string, request: dict<any>)
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
      renderBaseKey = ''
      renderBase = []
      if renderTimer >= 0
        timer_stop(renderTimer)
        renderTimer = -1
      endif
      Send({event: 'render_disabled'})
    endif
  elseif action ==# 'snapshot'
    EmitState(true)
    RefreshRenderTheme()
    EmitRender()
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
      authenticated = true
      authToken = ''
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
  authenticated = false
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
  ClearDeferredSave()
  if pendingQuit > 0
    Reply(pendingQuit, true)
  endif
  Send({event: 'bye'})
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
  autocmd WinScrolled,VimResized,OptionSet * RenderChanged()
  autocmd QuickFixCmdPost * Changed()
  autocmd User LspDiagsUpdated,LspAttached,LspDetached LspDiagnosticsChanged()
  autocmd VimLeavePre * Leaving()
augroup END

defcompile
bridge = ch_open(address, {mode: 'nl', callback: Receive, close_cb: Disconnected,
  waittime: 250, timeout: 0, noblock: true})
if ch_status(bridge) ==# 'open'
  Send({event: 'hello', token: authToken, protocol: 1, version: v:version,
    vim9: has('vim9script') != 0, pid: getpid()})
endif
