# Opt-in performance profile experiment

This compares the same revision, feature bundle, application entry point and
profile settings at optimization levels `z`, `2` and `3`. It uses fresh Meow
processes and an in-process TCP echo server, all on IPv4 loopback. It does not
access external services, subscriptions, real configurations or traffic. The
comparison says nothing about why an older M4 measurement differed.

The application profile is opt-in; the ordinary release profile and minimal
feature selection retain their existing semantics. Build and benchmark only
when other full Rust compiles and heavy jobs are idle. On shared machines,
coordinate externally: a container PID namespace may hide other jobs. Keep
25 GiB free and use one or two Cargo jobs. The build helper stops if free
space approaches the reserve. All outputs below are local, untracked artifacts;
record the commands, hashes, environment and results for review.
See the [build guide](performance-profiles.md) for the profile settings,
feature bundles and build-helper details.

From the repository root, for the full desktop-style feature bundle:

```bash
CARGO_BUILD_JOBS=1 bash scripts/build-performance-profiles.sh target/profile-build full
CARGO_BUILD_JOBS=1 cargo build --offline --locked --profile performance -p meow-bench --example profile_interleaved
python3 bench/run_profile.py \
  --binary target/profile-build/meow-z \
  --binary target/profile-build/meow-2 \
  --binary target/profile-build/meow-3 \
  --harness target/performance/examples/profile_interleaved \
  --output target/profile-trials --trials 3 --rounds 6 --connections 1000
python3 bench/profile_summary.py target/profile-trials/trial-{1,2,3}.json > target/profile-trials/summary.json
```

Both output directories must not exist. For a quicker smoke test use a new
output directory and `--trials 1 --rounds 6 --connections 10`. It still runs
the full throughput workload and should not be called a formal result. The
build helper records the exact commands, feature bundle, binary hashes and
sizes. Use `minimal` as the build helper's second argument to compare that
bundle separately; do not mix feature bundles within one comparison. Desktop
Linux data are not Android measurements. Run an appropriately built harness
and the same experiment on the actual Android device to establish Android
results; a cross-compiled artifact alone only provides its size.

The launcher reserves distinct ephemeral loopback ports, checks that each
listening socket belongs to its own child PID, writes isolated configurations
and home directories, then starts the benchmark. Each trial starts fresh
processes. It captures the actual command, process logs, executable hashes,
toolchain and host environment. A failed or timed-out trial exits nonzero and
its partial output remains `.incomplete`; it is not a valid sample. The
summary script verifies every round and variant is present exactly once.
Keep the raw trial JSON and logs alongside the summary, including failures.

The example warms each variant, then alternates all six orders per six rounds.
Each short test opens a new SOCKS connection, sends one byte, checks its exact
echo, and reports rate and latency percentiles. The throughput workload
extends the #746 workload: 4 KiB × 10,000 round trips on one connection;
ten sequential 1 MiB full-duplex echo transfers; 64 MiB on one connection;
and a concurrent multiflow transfer. It checks echoed content and length,
fails on errors, and retains every successful measurement. Throughput counts
both upload and echo bytes; consult the raw samples for exact byte totals.

CPU is proxy-process user plus system time from Linux `/proc` over each whole
short or throughput phase, divided by phase wall time; 100% represents one
core. It includes all activity by that proxy during the phase and excludes the
client and echo server. CPU tick granularity limits short intervals. RSS is
sampled at phase boundaries; VmHWM is the process-lifetime high-water mark,
not a per-phase peak. The summary reports all samples with median, minimum,
maximum and sample standard deviation; p99 values describe latency within
each sample, not a pooled distribution. Repetition and shared-host activity can
dominate small differences, and loopback does not represent an actual network.
