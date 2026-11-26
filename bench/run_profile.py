#!/usr/bin/env python3
"""Launch isolated Meow processes and run profile_interleaved on Linux loopback."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import socket
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parent.parent
CONFIG = """mixed-port: {port}
allow-lan: false
bind-address: "127.0.0.1"
mode: direct
log-level: warning
ipv6: false
max-connections: 0
dns:
  enable: false
"""


def command(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def digest(path):
    h = hashlib.sha256()
    with open(path, "rb") as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def optional_text(path):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return None


def owned_listener(pid, port):
    # Verify the LISTEN socket belongs to our process, not another process
    # that won the small race after the reserved socket was released.
    inodes = set()
    for fd in Path(f"/proc/{pid}/fd").iterdir():
        try:
            link = os.readlink(fd)
        except FileNotFoundError:
            continue
        if link.startswith("socket:["):
            inodes.add(link[8:-1])
    for line in Path("/proc/net/tcp").read_text().splitlines()[1:]:
        fields = line.split()
        address, port_hex = fields[1].split(":")
        if address == "0100007F" and int(port_hex, 16) == port and fields[3] == "0A":
            return fields[9] in inodes
    return False


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", action="append", required=True, help="Meow binary in variant order; supply 2 or 3 times")
    parser.add_argument("--harness", required=True, help="profile_interleaved example executable")
    parser.add_argument("--output", required=True, help="new output directory")
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--rounds", type=int, default=6)
    parser.add_argument("--connections", type=int, default=1000)
    parser.add_argument("--timeout", type=int, default=1800, help="per-trial harness timeout, seconds")
    args = parser.parse_args()
    if len(args.binary) not in (2, 3) or min(args.trials, args.rounds, args.connections, args.timeout) < 1:
        parser.error("supply 2 or 3 binaries and positive trial parameters")
    if args.rounds % 6:
        parser.error("rounds must be a multiple of 6 to balance order")
    binaries = [Path(name).resolve(strict=True) for name in args.binary]
    harness = Path(args.harness).resolve(strict=True)
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=False)
    metadata = {
        "revision": command("git", "rev-parse", "HEAD"),
        "git_status": command("git", "status", "--short"),
        "git_diff_sha256": hashlib.sha256(subprocess.check_output(["git", "diff", "--binary"], cwd=ROOT)).hexdigest(),
        "binaries": [{"path": str(p), "bytes": p.stat().st_size, "sha256": digest(p)} for p in binaries],
        "harness": {"path": str(harness), "bytes": harness.stat().st_size, "sha256": digest(harness)},
        "benchmark_sources_sha256": {str(p.relative_to(ROOT)): digest(p) for p in [ROOT / "bench/run_profile.py", ROOT / "bench/profile_summary.py", ROOT / "crates/meow-bench/examples/profile_interleaved.rs", ROOT / "crates/meow-bench/src/bench_throughput.rs"]},
        "platform": platform.uname()._asdict(),
        "cpu": Path("/proc/cpuinfo").read_text().split("\n\n")[0],
        "memory": Path("/proc/meminfo").read_text(),
        "load_before": Path("/proc/loadavg").read_text().strip(),
        "affinity": sorted(os.sched_getaffinity(0)),
        "cpu_governor": optional_text("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
        "cgroup": optional_text("/proc/self/cgroup"),
        "rustc": command("rustc", "-Vv"), "cargo": command("cargo", "-V"),
        "settings": vars(args),
        "build_env": {k: os.environ.get(k) for k in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET", "CARGO_BUILD_JOBS")},
    }
    (output / "environment.json").write_text(json.dumps(metadata, indent=2) + "\n")
    for trial in range(1, args.trials + 1):
        processes, logs, reservations = [], [], []
        try:
            for _ in binaries:
                sock = socket.socket()
                sock.bind(("127.0.0.1", 0))
                reservations.append(sock)
            ports = [sock.getsockname()[1] for sock in reservations]
            for i, (binary, port, reservation) in enumerate(zip(binaries, ports, reservations)):
                home = output / f"trial-{trial}-variant-{i}"
                home.mkdir()
                (home / "config.yaml").write_text(CONFIG.format(port=port))
                log = (home / "meow.log").open("wb")
                logs.append(log)
                reservation.close()
                proc = subprocess.Popen([str(binary), "-d", str(home), "-f", str(home / "config.yaml")], cwd=home, stdout=log, stderr=subprocess.STDOUT)
                processes.append(proc)
                deadline = time.monotonic() + 20
                while not owned_listener(proc.pid, port):
                    if proc.poll() is not None:
                        raise RuntimeError(f"variant {i} exited {proc.returncode} before listening")
                    if time.monotonic() >= deadline:
                        raise RuntimeError(f"variant {i} did not own loopback listener on {port}")
                    time.sleep(0.05)
            cmd = [str(harness), *[f"127.0.0.1:{port}" for port in ports], "--pids", ",".join(str(p.pid) for p in processes), "--rounds", str(args.rounds), "--connections", str(args.connections)]
            (output / f"trial-{trial}-command.json").write_text(json.dumps(cmd, indent=2) + "\n")
            temporary = output / f"trial-{trial}.incomplete"
            with temporary.open("wb") as out, (output / f"trial-{trial}-harness.log").open("wb") as err:
                subprocess.run(cmd, cwd=output, stdout=out, stderr=err, timeout=args.timeout, check=True)
            if any(p.poll() is not None for p in processes):
                raise RuntimeError("a proxy exited during the trial")
            data = json.loads(temporary.read_text())
            if data["rounds"] != args.rounds or data["connections_per_sample"] != args.connections or data["pids"] != [p.pid for p in processes] or data["proxies"] != [f"127.0.0.1:{port}" for port in ports]:
                raise RuntimeError("benchmark output does not match requested trial")
            expected = {(r, v) for r in range(args.rounds) for v in range(len(binaries))}
            actual = [(s["round"], s["variant"]) for s in data["samples"]]
            if len(actual) != len(expected) or set(actual) != expected:
                raise RuntimeError("incomplete or duplicate benchmark samples")
            # Reject malformed results before publishing the trial as complete.
            with (output / f"trial-{trial}-validation.log").open("wb") as validation_log:
                subprocess.run([sys.executable, str(ROOT / "bench/profile_summary.py"), str(temporary)], stdout=subprocess.DEVNULL, stderr=validation_log, check=True)
            temporary.rename(output / f"trial-{trial}.json")
        finally:
            for sock in reservations:
                sock.close()
            for proc in processes:
                if proc.poll() is None:
                    proc.terminate()
            for proc in processes:
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
            for log in logs:
                log.close()
    (output / "load-after.txt").write_text(Path("/proc/loadavg").read_text())
    print(f"Completed {args.trials} trials in {output}")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"benchmark failed: {error}", file=sys.stderr)
        sys.exit(1)
