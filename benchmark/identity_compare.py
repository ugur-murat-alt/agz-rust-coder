#!/usr/bin/env python3
"""Compare two read-only identity_measure binaries on identical, generated inputs.

Both binaries must use the same Rust toolchain and Cargo profile. This command
measures input identity only: not Cargo builds, LLM tokens, or end-to-end tasks.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import tempfile


def compare(a: dict, b: dict) -> dict:
    for field in ("schemaVersion", "stage", "fixture", "files", "warmup"):
        if a.get(field) != b.get(field):
            raise ValueError(f"incomparable measurement: {field}")
    x, y = a["samplesMs"], b["samplesMs"]
    if not 5 <= len(x) == len(y) <= 100:
        raise ValueError("equal sample counts in 5..100 are required")
    if any(not isinstance(v, (int, float)) or not math.isfinite(v) or v <= 0 for v in x + y):
        raise ValueError("measurements must be finite and positive")
    if len(a["hashes"]) != len(x) or len(b["hashes"]) != len(y):
        raise ValueError("every sample needs its input hash")
    if len(set(a["hashes"] + b["hashes"])) != 1:
        raise ValueError("input identity differs: reject the speed comparison")
    old, new = statistics.median(x), statistics.median(y)
    return {"files": a["files"], "inputHash": a["hashes"][0],
            "baselineMedianMs": old, "candidateMedianMs": new,
            "baselineP95Ms": sorted(x)[math.ceil(len(x) * .95) - 1],
            "candidateP95Ms": sorted(y)[math.ceil(len(y) * .95) - 1],
            "reductionPercent": (old - new) / old * 100,
            "baselineSamplesMs": x, "candidateSamplesMs": y}


def measure(binary: Path, fixture: Path, samples: int) -> dict:
    result = subprocess.run([str(binary), str(fixture), str(samples)],
                            capture_output=True, check=True, timeout=180, env=os.environ.copy())
    if len(result.stdout) > 1_000_000:
        raise ValueError("measurement output exceeds 1MB")
    return json.loads(result.stdout)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--samples", type=int, default=15, choices=range(5, 101))
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    binaries = [p.resolve(strict=True) for p in (args.baseline, args.candidate)]
    rows = []
    with tempfile.TemporaryDirectory(prefix="agz-identity-benchmark-") as temp:
        for count in (1000, 5000, 10000):
            fixture = Path(temp) / str(count)
            source = fixture / "src"
            source.mkdir(parents=True)
            manifest = fixture / "Cargo.toml"
            manifest.write_text('[package]\nname="identity-fixture"\nversion="0.1.0"\n[workspace]\n')
            for index in range(count):
                (source / f"file_{index:05}.rs").write_text(f"pub fn value_{index}() -> usize {{ {index} }}\n")
            for scenario in ("unchanged", "single-rust-file", "manifest"):
                if scenario == "single-rust-file":
                    (source / "file_00000.rs").write_text("pub fn value_0() -> usize { 42 }\n")
                elif scenario == "manifest":
                    manifest.write_text(manifest.read_text() + '# changed manifest\n')
                # Alternate the order between scenarios to reduce simple order bias.
                order = (0, 1) if len(rows) % 2 == 0 else (1, 0)
                runs = {i: measure(binaries[i], fixture, args.samples) for i in order}
                rows.append({"scenario": scenario, "order": list(order), **compare(runs[0], runs[1])})
    report = {"schemaVersion": 1, "stage": "input-identity", "platform": platform.platform(),
              "binarySha256": [hashlib.sha256(p.read_bytes()).hexdigest() for p in binaries],
              "warmup": 3, "samplesPerScenario": args.samples, "rows": rows,
              "limitations": "Single host, warm filesystem cache, synthetic no-Git fixture; not a total build or agent speed claim."}
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"rows": len(rows), "allInputHashesMatched": True}))


if __name__ == "__main__":
    main()
