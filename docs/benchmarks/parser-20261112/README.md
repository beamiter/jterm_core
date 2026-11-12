# First same-machine framing baseline

Measured 2026-11-12 UTC in the cloud workspace, Rust 1.99.1 release build. Base production parser: `8de4951fa9f458c7f1e876510f89685e4160a9e7`; benchmark source hash is in machine.txt. Harness/docs/tests were uncommitted additions at measurement time and are retained with this report. No other collaborating worker was running compilation at sampling; shared-host external contention is unknown. CPU topology is unavailable in this sandbox, not guessed.

224 raw samples, seven per mode/corpus/profile/chunk; approximately 2 MiB each. Below are median bounded times and bounded/legacy median ratios. A ratio above 1 means bounded mode took longer on this workload. These are not UI frame times or end-to-end terminal results. See ../../PARSER_PERFORMANCE_BASELINE.md for methodology.

| Corpus | Profile | Chunk bytes | Bounded median ms | Bounded min–max ms | Median ratio |
|---|---|---:|---:|---:|---:|
| ascii | ember | 1 | 101.255 | 95.904–107.153 | 0.981 |
| ascii | ember | 4096 | 3.330 | 2.589–7.211 | 1.097 |
| ascii | frost | 1 | 137.959 | 135.136–159.705 | 1.002 |
| ascii | frost | 4096 | 4.676 | 4.527–4.749 | 0.946 |
| csi_utf8 | ember | 1 | 112.812 | 106.655–116.680 | 1.001 |
| csi_utf8 | ember | 4096 | 51.309 | 47.808–55.813 | 0.991 |
| csi_utf8 | frost | 1 | 151.204 | 149.378–158.284 | 1.000 |
| csi_utf8 | frost | 4096 | 56.169 | 55.738–59.546 | 0.997 |
| short_osc | ember | 1 | 120.745 | 116.225–134.645 | 1.237 |
| short_osc | ember | 4096 | 8.582 | 7.849–9.145 | 0.899 |
| short_osc | frost | 1 | 136.976 | 133.874–141.055 | 1.288 |
| short_osc | frost | 4096 | 12.312 | 11.376–12.970 | 1.025 |
| dcs_query | ember | 1 | 115.804 | 113.183–125.014 | 1.062 |
| dcs_query | ember | 4096 | 37.344 | 36.314–40.915 | 0.992 |
| dcs_query | frost | 1 | 140.317 | 138.865–141.503 | 1.113 |
| dcs_query | frost | 4096 | 46.273 | 43.727–47.340 | 1.003 |

Short-OSC 1-byte fragmentation has visibly greater bounded-mode cost in this run, while larger feeds behave differently. This is a diagnostic finding, not a universal speedup/regression estimate. No safety cap was relaxed. Absolute timings are not CI gates, and smaller differences require repeated controlled measurements before optimization decisions.
