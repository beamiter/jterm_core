vim9script
# Real Vim owns editing, undo, mappings, plugins, buffers and windows.  This
# optional runtime publishes metadata and accepts a small, explicit action set.
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
var deferredSave: dict<any> = {}
var deferredSaveTimer = -1
var deferredSaveBuffer = 0

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
    'snapshot', 'next_diagnostic', 'previous_diagnostic', 'quit']
  if executable('rg')
    add(result, 'search')
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
  var context: any = get(getqflist({context: 0}), 'context', {})
  var searchOmitted = type(context) == v:t_dict && get(context, 'source', '') ==# 'search'
    ? searchDropped : 0
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
    capabilities: Capabilities(),
    build: {running: buildRunning, command: buildCommand, exit_code: buildExit,
      cancelled: buildCancelled, output: copy(buildTail),
      truncated: len(buildLines) + buildDropped - len(buildTail)},
    search: {running: searchRunning, query: searchQuery, count: len(searchRows),
      truncated: searchDropped, exit_code: searchExit, errors: searchErrors}}
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
  if authenticated && !shuttingDown && flushTimer < 0
    flushTimer = timer_start(40, Flush)
  endif
enddef

def Poll(_: number)
  # setqflist() itself has no QuickFixCmdPost event.  This bounded, low-rate
  # reconciliation also observes plugin capabilities loaded after VimEnter.
  EmitState()
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
  if !empty(deferredSave) && index(['snapshot', 'cancel_build'], action) < 0
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
  setqflist([], ' ', {title: 'Build: ' .. buildCommand,
    lines: ['JTERM_DIRECTORY ' .. projectRoot] + buildLines,
    efm: '%DJTERM_DIRECTORY %f,' .. buildErrorFormat,
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
  if searchExit <= 1
    setqflist([], ' ', {title: 'Search: ' .. searchQuery, items: searchRows,
      context: {jterm_vim9: true, source: 'search'}})
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

def StartSearch(request: dict<any>)
  var query = RequiredString(request, 'query', 4096)
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
  searchDropped = 0
  searchExit = 0
  searchClosed = false
  searchExited = false
  searchRunning = true
  # A literal query and argv boundaries protect leading '-', quotes, spaces
  # and shell syntax.  rg still honors the project's ignore files.
  searchJob = job_start(['rg', '--json', '--line-number', '--column', '--color', 'never',
    '--fixed-strings', '--smart-case', '--hidden', '--glob', '!.git', '--', query, '.'],
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
  if action ==# 'snapshot'
    EmitState(true)
  elseif action ==# 'build'
    StartBuild(request)
  elseif action ==# 'cancel_build'
    if buildRunning
      buildCancelled = true
      job_stop(buildJob)
    endif
  elseif action ==# 'search'
    StartSearch(request)
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
      cursor(lineNumber, column)
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
