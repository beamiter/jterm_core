# Compare two command outputs

`output_compare` compares two **available retained snapshots**, earlier first.
It returns an owned report, so a native review window can keep the result stable
while its terminal continues working. It does not run commands or read live
terminal rows, and it adds no dependency or persistence format.

## Native integration contract

1. Resolve exactly two completed commands by the originating pane/session and
   stable record identities. Order them chronologically. Do not substitute a
   different command when one has been evicted or the focused pane has changed.
2. Capture each available output and its truncation flag. Missing output is an
   unavailable comparison, not an empty successful capture. Display the two
   commands, working directories, exit outcomes, and their identities beside
   the result so users can assess what was compared.
3. Call `compare(OutputSnapshot { text, truncated }, ...)` with plain output.
   Existing ANSI projections should be applied by their owning frontend.
   The core makes remaining control and visual-spoofing characters visible.
4. Show `summary()` and `text` in a read-only, scrollable native surface. `+`
   denotes later output, `-` denotes earlier output, and a leading space denotes
   unchanged text. CR and missing final newlines remain distinguishable.
5. Keep the report's limit notices. `is_complete()` is false if capture, input,
   alignment, or rendering limits apply. Identical partial text never proves
   that the complete executions produced identical output.

Comparison and selected export are separate read-only actions. Export must use
the exact reviewed selection, not whichever records happen to be focused when
the user finishes interacting with a dialog. An unavailable selected record must
reject that selection; an existing record with unavailable output can be exported
with its output-unavailable metadata intact. Preserve each host's existing
private-file, redaction, and bounded-snapshot behavior.

## Work and memory budgets

Each input is examined up to 256 KiB and 4096 physical lines. UTF-8 prefixes never
split a scalar. Exact common prefixes and suffixes are retained without building
an alignment matrix. The changed middle uses a longest-common-subsequence matrix
only if it has at most 1,048,576 cells (2 MiB of `u16` storage). A larger middle is
shown as removed/added regions with an explicit coarse-alignment notice; the
result is not presented as a minimal edit distance. Expanded display text is
capped at 512 KiB, including its clipping notice. These bounds limit individual
work, but do not establish measured desktop latency on every machine; hosts may
use an existing bounded worker when appropriate.

Run focused regression coverage with:

```sh
cargo test --locked --lib output_compare
```

Coverage includes inserted lines, repeated/reordered content, reconstruction of
both sources, empty output, CRLF/final-newline differences, Unicode, upstream
truncation, input limits, coarse fallback, and expansion by control characters.
