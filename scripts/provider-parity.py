#!/usr/bin/env python3
"""Fetch pinned references and run their unchanged, locked baseline checks.

Only writes under the explicitly supplied reference directory. A failed check
is recorded, never converted into candidate parity evidence.
"""

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import time


ROOT = Path(__file__).resolve().parents[1]
REFERENCES = ROOT / "docs/provider-parity/references.json"


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def fetch(directory, name, reference):
    destination = directory / reference.get("checkoutDirectory", name)
    marker = directory / f"{name}.reference.json"
    identity = {key: reference[key] for key in ("repository", "commit")}
    if destination.exists():
        if not marker.exists() or json.loads(marker.read_text()) != identity:
            raise ValueError(f"refusing unverified existing checkout: {destination}")
    else:
        # Upstream baseline guards inspect git ls-files and the real revision.
        subprocess.run(["gh", "repo", "clone", reference["repository"], str(destination), "--", "--no-checkout"], check=True)
        subprocess.run(["git", "checkout", "--detach", reference["commit"]], cwd=destination, check=True)
        marker.write_text(json.dumps(identity, indent=2) + "\n")
    actual = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=destination, text=True).strip()
    if actual != reference["commit"]:
        raise ValueError(f"{name}: reference revision changed")
    # Generated ignored files are allowed; modifications to tracked inputs are not.
    subprocess.run(["git", "diff", "--exit-code", "HEAD"], cwd=destination, check=True, stdout=subprocess.DEVNULL)
    untracked = subprocess.check_output(["git", "ls-files", "--others", "--exclude-standard"], cwd=destination, text=True)
    if untracked.strip():
        raise ValueError(f"{name}: untracked reference inputs")


def baseline(directory, name, reference):
    destination = directory / reference.get("checkoutDirectory", name)
    before = {lock: digest(destination / lock) for lock in reference["locks"]}
    records = []
    for index, command in enumerate(reference["install"] + reference["checks"]):
        log = directory / f"{name}-{index}.log"
        started = time.time()
        with log.open("w") as output:
            result = subprocess.run(command, cwd=destination, stdout=output, stderr=subprocess.STDOUT)
        records.append({"command": command, "exitCode": result.returncode,
                        "elapsedSeconds": round(time.time() - started, 3),
                        "log": log.name, "logSha256": digest(log)})
        print(f"{name}: {'PASS' if result.returncode == 0 else 'FAIL'} {' '.join(command)}", flush=True)
        if result.returncode:
            break
    after = {lock: digest(destination / lock) for lock in reference["locks"]}
    evidence = {"repository": reference["repository"], "commit": reference["commit"],
                "locksBefore": before, "locksAfter": after, "commands": records,
                "passed": before == after and all(row["exitCode"] == 0 for row in records)}
    (directory / f"{name}.baseline.json").write_text(json.dumps(evidence, indent=2) + "\n")
    if before != after:
        raise ValueError(f"{name}: baseline changed a lockfile")
    return evidence["passed"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["fetch", "baseline"])
    parser.add_argument("directory", type=Path)
    parser.add_argument("--provider", choices=["antigravity", "multiprovider", "cursor"])
    args = parser.parse_args()
    directory = args.directory.resolve()
    directory.mkdir(parents=True, exist_ok=True)
    references = json.loads(REFERENCES.read_text())["providers"]
    passed = True
    for name, reference in references.items():
        if args.provider and name != args.provider:
            continue
        fetch(directory, name, reference)
        if args.action == "baseline":
            passed = baseline(directory, name, reference) and passed
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
