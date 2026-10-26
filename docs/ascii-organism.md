# ASCII organism: body, state and attention

The four terminal frontends share one content-free life model and renderer.
Native adapters report actual command lifecycle events and paint the resulting
state. A facial expression or a quiet output stream never establishes success,
failure, completion or a new command.

## One life per GUI

The companion belongs to the GUI, not to a block, split or status card. All
native surfaces must use `organism_companion::CompanionSurface` to choose
exactly one representation: `Body`, `Compact`, or `Hidden`. A compact glyph is
a replacement for the body, never an additional pet. Full, Calm and Static
motion use the same ownership rule. Ineligible/remote/read-only/alternate-screen
hosts cannot keep a passive duplicate after losing ownership.

Window life, ambient mind, voice and care clock live outside pane state.
Pane-local reducers still track their own authoritative command and repository
facts. Switching panes changes the companion's immediate context and geometry;
it does not create a new personality or reset its care interval. Old surfaces
are cleared as part of the ownership handoff, before a new one appears.

Block/Chat no longer need a permanent organism status row. Detail views and
tooltips explain the live body in text. Settings pose galleries are explicitly
examples, and must not run a second live companion behind their modal view.

## A voice and optional gentle reminders

`CompanionVoice` provides four authored styles (Gentle, Curious, Playful and
Steadfast), context-aware Hello/Company replies, three rotating short stories
per style, and Rest replies. A stable host key can keep its style across
restarts; no key text is retained. These are local authored replies, not an
LLM chat service. Replies never change a command result, earn growth, dispatch
an agent, or diagnose the human's feelings. A running command remains running
when the user asks to rest.

`CompanionCare` is optional and disabled by default. It uses the existing
window heartbeat and sampled foreground activity, without collecting keys or
terminal text. After 45 minutes of such activity it may offer one text prompt:
water, a comfortable stretch, or looking away from the screen. The host only
allows an offer in a quiet, available surface. Prompts never accumulate into a
notification queue. A five-minute absence or long suspend resets the interval;
Snooze postpones an offer for 20 minutes; Rest acknowledges a pause without
claiming the user performed a health-related action. Motion preference does
not enable reminders. The host's explicit care preference does.

See [the companion evolution contract](companion-evolution.md) for the current
GUI boundary and the future cross-terminal/JWM ownership model.

## A body with pauses and small actions

Full motion uses short, weighted visual clips within the existing three-row
ASCII body:

| Situation | Body performance |
| --- | --- |
| Quiet idle | Long pauses, an occasional look and blink, a tail flick and a short paw-grooming sequence |
| Walking | Two steps separated by contact/settle poses, only while the host reports movement |
| Exploring | Looks to both sides, blinks and pauses before looking again |
| Greeting | Raises a paw, winks and relaxes within the existing greeting hold |
| Success | A small crouch and release within the original success reaction |
| Recovery | A wider celebration with alternating accents, preserving the cat silhouette |
| After push | A relaxed expression and occasional tail flick; the success mark stays visible |
| Sleep | Slow breathing and the existing sleep marks |
| Steady command | A rare blink; output rhythm remains authoritative |

The idle clip is 16 seconds at the adult Full-motion cadence; more than 85% of
it is the original quiet pose. Juvenile and seasoned bodies retain their
existing faster/slower cadence and width-preserving maturity marks. This does
not add a timer, random event, notification, persistent reward, or physiology
step. Hosts continue to control actual animation scheduling.

Each clip has a fixed ASCII bounding box equal to its canonical behavior.
Frame zero remains the original pose, so existing Calm and Static adapters
keep their stable presentation. Growth applies to all frames with one owned
buffer for juvenile/seasoned appearances; adult frames remain borrowed.

Semantic transitions take priority over decorative clips. Failure vigils,
unknown outcomes, tense ears, drowsiness and listless yawns retain their meaning.
An animation frame is a pure sample: skipping ahead after a suspended window
does not replay gestures or advance the life model.

## One explanation across native surfaces

`organism_daily` provides three allocation-free readout helpers:

- `behavior_label(Behavior)` names the observed pose in a compact heading.
- `context_explanation(RenderContext)` explains the actual painted context,
  including idle body language and all three command-watching output rhythms.
- `life_description(LifeState)` summarizes the simulated creature's state in
  plain language. It describes the creature, never the human's mental state,
  health or productivity.

Use the current underlying context as the input. A command or unresolved repo
vigil must never be replaced with an emotional explanation merely because the
creature is tired. Quiet output means only that the running command has no new
observed output; it is not a stall or completion diagnosis.

Completion adapters must preserve `Reaction::tone`, `description`, and
`hold_duration()`. A habituated quiet success must not become a loud success
card simply because its body uses `Celebrate`. Retained status belongs to that
specific reaction and expires with its original hold; new work, input, context
changes and resets take priority.

## Native integration responsibilities

Feed `AmbientMind::step` the current authoritative `RepoVigil` when the pane is
genuinely idle. Passing `None` unconditionally discards the work loop as soon as
its immediate reaction settles. Keep repo identity, day boundaries and owner
changes at the existing native memory/context boundary; do not borrow a stale
failure from another directory.

A painted transition must start when it actually becomes visible, not when an
offscreen pose was computed. Retargeting, typing, layout rejection, motion
changes and owner changes cancel obsolete presentation. A body that cannot fit
must not schedule its own fast animation loop merely to keep failing its fit
check. Shared physiology or an expiring real reaction may independently need
a slower wake.

The live overlay remains subordinate to terminal input and selection. Native
inspectors/readouts show observed state; opening one does not create work,
earn growth or extend a greeting. Visibility, palette commands, keyboard
bindings and settings still belong to each frontend.

## Validation

```sh
cargo test --locked --lib organism
cargo clippy --locked --lib -- -D warnings
```

The suite covers event outcomes, repo recovery, memory, attention admission,
all growth/pose envelopes, quiet-time weighting, protected state precedence,
and frame clocks up to `u64::MAX`. Native frontend tests cover their lifecycle
and presentation adapters separately. These tests do not measure desktop GPU
performance or replace a native window smoke test.
