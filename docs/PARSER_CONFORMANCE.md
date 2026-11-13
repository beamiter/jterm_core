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

## Unicode and TUI framing extension

RFC 3629 section 3/4 (https://www.rfc-editor.org/rfc/rfc3629.txt) supplies
independent UTF-8 byte/range expectations. Explicit byte literals are paired with
handwritten scalar literals at U+0080, U+07FF/U+0800, U+D7FF/U+E000,
U+FFFF/U+10000 and U+10FFFF. U+FFFF is a noncharacter but has a well-formed UTF-8
encoding; this framing test does not promise a displayed glyph. Surrogates and
malformed sequences remain outside this valid-input slice.

Combining accents, variation selector 16, ZWJ emoji components and regional
indicators must survive as their original ordered scalars, without normalization,
composition or deletion. This asserts neither UAX #29 grapheme clustering nor
terminal cell widths or font rendering. XTerm's Control Bytes discussion says
UTF-8-decoded C1 values must not be processed a second time: encoded U+009B,
U+009D and U+009C therefore remain scalar tokens next to CSI/OSC-looking text.
This is distinct from raw eight-bit C1 control support, still excluded above.

A hand-authored TUI-like lifecycle uses DEC private modes 1049/25, CUP, Chinese
and combining text, SGR, CPR query, then cursor/primary-screen restoration syntax.
For both profiles it compares **every pair of ordered feed cuts**, as well as
single cuts and bytewise/empty feeds, directly with the handwritten tokens. No
whole-feed run supplies expected data. This tests syntax across mode-like and
UTF-8 boundaries, not actual alternate-screen mutation, replies, rendering or
real Vim execution. Existing real Vim CI remains separate integration evidence.

## CSI storage and numeric compatibility boundaries

The `local_policy_csi_*` cases specify bounded-parser compatibility behavior for
both profiles, separately where necessary. These are implementation policy
contracts, not standard-mandated acceptance or rejection rules:

* Raw CSI parameter storage retains at most 256 bytes for EmberV1 or 128 bytes
  for FrostLegacyV1; a leading private prefix occupies one of those bytes.
  Tests cover cap-minus-one, exact cap, cap-plus-one and longer input. Excess
  bytes are consumed rather than becoming printable text. This truncation is
  **not** a whole-sequence rejection: `rejected_count` stays zero.
* Eight intermediate bytes are retained. A subsequent valid query must be
  separate after the full final byte; no truncated sequence bytes leak into it.
* Each numeric field saturates at 65535 independently, including colon fields;
  empty fields remain zero, and colon flags are preserved.
* CAN, SUB and a fresh ESC after an over-cap parameter stream discard the old
  sequence and allow the next query. All single split points and bytewise feeds
  (including empty feeds) are checked with independent expected tokens.

These checks cover shared parser primitives, not consumer integration or the
five terminals' host behavior. They do not make silently truncating arbitrary
CSI inputs a universal security policy; downstream command semantics remain
separate, and unsupported syntax behavior is outside these fixtures.
