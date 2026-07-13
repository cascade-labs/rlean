#!/usr/bin/env python3
"""Print the workspace crates in dependency (topological) publish order.

Usage:
    cargo metadata --format-version 1 --no-deps | scripts/publish-order.py

The output order is exactly what scripts/publish-crates.sh must publish in: a crate
appears only after every workspace crate it depends on. Paste the result into the
ORDER=( ... ) array in publish-crates.sh whenever the dependency graph changes.
"""
import json
import sys


def main() -> None:
    data = json.load(sys.stdin)
    names = {p["name"] for p in data["packages"]}
    deps = {}
    for p in data["packages"]:
        deps[p["name"]] = {
            d["name"]
            for d in p["dependencies"]
            if d["name"] in names and d["name"] != p["name"]
        }

    order: list[str] = []
    remaining = dict(deps)
    while remaining:
        ready = sorted(n for n, ds in remaining.items() if ds <= set(order))
        if not ready:
            sys.exit(f"dependency cycle among: {sorted(remaining)}")
        for n in ready:
            order.append(n)
            del remaining[n]

    for n in order:
        print(n)


if __name__ == "__main__":
    main()
