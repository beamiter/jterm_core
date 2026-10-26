# Continuous Block and Chat canvas

Block and Chat use a continuous vertical surface inside each tab/pane. The
terminal remains the source of commands, output, cursor, selection and shell
state. The canvas changes how that source is presented and moved.

## Interaction contract

- A precision touchpad movement is a logical-pixel movement, including its
  fractional part. It must not wait until enough input has accumulated for a
  whole terminal row. Do not add another inertia multiplier to operating-system
  pixel events. GTK may use its controller's own gesture-end velocity when it
  owns the scrolling gesture.
- Discrete wheel steps and explicit return-to-latest actions have a short,
  continuous trajectory. New wheel input extends the existing destination;
  reversing the wheel responds from the pixels currently on screen.
- Streaming output retargets the live edge without resetting a scroll animation
  on every chunk. Reading history disables follow. New output below a reader
  does not move their position, and stopping input never rounds back to a row.
- A short history begins at the top of the canvas and grows downward from a
  stable origin. Chat's unused space is between the history and fixed composer;
  adding a second short command does not push the first command upwards.
- Content inserted, removed or remeasured above a reading anchor preserves that
  anchor's screen position. Existing stable command/raw-row identities remain
  the authority; an index in a temporary visible list is not a durable anchor.
- Text, card backgrounds, search/selection highlights, clipping and pointer
  mapping use the same presented offset. A terminal-row projection paints an
  extra edge row while fractional movement reveals it, avoiding a blank strip.
- Chat's prompt composer stays docked. Scrolling the transcript does not move
  the editable prompt or change its existing PTY/IME route. Interactive and
  alternate-screen applications retain their existing terminal ownership.
- Each pane owns its motion. Switching tabs, closing panes or changing surface
  ownership must retire stale callbacks and must not transfer momentum to the
  next session. Hidden and settled canvases do not request animation frames.
- Respect the desktop reduced-motion preference: continuous precision movement
  still works, while programmatic easing completes immediately.

The surface feels continuous across a long history; it does not allocate an
unbounded bitmap or remove the existing bounded scrollback policy. Retained
GTK cards and bounded GPU projections remain responsible for visible content.

## Shared motion

`jterm_core::transcript_canvas::CanvasMotion` stores a presented position, a
destination, a velocity and valid bounds, all in logical pixels. It has no GUI
dependency, allocation, timer, terminal text or input buffer.

The host calls `advance(elapsed_seconds)` from its native frame callback and
uses the returned position for both painting and hit testing. A critically
damped spring is solved analytically for each elapsed interval. A 60 Hz and a
144 Hz display therefore follow the same trajectory at the same time, rather
than applying an arbitrary decay once per frame. A scheduling gap over 250 ms
settles the old destination instead of replaying a hidden tab's motion.

| Situation | Motion operation | Host responsibility |
| --- | --- | --- |
| Discrete wheel | `scroll_by(pixel_delta)` | Convert line/notch units once; disable follow when reading |
| Precision touchpad or scrollbar | `jump_to(position + pixel_delta)` | Deliver exact pixels; do not double-apply native momentum |
| New live output | `set_target(live_edge)` | Retarget only while following; publish changed layout |
| Reading interruption | `cancel()` or direct input | Keep the current anchor; do not infer follow from animation lag |
| Content above the anchor changes | `rebase(lower, upper, delta)` | Translate the stable reading anchor before clamping to the new bounds |
| Resize or bounded-history trimming | `set_bounds(lower, upper)` | Preserve the existing semantic anchor before clamping |
| Desktop reduced motion | `set_reduced_motion(true)` | Observe the toolkit's preference |
| Native frame | `advance(dt)` | Request another frame only if `is_animating()` |

`set_target` retains position and velocity during ongoing output, and an
unchanged target never extends the animation. `jump_to` and `cancel` retain
fractional pixels. A reversed discrete wheel discards movement that was queued
in the opposite direction but has not been presented. Collapsing content and
rewritten progress lines cannot make a fast spring overshoot its new target.

When trimming a prefix, retain the pre-mutation reading coordinates and call
`rebase` with the new bounds and removed-height delta. Calling `set_bounds`
before `shift` can clamp a reader near the old bottom and then subtract the
removed height again. A native adjustment may already have clamped itself by
the time widgets are removed, so adapters must capture the motion before that
mutation. `shift` remains suitable when the bounds are unchanged.

Viewport resize and font zoom retain a reading anchor's fractional row
position, not only its integer projection. A reader less than one row above
the live edge still owns history; a zero integer offset must not silently
restore live following. Reflow retires obsolete wheel destinations rather
than applying old geometry's queued movement to the new layout.

## Native adapters

Anvil and Forge own a retained GTK history tree and a `GtkAdjustment`. Their
adapter uses GTK's frame clock, distinguishes surface pixels from wheel units,
and treats externally moved adjustments as direct user control. Existing card
virtualization keeps the full history's layout height while mounting content
near the viewport.

Ember and Frost keep their existing terminal-backed row/provenance projection.
Each session adds a continuous pixel coordinate. Only crossing a whole-row
boundary changes the projected row window; the remainder is a shared render
and input translation. The extra visible edge row is overscan, not additional
terminal history. Live following and history anchor corrections use the same
session-owned motion state.

This change does not turn every terminal row into an independent rich-text
widget. It establishes continuous canvas geometry without replacing terminal
semantics or introducing a separate command editor.

## Verification

Core behavior is exercised with `cargo test --lib transcript_canvas`:

- exact fractional precision input and final positions;
- identical trajectories at 30/60/90/120/144/240 Hz;
- immediate wheel reversal and no accumulated overscroll at document edges;
- continuous retargeting during output and eventual idle after unchanged output;
- reading-position stability, interruption and anchor rebasing;
- independent panes, reduced motion and suspended-frame recovery;
- bounded geometry, collapsing content and non-finite input rejection.

Each frontend must additionally check its native frame scheduling, fractional
paint/input alignment, output-follow interruption, tab isolation and Chat
composer docking. A software-rendered/Xvfb check establishes only that runtime
and display path. Physical touchpad, monitor refresh rate and hardware GPU
feel require an actual desktop acceptance run; controller tests alone are not
a frame-time benchmark.

`scripts/native-transcript-canvas-smoke.py` additionally opens a built frontend
in a private Xvfb display with a synthetic Bash/OSC profile, creates 18 real
command/output pairs and injects an X11 wheel event over transcript text. It
paces real keyboard input and waits for the shell's command sequence to finish,
so a slow software renderer cannot silently turn the fixture into partial
commands. Wheel press/release is dispatched without a post-event delay, keeping
the initial animation frames inside the sampling window. It
first checks that the first output keeps the same screen Y when the second
short command is added, with separate screenshots of both states. It then
samples actual displayed marker pixels through the motion, checks for at least
four different intermediate positions and a stable final position, and retains
screenshots, a short GIF, binary SHA-256 and the sample timing in its output
directory. It needs Python with Pillow/NumPy, Xvfb and xdotool:

```sh
python3 scripts/native-transcript-canvas-smoke.py anvil chat \
  --binary ../anvil/target/debug/anvil --output /tmp/anvil-chat-canvas
```

Run separately for each frontend and `block`/`chat`. The output directory must
be new. This pixel test does not certify hardware latency, physical touchpad
momentum or all selection/IME interactions; the native adapter tests cover
their own coordinate and event contracts.
