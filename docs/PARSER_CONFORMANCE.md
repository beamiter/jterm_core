# Independent parser framing corpus

`crates/jterm_engine/tests/parser_conformance.rs` supplies handwritten expected
tokens. It does not load a historical implementation, recorded host output, or
use a whole-feed parser result as the expected result. Every feed partition is
compared directly with the same manually specified expectation.

## References and scope

* ECMA-48, fifth edition (June 1991), control-sequence representation and control
  functions: https://ecma-international.org/wp-content/uploads/ECMA-48_5th_edition_june_1991.pdf
* XTerm control sequences, patch 411 (2026-08-23), C0 controls, escape sequences,
  CSI, operating-system commands and device-control strings:
  https://invisible-island.net/xterm/ctlseqs/ctlseqs.txt

These are framing tests, not a claim of complete ECMA-48, DEC, XTerm, Kitty or
Vim conformance. The parser only identifies syntax; grid mutation, default
parameter meanings, replies and supported terminal capabilities are separate.

## Mapping of expectations

* Printable ASCII and valid UTF-8: exact Unicode scalar output, including splits
  inside multibyte encodings. UTF-8 is the host encoding assumption, not an
  ECMA-48 definition of grapheme width.
* CR, LF, BS, HT and BEL: exact C0 token values. CR/LF within CSI are executed
  as control tokens without terminating the surrounding sequence.
* CUP `CSI 12;34 H`: raw parameter bytes `12;34`, numeric fields 12/34, final H.
* DEC private `CSI ?25 l`: private prefix `?` remains distinct from parameter 25.
* DECSCUSR `CSI 2 SP q`: the space is an intermediate, not a parameter or print.
* Secondary DA `CSI >0 c`: prefix `>` is retained; no device reply is asserted.
* Empty fields and XTerm colon SGR syntax: raw bytes preserve empty/subparameter
  positions. The API represents omitted numeric fields as zero; this is an API
  representation, not an assertion that every control defaults to zero.
* DEC save/restore ESC 7/8 and ESC ( 0/B character-set designation: framing only.
  `EmberV1` emits `Charset`, whose bank is the raw introducer byte `(`, not
  numeric bank index zero. `FrostLegacyV1` emits `Escape` with intermediate `(`
  and final designator instead. Both exact public API representations have
  separate handwritten expectations; the harness does not normalize them.
* OSC terminates with BEL or ST (ESC backslash), and the API distinguishes them.
  DCS/APC ST-delimited payloads are returned opaque. Kitty-like APC syntax is
  not interpreted, decoded, or acknowledged by this test.
* CAN/SUB abandon incomplete CSI/string sequences. Fresh ESC can restart an
  un-rejected control string. Neither cancellation nor restart is a print.
* Vim's intentionally unknown DCS `zz`, followed by `CSI 0 % m` and CPR query,
  remains an opaque DCS and two distinct CSI tokens. `%` must not disappear.
  This is a concrete syntax probe, not a claim that unknown DCS deserves a reply
  or that `CSI 0 % m` has SGR reset semantics.

## Separate local security-policy expectations

Tests named `local_policy_*` are deliberately not presented as standard mandates.
With an eight-byte payload limit, exactly eight bytes are accepted and nine are
rejected. After rejection, embedded erase/query/clipboard-looking syntax remains
non-executable until ST, CAN or SUB; a following genuine SGR and print resume.
All five string kinds are exercised for rejection. APC abandonment reports one
`Step.abandoned_apc`, while no `RejectApc` reflective recovery event is allowed.
This verifies the local bounded-mode policy in both compatibility profiles.

## Method and exclusions

Each case runs against `EmberV1` and `FrostLegacyV1` bounded constructors, at every
single split and then byte-by-byte with repeated empty feeds. ASCII batching is
normalized only by expanding ASCII runs into scalar tokens. Boundary notifications
are omitted from semantic tokens, but rejection and APC-abandonment counts are
asserted independently. Progress, input consumption, final ground state and
retained storage are also bounded for these specific corpus inputs. The
`limit + 3` assertion is not a universal CSI bound: CSI has independent fixed
parameter/intermediate caps. The charset API difference is asserted explicitly; no profile differences are
silently reconciled beyond non-semantic Boundary notifications.

This deliberately small common subset does not cover eight-bit C1 controls,
invalid UTF-8 replacement policy, arbitrary parameter saturation, semantic
screen state, grapheme rendering, terminal replies or application interoperability.
The existing real Vim workbench tests and the separate Tauri PTY Vim smoke remain
complementary integration evidence, not the source of these expected tokens.
