#!/usr/bin/env bash
# Build the same meow entry point/features at z, 2 and 3, sequentially.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
if [[ $# -ne 2 || ( $2 != full && $2 != minimal ) ]]; then
    echo "Usage: $0 NEW_OUTPUT_DIRECTORY {full|minimal}" >&2
    exit 2
fi
out=$1
features=$2
jobs=${CARGO_BUILD_JOBS:-1}
if [[ $jobs != 1 && $jobs != 2 ]]; then
    echo 'CARGO_BUILD_JOBS must be 1 or 2.' >&2
    exit 2
fi
# Make the target path explicit and shared across builds, avoiding a second
# debug tree or three independent dependency trees. Never delete anything.
target=${CARGO_TARGET_DIR:-"$root/target"}
mkdir -p "$target"
target=$(cd "$target" && pwd)
if ! mkdir "$out"; then
    echo "Output directory must be new: $out" >&2
    exit 2
fi
out=$(cd "$out" && pwd)
if command -v sha256sum >/dev/null; then
    hash=(sha256sum)
elif command -v shasum >/dev/null; then
    hash=(shasum -a 256)
else
    echo 'sha256sum or shasum is required.' >&2
    exit 2
fi
# Reject hidden compiler/profile tuning; keep these builds comparable and
# portable rather than silently using a machine-specific instruction set.
for name in RUSTFLAGS CARGO_ENCODED_RUSTFLAGS RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER; do
    if [[ -n ${!name:-} ]]; then
        echo "Unset $name before running this comparison." >&2
        exit 2
    fi
done
while IFS='=' read -r name _; do
    if [[ $name == CARGO_PROFILE_PERFORMANCE_* ]]; then
        echo "Unset $name before running this comparison." >&2
        exit 2
    fi
done < <(env)

check_space() {
    local path=$1 free_kib
    free_kib=$(df -Pk "$path" | awk 'NR == 2 {print $4}')
    # 25 GiB reserve plus 4 GiB build headroom. This check is a guard,
    # not a guarantee against other jobs consuming the same filesystem.
    if [[ ! $free_kib =~ ^[0-9]+$ || $free_kib -lt 30408704 ]]; then
        echo "Need at least 29 GiB free on $path (25 GiB reserve + 4 GiB headroom)." >&2
        exit 1
    fi
}
check_space "$target"
check_space "$out"
{
    printf 'revision: '; git rev-parse HEAD
    printf 'feature bundle: %s\n' "$features"
    printf 'jobs: %s\ntarget directory: %s\n' "$jobs" "$target"
    printf 'host: '; uname -a
    rustc -vV
    cargo -V
    printf '\nworking tree status:\n'; git status --short
    printf '\nCargo.lock SHA-256:\n'; "${hash[@]}" Cargo.lock
} > "$out/build-environment.txt"
git diff --binary HEAD > "$out/tracked-changes.patch"
# Preserve identity of untracked files too, without copying secrets or a
# potentially enormous tree into the results directory.
while IFS= read -r -d '' path; do
    [[ $root/$path == "$out/"* ]] && continue
    "${hash[@]}" "$path"
done \
    < <(git ls-files --others --exclude-standard -z) > "$out/untracked-sha256.txt"

for level in z 2 3; do
    check_space "$target"
    check_space "$out"
    printf 'CARGO_BUILD_JOBS=%q CARGO_TARGET_DIR=%q CARGO_PROFILE_PERFORMANCE_OPT_LEVEL=%q cargo build --offline --locked --profile performance -p meow-app --bin meow --no-default-features --features %q\n' \
        "$jobs" "$target" "$level" "$features" | tee -a "$out/commands.txt"
    CARGO_BUILD_JOBS=$jobs CARGO_TARGET_DIR=$target CARGO_PROFILE_PERFORMANCE_OPT_LEVEL=$level \
        python3 - "$target" "$out" "$out/build-$level.log" "$features" <<'PY'
import os
import shutil
import signal
import subprocess
import sys
import time

target, out, log, features = sys.argv[1:]
command = ["cargo", "build", "--offline", "--locked", "--profile", "performance",
           "-p", "meow-app", "--bin", "meow", "--no-default-features", "--features", features]
# Leave a 1 GiB cushion for writes between polls and during shutdown.
stop_at = 26 * 1024**3
with open(log, "w", encoding="utf-8") as stream:
    process = subprocess.Popen(command, stdout=stream, stderr=subprocess.STDOUT,
                               start_new_session=True)
    try:
        while process.poll() is None:
            if min(shutil.disk_usage(target).free, shutil.disk_usage(out).free) < stop_at:
                print("Less than 26 GiB free; stopping Cargo process group to protect the 25 GiB reserve.", file=sys.stderr)
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
                sys.exit(1)
            time.sleep(0.25)
    except BaseException:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
        raise
    if process.returncode:
        print(f"Cargo failed ({process.returncode}); see {log}", file=sys.stderr)
        sys.exit(1)
PY
    exe="$target/performance/meow"
    [[ -f $exe ]] || exe="$exe.exe"
    cp "$exe" "$out/meow-$level"
    { printf 'bytes '; wc -c < "$out/meow-$level"; "${hash[@]}" "$out/meow-$level"; } \
        > "$out/meow-$level.txt"
done
