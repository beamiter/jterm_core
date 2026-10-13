# Chat mode: native terminal contract

Chat mode is an opt-in terminal presentation beside Block mode. It does not
require an AI provider and does not replace a shell with command-by-command
execution. Each pane keeps its existing long-lived PTY, shell, terminal parser,
execution lifecycle, history identities, and input path.

## Minimum complete presentation

- A dark transcript surface, a right-aligned command capsule in the theme's
  blue/accent color, and a separate left-aligned rounded result card.
- A bottom-docked multiline composer containing the **existing live shell
  terminal**, including its real cursor and IME. This is not a second editor
  whose contents are pasted back into the shell on submission.
- A local ASCII companion inhabits that composer and uses existing observed
  command outcomes. It must not cover typed characters or receive a copy of
  input. No model, network service, or fabricated assistant response is needed.
- The result card keeps real output and its existing selection/search origins.
  Labels, companion art, buttons, borders, spacers, and status rows are native
  chrome with no raw-terminal origin. They never become PTY bytes or terminal
  copy/search content. Explicit command-copy controls may copy the same safe
  command metadata that the existing Block command-copy action already uses.

Separate layout and presentation make this a new mode; simply renaming Block
or changing the color/radius of its existing full-width header is insufficient.
The parser, PTY, lifecycle, and safe history implementation should be reused.

## Configuration and switching

The canonical opt-in spelling in all four applications is:

```toml
terminal_mode = "chat"
```

Anvil and Forge add `Chat` beside their existing `Block`, `Vte`, and `Unified`
variants. `Chat` uses their existing shell-integrated terminal engine and is
not Unified. All old values, defaults, CLI options, and legacy behavior remain
valid. Their existing case-insensitive parsing applies to the new value too;
no additional `chatbot`, `capsule`, or `ai` aliases are introduced.

Ember and Frost retain the existing `block_mode` boolean for old configuration
files and add an optional explicit terminal-mode setting with canonical values
`normal`, `block`, and `chat`. `normal` means their existing conventional
terminal rendering, not a change of terminal engine. An explicit mode
takes precedence; an absent value preserves the boolean's old meaning and its
old default. Saving Chat must retain an explicit `chat` value, rather than
silently serializing it as `block_mode = true`.

Changing a preference must not terminate a running shell or rebuild its PTY.
Applications whose existing backend-setting changes apply only to new/restored
panes may keep that behavior, with a clear UI message. An existing Block pane
must not claim it has become Chat when only the new-pane preference changed.
A live layout-only switch is allowed where the frontend can preserve the same
session, cursor, selection identities, scroll anchor, and unfinished input.

## Shared core and frontend boundary

`chat_mode::resolve_surface(ChatSessionState)` is the shared, pure selection
function. Frontends supply facts from their existing lifecycle owner; they do
not add a second OSC parser or infer readiness from text that resembles `$`.

Priority is exact:

1. Alternate screen active: `Fullscreen`.
2. Usable shell integration absent: `Passthrough`.
3. Command running: `Running`, even if a stale prompt flag is also set.
4. Ready prompt: `Prompt`.
5. Any between-boundaries state: `Passthrough`.

Only `Prompt` docks the live terminal as the composer. `Running` keeps the real
terminal usable as the active result/interaction surface. `Fullscreen` gives
the entire pane to the existing native terminal. `Passthrough` keeps the same
session usable without guessing command boundaries. These are temporary
surfaces, not changes to the saved mode; a later valid prompt restores Chat.

The core owns no command strings, input bytes, PTY writes, timers, GUI geometry,
pixels, or persistence. Native frontends own capsule/card painting, clipping,
scrolling, mouse hit testing, accessibility, font metrics, and projection from
existing stable command/row identities. Existing `block_contract` outcome and
provenance rules remain authoritative; missing exit status is not success.

## Input and compatibility requirements

- Keystrokes, shell history arrows, completion Tab, readline editing, IME, and
  bracketed paste retain their existing terminal routes. A visual composer
  must not intercept Enter or install another command-execution handler.
- An optional send button invokes the existing Enter action **once**, without
  copying the displayed command, clearing it, inserting it again, or adding a
  second carriage return. Recompute surface state when clicked. A stale button
  cannot submit to a foreground application. Keyboard Enter remains available
  in every state, including interactive programs.
- Multiline input remains shell-owned: wrapped lines, continuation prompts,
  heredocs, and pasted multiline text follow existing shell/paste policy. The
  UI never splits a multiline command into separate `exec` calls.
- `sudo`, `read -s`, SSH, `cat`, and REPLs keep their native stdin and echo
  behavior. Chat introduces no keystroke buffer or secret capture. A hidden
  password must not appear in a capsule, companion memory, log, or new history.
- Ctrl-C, Ctrl-D, Ctrl-Z, foreground process groups, window resize, mouse
  reporting, and application keyboard protocols stay owned by the same PTY and
  emulator. Vim/less/top alternate-screen transitions restore Chat afterward.
- Completed command/result pairs refer to the existing history record, not a
  second independently persisted conversation. Copy, export, search, collapse,
  restore, bookmarks, and scroll anchors use existing stable identities.
- Painting and hit testing use the same clipped rectangle. Synthetic chrome
  has no raw origin; a click outside a live grid cannot be clamped into a
  valid terminal input/selection coordinate.

## Required verification

### Core

- All 16 combinations of integration, prompt, running, and alternate-screen
  flags select one deterministic safe surface.
- Running overrides a stale prompt. Alternate screen overrides every flag.
- Initialization, missing integration, and between-boundaries states pass
  through; valid later prompt recovers without session replacement.
- A prompt/run/alternate-screen/return/prompt sequence restores the composer.
- Only a live `Prompt` allows the optional submit affordance.

### Each frontend

1. Parse and save `chat`; preserve all old mode values and defaults. Preserve
   existing case/unknown-value behavior. Ember/Frost specifically test absent
   mode with both legacy booleans, explicit Chat with both legacy booleans,
   and explicit `block`/`normal` overriding the contradictory legacy boolean.
2. New Chat pane renders independently from a simultaneous unchanged Block
   pane. Settings/CLI accurately report which panes a switch affects.
3. Run `pwd`, then `cd` to another directory, then `pwd`; one shell persists.
   Environment assignments, jobs, and exit status also remain session-local.
4. Verify Tab completion, arrows, middle-of-line editing, IME, paste, multiline
   shell syntax, heredoc, and continuation prompts. No swallowed or doubled
   keystrokes/commands. Failed PTY writes must not be reported as accepted.
5. Exercise `cat`/a REPL and `read -s` while the transcript is scrolled. Input
   goes once to the live process; hidden input is absent from new UI/history.
6. Enter and exit Vim/less repeatedly, including Ctrl-C, resize, and mouse
   reporting. Chat restores afterward, preserving the same PTY identity.
7. Search/copy/export known output while excluding labels, status, companion,
   and spacers. Scroll back during output; new output must not steal the read
   position. Jump-to-live returns to the real composer/interaction surface.
8. Disable or lose shell integration. Raw terminal fallback remains usable;
   it neither fabricates bubbles nor replays earlier input. Later valid
   integration recovers the Chat presentation.
9. Verify with AI disabled and without provider credentials/network access.
   Verify the existing default and explicit Block mode have no regressions.

Unit/static checks establish only the cases they execute. Native visual and
interactive verification must be reported separately, including unavailable
runtime or GPU/display coverage; screenshots alone do not prove PTY safety.
