#!/usr/bin/env python3
"""Summarize complete profile_interleaved JSON trials without dropping samples."""

import argparse
import json
import math
import statistics
import sys

EXPECTED_BYTES = {
    "4 KB x 10000": 4 * 1024 * 10000 * 2,
    "1 MB x 10": 1024 * 1024 * 10 * 2,
    "64 MB x 1": 64 * 1024 * 1024 * 2,
    "16 MB x 4 concurrent": 16 * 1024 * 1024 * 4 * 2,
}


def number(value, name, *, positive=False):
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value < 0 or (positive and value == 0):
        raise ValueError(f"invalid {name}: {value!r}")
    return value


def validate_sample(sample, connections, hz):
    short = sample["short"]
    if short["connections"] != connections:
        raise ValueError("short connection count differs from configured count")
    elapsed = number(short["elapsed_secs"], "short elapsed", positive=True)
    rate = number(short["connections_per_sec"], "short rate", positive=True)
    if not math.isclose(rate, connections / elapsed, rel_tol=1e-9):
        raise ValueError("short rate does not match count and elapsed time")
    latencies = [number(short[name], name, positive=True) for name in ("min_us", "p50_us", "p95_us", "p99_us", "max_us")]
    if latencies != sorted(latencies):
        raise ValueError("short latency percentiles are out of order")
    tests = sample["throughput"]
    labels = [test["label"] for test in tests]
    if len(labels) != len(EXPECTED_BYTES) or set(labels) != set(EXPECTED_BYTES):
        raise ValueError("throughput workload labels are incomplete or duplicated")
    for test in tests:
        if test["total_bytes"] != EXPECTED_BYTES[test["label"]]:
            raise ValueError(f"wrong byte count for {test['label']}")
        elapsed = number(test["elapsed_secs"], "throughput elapsed", positive=True)
        rate = number(test["gbps"], "throughput rate", positive=True)
        if not math.isclose(rate, test["total_bytes"] * 8 / elapsed / 1e9, rel_tol=1e-9):
            raise ValueError(f"wrong throughput rate for {test['label']}")
    for phase in ("short_process", "throughput_process"):
        process = sample[phase]
        wall = number(process["wall_secs"], phase + " wall", positive=True)
        cpu = number(process["cpu_secs"], phase + " CPU")
        percent = number(process["cpu_percent_one_core"], phase + " percent")
        if not math.isclose(percent, cpu / wall * 100, rel_tol=1e-9, abs_tol=1e-12):
            raise ValueError(f"wrong CPU percentage for {phase}")
        for point in ("before", "after"):
            for key in ("cpu_ticks", "rss_kib", "high_water_kib"):
                number(process[point][key], phase + " " + key)
        delta = process["after"]["cpu_ticks"] - process["before"]["cpu_ticks"]
        if delta < 0 or not math.isclose(cpu, delta / hz, rel_tol=1e-9, abs_tol=1e-12):
            raise ValueError(f"wrong CPU ticks for {phase}")


def distribution(values):
    return {
        "n": len(values),
        "median": statistics.median(values),
        "min": min(values),
        "max": max(values),
        "sample_stdev": statistics.stdev(values) if len(values) > 1 else None,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("trials", nargs="+", help="complete JSON trial files")
    args = parser.parse_args()
    collected = {}
    settings = None
    for path in args.trials:
        with open(path, encoding="utf-8") as source:
            trial = json.load(source)
        variants = len(trial["proxies"])
        connections = trial["connections_per_sample"]
        if variants not in (2, 3) or not isinstance(connections, int) or isinstance(connections, bool) or connections < 1:
            raise ValueError(f"{path}: invalid variant or connection count")
        if not isinstance(trial["rounds"], int) or isinstance(trial["rounds"], bool) or trial["rounds"] < 1:
            raise ValueError(f"{path}: invalid round count")
        hz = number(trial["clock_ticks_per_sec"], "clock ticks", positive=True)
        current_settings = (variants, connections, trial["rounds"], hz)
        if settings is not None and current_settings != settings:
            raise ValueError(f"{path}: trial settings differ")
        settings = current_settings
        samples = trial["samples"]
        if len(samples) != variants * trial["rounds"]:
            raise ValueError(f"{path}: incomplete sample set")
        seen = {(sample["round"], sample["variant"]) for sample in samples}
        expected = {(round_, variant) for round_ in range(trial["rounds"]) for variant in range(variants)}
        if seen != expected:
            raise ValueError(f"{path}: missing or duplicate rounds/variants")
        for sample in samples:
            validate_sample(sample, connections, hz)
            variant = str(sample["variant"])
            metrics = collected.setdefault(variant, {})
            def add(name, value):
                metrics.setdefault(name, []).append(value)
            for name in ("connections_per_sec", "p50_us", "p95_us", "p99_us", "max_us"):
                add("short_" + name, sample["short"][name])
            for test in sample["throughput"]:
                add("throughput_gbps/" + test["label"], test["gbps"])
            for phase in ("short_process", "throughput_process"):
                process = sample[phase]
                add(phase + "_cpu_percent_one_core", process["cpu_percent_one_core"])
                add(phase + "_rss_kib_before", process["before"]["rss_kib"])
                add(phase + "_rss_kib_after", process["after"]["rss_kib"])
                add(phase + "_cumulative_high_water_kib", process["after"]["high_water_kib"])
    result = {variant: {name: distribution(values) for name, values in metrics.items()}
              for variant, metrics in collected.items()}
    json.dump({"trials": args.trials, "variants": result}, sys.stdout, indent=2)
    print()


if __name__ == "__main__":
    main()
