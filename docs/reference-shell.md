# Shared native reference shell

`reference_shell` is the toolkit-independent contract used by Anvil, Forge,
Ember and Frost. It records logical dimensions, semantic colors, typography
estimates, public presentation names and compact panel collapse rules.
It contains no terminal/input implementation or fabricated task state.

The supplied JPEG is 1448 × 1086. The target application is its upper window,
crop `(46,28)` with size `1356 × 704`. The three panels below are explanatory
details, not additional application panels. Measurements and source colors
were supplied by the parent task after actual pixel inspection. Implementers
in this execution environment did not receive image pixels; pixel acceptance
belongs to the parent task and remains pending until native screenshots are
compared with that source.

## Component mapping

| Contract component | Anvil | Forge | Ember | Frost |
| --- | --- | --- | --- | --- |
| Global toolbar / rail / navigation | Relm4 / GTK widgets around existing app model | GTK widgets around existing notebook and UiState | egui panels around render_ui | Iced widgets around view and pane_view |
| Center header and shell | GTK boxes / CSS | GTK boxes / CSS | native egui layout | native Iced layout |
| Block content | existing continuous native VTE backend | existing continuous native VTE backend | terminal renderer and PTY projection | TermWidget and PTY projection |
| Chat / composer | existing TermView Chat and live shell | existing TermView Chat and live shell | existing Chat projection and live input | existing Chat projection and live input |
| Agent / Context / Files | existing tasks, context and files controllers | existing tasks, context and files controllers | existing session/task/file state | existing session/task/file state |
| Scroll / follow intent | retained GTK canvas and transfer policy | retained GTK canvas and transfer policy | session TranscriptCanvas | session TranscriptCanvas |
| Shared motion | CanvasMotion | CanvasMotion | CanvasMotion | CanvasMotion |

The public mode switch contains Block and Chat only. Legacy normal, vte and
unified persisted values remain readable and resolve to the supported Block
presentation. Internal backends may remain distinct to preserve running
sessions, recovery and remote-session behavior. Backend mapping differences
must be recorded rather than silently losing capabilities.

At the reference size: titlebar 42 px, rail 55 px, navigation 237 px, center
758 px and inspector 306 px. Center header is 49 px; composer is inset 28 px
from both sides, 80 px high and 11 px from the bottom. Metadata groups pack
left with a fixed 6 px gap. Default center is pale, with a dark surrounding
shell and dark native terminal content. Regions beneath the reference window
must not become a permanent second dashboard.

The center absorbs width changes. The inspector collapses below 1078 logical
pixels and navigation below 772 pixels, preserving a useful center. Frontends
must keep collapsed capability entry points accessible. DPI changes scale the
logical contract rather than squeezing every component proportionally.

The original font is unknown. DejaVu Sans / DejaVu Sans Mono are explicit
available validation assumptions, not an asserted identification of the source
font. Terminal font preferences and user scaling remain authoritative.

## Acceptance and honest state

Each frontend needs actual native screenshots at the same window bounds and
DPR in Block and Chat, plus compact/wide windows, long content, scrolling,
IME and keyboard focus checks. The parent compares toolbar, left navigation,
center/composer and inspector independently and requests corrections. A
headless render, successful build or passing numeric metric is insufficient
for final pixel acceptance.

Only observed backend task/output state may be shown as current. Unsupported
Attach/Image/Apply and provider actions must be explicitly unavailable.
Deterministic demonstration data belongs in a named test fixture; it must not
be injected into a real session as fabricated agent work or test success.
There must be one application-level organism, not one per card or split.

## Stage 5 pixel review corrections

The supervising task inspected all eight Stage 5 native captures against the
original. Broad regions are close; these corrections remain acceptance work.
The new dimensions below follow that review, not an original design export.

- Both modes use one controls factory and the same slots. At reference size,
  controls are 28 px high at y657: composer bottom 693 minus 8 px inset minus
  28 px. Keep the tools packed at 6 px spacing. The right model slot keeps at
  least 120 px and uses `Model unavailable` consistently. Changing native input
  ownership must not move these controls or introduce a differently styled
  input field. Disabled text uses the muted token without layered opacity.
- Spaces rows are compact 30 px. Five fixture Recent rows use 32 px each,
  differentiated shared icons and an explicit selected first row. The status
  belongs outside the navigation scroller in a bottom-anchored 49 px region:
  its separator is near y655 at the reference window height.
- Mode controls are 28 px high, inset 7 px into the 42 px titlebar. Header
  actions have 24 px hit areas and fixed 4 px gaps, with stable slots for
  plus, search and overflow even when an action is unavailable. Inspector tabs
  use the same 50 px selected-tab width rather than toolkit defaults.
- Use regular, left-aligned 12 px text inside the right-anchored user bubble.
  Headline uses DejaVu Sans at 20 px with bold weight 700; body and navigation
  use weight 400. This is a shared font assumption, not identification of the
  JPEG's original font. Page/role titles are 12 px, with consistent hierarchy.
- Inspector metadata first text is near y185 with 19 px pitch; progress count
  is right aligned beside its bar. The Current Step surface is 45 px high
  near y298. Preserve the full-width output pane at y542–682, with its heading
  and honest state outside the pane rather than an extra inset subheader.
- Diff uses old and new 28 px number gutters, a 16 px sign column and the same
  nine fixture rows at 15 px line height. Preserve empty rows as layout rows.
  Body padding is 4 px above and below the code rows. Copy and unavailable
  Apply occupy stable native control slots.

Running/TUI output must remain in a correctly resized native viewport above
the noneditable composer shell. Do not paint a white shell over raw output or
create a second editor/IME owner. Structured Assistant/task/diff fixtures do
not prove production event binding. The ASCII organism currently observed in
GTK captures is not accepted as a reconstruction of the graphical reference;
its existing unique owner must never cover real terminal output.

No shell layout change may restart a running PTY, re-send input, steal IME
ownership, reset reading anchors or blindly follow output while the user is
reading history. Live output retains culling, fractional motion and the
existing intent-aware follow policy.

## Finite reference-component completion

`reference_popup` defines one dark 264 px Tools menu: six 30 px rows with a
14 px shared icon, command name and a right-aligned description. The first
three route only to existing Terminal, Command palette and Search actions;
unavailable capabilities retain disabled rows. Adapters own focus restoration,
Escape, keyboard navigation and anchor preservation. Opening a popup does not
change the conversation model, follow intent or PTY input.

`reference_identity` embeds byte-identical 128 px product icons already shipped
by each repository. Source paths and SHA256 are retained in
`assets/identities/sources.json`. Decode/cache once, using 24 px product and
22 px Spaces slots. Workspace selectors retain identity/name, actual or neutral
branch, and chevron slots. Never invent a live branch to match fixture data.
Diff headers retain language/count slots and surfaced Copy/disabled Apply
controls without changing the already aligned code-body height.

`reference_organism` reconstructs the supervising task's measured JPEG contour
as shared cubic paths in a 64 px box: dark rounded body, crossed ear loops,
diagonal orbits, pale eyes, short smile and restrained blue glow. It is not an
original asset recovered from the JPEG. Native adapters draw these same paths
only for the existing unique application owner in an admitted safe region.
Preserve actual organism state and speech; a status dot cannot fabricate awake
state. Provide a native rounded speech bubble and dismiss/focus affordances.
The component cannot cover terminal output, editor/composer controls or create
a companion per split/message. Capture the visible expanded state separately.

The Stage 6 Anvil and Forge same-mode composer geometry has passed supervising
pixel review. Do not restructure these accepted factories to implement menus,
identity assets or graphical companion presentation.
