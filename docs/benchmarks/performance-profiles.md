# Opt-in performance profile experiment

The normal `release` profile remains size-optimized, including the existing
minimal/router size check. `performance` inherits its LTO, stripping,
codegen-unit and panic settings, but uses optimization level 3. The release
package overrides for smoltcp, quiche, and shadowsocks remain at level 3 in
all three builds below. This is an experiment, not a change to release or
feature defaults, and is not evidence about any historical M4 comparison.

## Build the three desktop candidates offline

Use one unchanged checkout, installed toolchain, cached dependencies and host
target. Make sure there is no other full Rust compile or benchmark running on
the host, and leave at least 25 GiB free throughout. The script checks for
29 GiB free before each build (25 GiB reserve plus 4 GiB headroom) and polls
both filesystems while building, terminating the Cargo process group if free
space drops below 26 GiB. Other processes or very rapid writes could consume
the remaining cushion before termination; monitor disk independently. The
output directory must not already exist, and Python 3 is required.

```sh
CARGO_BUILD_JOBS=1 bash scripts/build-performance-profiles.sh target/perf-build full
```

The `full` bundle has the same features as the default app build; `minimal`
is an optional separate experiment and must not be mixed with `full` results.
The builds are sequential, share the existing target directory and do not
create a debug build. The script refuses compiler wrappers, Rust flags and
performance-profile environment overrides. Use a clean, consistent Cargo
configuration and build environment (including C/C++ flags). It runs the
following entry point once each with `LEVEL=z`, `2`, then `3`:

```sh
CARGO_BUILD_JOBS=1 CARGO_TARGET_DIR="$PWD/target" \
  CARGO_PROFILE_PERFORMANCE_OPT_LEVEL=LEVEL \
  cargo build --offline --locked --profile performance -p meow-app --bin meow \
  --no-default-features --features full
```

`commands.txt`, compiler and revision metadata, working-tree status, the
tracked patch, hashes of untracked files, build logs, artifact byte counts and
SHA-256 hashes are written to the output directory. Copying each binary before
the next build preserves the three artifacts. Missing cached crates or a
toolchain are errors: provision them separately, then rerun into a new output
directory. `CARGO_BUILD_JOBS=2` is also allowed when host capacity permits.
Check configuration and run the copied binary using the same `meow -f FILE -t`
and `meow -f FILE` entry points as an ordinary deployment.

To build the opt-in variant outside the comparison:

```sh
CARGO_BUILD_JOBS=1 cargo build --offline --locked --profile performance -p meow-app --bin meow
```

The comparison script builds and measures the native host binary; its output
does not establish Android performance. Android needs a separate controlled
build with the actual Android core's features, target, NDK/linker and toolchain,
followed by the same workload on the device; record those details and do not
infer Android results from desktop measurements.
