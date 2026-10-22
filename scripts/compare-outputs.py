#!/usr/bin/env python3
"""Compare recorded JSON results, ignoring object-key order but preserving array order and values."""
import argparse
import json
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("reference", type=Path)
parser.add_argument("candidate", type=Path)
options = parser.parse_args()


def differences(left, right, path="$", output=None):
    output = [] if output is None else output
    if type(left) is not type(right):
        output.append(path + " (type)")
    elif isinstance(left, dict):
        for key in sorted(left.keys() | right.keys()):
            if key not in left or key not in right:
                output.append(path + "." + key + " (missing)")
            else:
                differences(left[key], right[key], path + "." + key, output)
    elif isinstance(left, list):
        if len(left) != len(right):
            output.append(path + " (length)")
        for i, (a, b) in enumerate(zip(left, right)):
            differences(a, b, f"{path}[{i}]", output)
    elif left != right:
        output.append(path)
    return output


reference = {p.name: p for p in options.reference.glob("*.json")}
candidate = {p.name: p for p in options.candidate.glob("*.json")}
reference.pop("commands.json", None)
candidate.pop("commands.json", None)
if not reference:
    parser.error("the reference directory has no recorded outputs")
failed = 0
for name in sorted(reference.keys() | candidate.keys()):
    if name not in reference or name not in candidate:
        print(f"{name}: missing from one recording")
        failed += 1
        continue
    paths = differences(json.loads(reference[name].read_text()), json.loads(candidate[name].read_text()))
    if paths:
        failed += 1
        print(f"{name}: {len(paths)} differences")
        for path in paths[:20]:
            print("  " + path)
        if len(paths) > 20:
            print(f"  ... {len(paths) - 20} more")
total = len(reference.keys() | candidate.keys())
print(f"{total - failed}/{total} outputs match")
raise SystemExit(bool(failed))
