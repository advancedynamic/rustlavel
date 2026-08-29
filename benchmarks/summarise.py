#!/usr/bin/env python3
"""Turn a directory of oha output into a table somebody can read.

Reports the median of the repeated runs rather than the best, and prints p99
beside throughput. A framework that serves 100,000 requests a second and stalls
one in a hundred for half a second is not fast, and a mean would hide that.
"""

import json
import pathlib
import statistics
import sys

CASES = [
    ("plaintext", "Plaintext"),
    ("json", "JSON"),
    ("routing", "Routing"),
    ("middleware", "Middleware ×5"),
    ("json-big", "JSON ×100"),
    ("db-user", "DB one row"),
    ("db-posts", "DB relations"),
    ("template", "Template"),
]


def load(directory: pathlib.Path):
    """{app: {case: {"rps": [...], "p99": [...]}}} plus {app: meta}."""
    results, meta = {}, {}

    for path in sorted(directory.glob("*.json")):
        parts = path.stem.split(".")

        if len(parts) == 2 and parts[1] == "meta":
            meta[parts[0]] = json.loads(path.read_text())
            continue
        if len(parts) != 3:
            continue

        app, case, _run = parts
        try:
            data = json.loads(path.read_text())
        except json.JSONDecodeError:
            continue

        summary = data.get("summary", {})
        latency = data.get("latencyPercentiles", {})
        slot = results.setdefault(app, {}).setdefault(case, {"rps": [], "p99": []})
        slot["rps"].append(summary.get("requestsPerSec", 0.0))
        # oha reports seconds; milliseconds is what anyone reading this thinks in.
        slot["p99"].append(latency.get("p99", 0.0) * 1000)

    return results, meta


def column(value, width=13):
    return f"{value:>{width}}"


def main():
    directory = pathlib.Path(sys.argv[1])
    results, meta = load(directory)

    if not results:
        print(f"No results in {directory}")
        return

    apps = sorted(results)
    print("# Benchmark results\n")
    print(f"Run from `{directory.name}`. Median of the repeated runs.")
    print("Requests per second, higher is better; p99 latency in milliseconds,")
    print("lower is better. Both matter — throughput alone hides a long tail.\n")

    header = "| Case " + "".join(f"| {app} " for app in apps) + "|"
    divider = "|---" * (len(apps) + 1) + "|"
    print(header)
    print(divider)

    for key, label in CASES:
        cells = []
        for app in apps:
            slot = results.get(app, {}).get(key)
            if not slot or not slot["rps"]:
                cells.append("—")
                continue
            rps = statistics.median(slot["rps"])
            p99 = statistics.median(slot["p99"])
            cells.append(f"{rps:,.0f} <br><small>p99 {p99:.1f} ms</small>")
        print(f"| {label} " + "".join(f"| {cell} " for cell in cells) + "|")

    if meta:
        print("\n## Outside the request path\n")
        print("| | " + " | ".join(apps) + " |")
        print("|---" * (len(apps) + 1) + "|")
        for label, field, unit in [
            ("Startup", "startup_ms", "ms"),
            ("Memory (RSS)", "memory_mb", "MB"),
            ("Artifact", "artifact_mb", "MB"),
        ]:
            cells = []
            for app in apps:
                value = meta.get(app, {}).get(field, "—")
                cells.append(f"{value} {unit}" if value != "—" else "—")
            print(f"| {label} | " + " | ".join(cells) + " |")

    print("\n## Reading these honestly\n")
    print("- Measured on one developer machine, not a server. The *ratios* between")
    print("  the apps mean something; the absolute numbers do not travel.")
    print("- PostgreSQL runs in Docker, which on macOS means a virtual machine.")
    print("  Every database row is therefore pessimistic in the same direction for")
    print("  every app, which keeps the comparison fair but the absolutes wrong.")
    print("- The load generator shares a machine with the thing it is loading, so")
    print("  the fastest cases are partly measuring `oha`.")
    print("- Written by the author of one of the frameworks being compared. Treat")
    print("  a result flattering that one with more suspicion than the rest.")


if __name__ == "__main__":
    main()
