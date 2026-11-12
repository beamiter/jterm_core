# Same-machine parser framing baseline

This is a diagnostic baseline, not a CI wall-clock gate and not a claim about terminal drawing, PTY latency, memory RSS, or all four consumers. It measures only the shared `jterm_engine::parser` public framing API. Correctness is checked separately by `parser_conformance`; timing output is never expected protocol output.

Run on an otherwise idle machine:

```sh
CARGO_BUILD_JOBS=1 crates/jterm_engine/scripts/benchmark-parser.sh > parser.csv 2> machine.txt
```

The script records UTC, exact base revision, tracked staged-plus-unstaged working-diff hash, benchmark source hash, compiler, kernel and CPU. Save all changed sources with the report: the diff hash alone does not include untracked files. `--smoke` uses 64 KiB and one sample only to exercise the harness; it is not a statistically meaningful baseline.

The release binary uses four generated public byte streams: printable ASCII, CSI plus valid UTF-8, short OSC titles, and DCS/query framing. Each uses approximately 2 MiB per sample, 1-byte and 4096-byte feeds, both compatibility profiles, and bounded/legacy constructors in the same executable. Corpus allocation occurs outside timing. One untimed pass warms each mode; seven samples alternate the order of the two modes to reduce order bias. Parser construction and destruction remain in each timed pass. Events are consumed with `black_box`, and progress and valid-corpus non-rejection are asserted. `events` is a framing diagnostic and is not normalized across chunk sizes/profiles.

Report raw samples, medians and range for each group; compute bounded/legacy ratio only within identical corpus/profile/chunk. Shared-cloud scheduling, CPU frequency changes and concurrent work can dominate small differences. Do not compare results across machines or compiler versions as if they were an A/B speedup. No absolute latency threshold belongs in CI here. No allocator hooks are installed: this harness does **not** measure allocation counts or prove a byte-exact memory ceiling.

Legacy mode is included only as an on-machine cost comparator on valid bounded inputs. It is not a safety oracle and must not replace bounded production mode. Oversized-input rejection and cancellation correctness stay in the independent safety corpus; never infer correctness from equal timings or event counts.
