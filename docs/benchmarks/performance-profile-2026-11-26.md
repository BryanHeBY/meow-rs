# Performance profile experiment, 2026-11-26

Base revision: `800d11c53cb5e591dd9b524a0a3a7a666fc71f44`, with the
performance-profile and benchmark changes in this worktree. Native host:
Linux 7.2.14 x86_64 on KVM, 30 visible CPUs, AMD Ryzen Threadripper PRO
9985WX, 30,785,076 KiB reported memory; Rust 1.98.1, LLVM 22.1.8. The host is
shared and other jobs were active;
the sandbox's PID namespace cannot show whether their full builds have ended.

The sandbox denied `socket.socket()` with `PermissionError: [Errno 1]
Operation not permitted`, even before a localhost bind. Consequently no valid
loopback measurements, CPU/RSS, short-connection or throughput results were
collected here. Android was not measured. There is no evidence here for a
causal explanation of the historical M4 difference.

Build and measurement commands, workload definitions, failure handling and
environment capture are in [the runbook](performance-profile.md). The build
uses `CARGO_BUILD_JOBS=1` and the `full` bundle. Preserve this distinction
between an unavailable measurement and a numerical result when rerunning.

The native full-feature candidates:

| Optimization | Bytes | SHA-256 |
| --- | ---: | --- |
| z | 18,796,776 | `7c71ee7b218e09066a33648be7034b609aa06d7d216029eb4b484f2fceb9af44` |
| 2 | 27,978,344 | `c1097918ef0fba9aabd65ba63ee2c9810f269c0169286a2cc267b379338295cd` |
| 3 | 30,023,144 | `06382be7810cb4e3915e248d73a69b81e09e8bb6e05eb20d6b891abff753d34d` |

All three passed `meow -d /tmp/meow-profile-validation -f "$PWD/config-bench.yaml" -t`.
Level 2 is 48.8% larger than z; level 3 is 59.7% larger. These sizes are not
performance measurements and do not represent an Android artifact.

`cargo fmt --all -- --check`, the benchmark example's optimized offline build,
its `--help` entry point and `cargo test --offline --locked --profile performance
-p meow-bench --no-run` passed. A synthetic summary was accepted, while a
missing sample and a wrong byte count were rejected. All three compiled
throughput regression tests need sockets and could not be executed here. The
launcher was also invoked with the three built binaries and compiled harness,
one trial, six rounds and ten short connections; it exited with the same
socket `EPERM` before starting the workload. Its captured environment is at
`target/performance-smoke-denied/environment.json` in this worktree.

## Independent Linux loopback rerun

The root rerun completed all three trials with six interleaved rounds and
1,000 short connections per sample. It waited for local compilers to stop and
used only IPv4 loopback inside a fresh network namespace. The following are
medians of 18 samples per variant. These are not Android or WAN results.

| Optimization | Short p95 (us) | 4 KiB x 10,000 (Gb/s) | 1 MiB x 10 (Gb/s) | 64 MiB x 1 (Gb/s) | Four concurrent flows (Gb/s) | Throughput CPU (% of one core) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| z | 205 | 1.95 | 1.88 | 15.1 | 21.0 | 62.5 |
| 2 | 192 | 1.94 | 1.31 | 22.7 | 20.7 | 46.8 |
| 3 | 199 | 1.93 | 2.65 | 14.3 | 20.9 | 46.0 |

The result is workload-specific. Level 2 is promising for the 64 MiB
single-flow scenario but weaker in 1 MiB x 10, while the concurrent medians
are close. Keep normal release settings unchanged and keep the runtime
experiment opt-in. The separate Hysteria2 batching prototype is not included:
its debug loopback reduced syscall count in bursts but spent more elapsed time,
and it uses a 512 KiB receive arena per driver. Production benefit remains
unestablished.
