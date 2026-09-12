"""Measure fresh Cargo requests and separate queue/process latency.

The two debounce settings diagnose the wait contribution on one binary. Use
--baseline-report for a source change under the same settings and fixture.
Every recorded request must execute Cargo and the same single lib test.
queueMs includes admission/preflight and scheduler waiting. Unexposed admission
and preflight fields remain null rather than being inferred as zero.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import tempfile
import time

from mcp_stdio import McpSession


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def fixture(root):
    files = {
        "Cargo.toml": '[package]\nname="latency-fixture"\nversion="0.1.0"\nedition="2024"\n[workspace]\n',
        "Cargo.lock": 'version=4\n[[package]]\nname="latency-fixture"\nversion="0.1.0"\n',
        "src/lib.rs": '#[test] fn current_source_is_tested() { assert_eq!(2 + 2, 4); }\n',
    }
    for name, contents in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        if path.exists() and path.read_text() != contents:
            raise ValueError(f"fixture changed: {name}")
        if not path.exists():
            path.write_text(contents)
    return {name: digest(root / name) for name in files}


def run(binary, cargo, root, debounce):
    env = {k: v for k, v in os.environ.items() if not k.startswith("AGZ_RUST_MCP_")}
    env.pop("CARGO_TARGET_DIR", None)
    env.update(RUSTUP_TOOLCHAIN="1.88.0", CARGO_NET_OFFLINE="true", CARGO_INCREMENTAL="0",
               RUSTC_WRAPPER="", RUST_BALANCED_SCCACHE="off")
    with tempfile.TemporaryDirectory(prefix="agz-request-latency-") as directory:
        state = Path(directory)
        command = [str(binary), "--allow-root", str(root), "--cargo-path", str(cargo),
                   "--gate-cache", "isolated", "--gate-cache-dir", str(state / "gate"),
                   "--gate-lease-dir", str(state / "leases"), "--telemetry-enabled", "false",
                   "--gate-debounce-ms", str(debounce)]
        rows = []
        with McpSession(command, cwd=root, env=env, stderr=state / "stderr.log") as client:
            for cache in ("cold", "warm"):
                started = time.monotonic()
                response, size = client.call("check", {
                    "dir": str(root), "target": "test", "detail": "compact",
                    "options": {"packages": ["latency-fixture"], "cargoTarget": {"kind": "lib"}},
                })
                elapsed = (time.monotonic() - started) * 1000
                result = response["structuredContent"]
                assert result["status"] == "FAST_PASS", str(result)[:4000]
                data = result["data"]
                assert len(data["steps"]) == 1
                step = data["steps"][0]
                assert step["exitCode"] == 0 and step["evidence"]["testsExecuted"] == 1
                assert step["evidence"]["buildSuccess"]
                rows.append({
                    "cache": cache, "debounceMs": debounce, "wallMs": round(elapsed, 2),
                    "responseMs": data["responseMs"], "queueMs": data["queueMs"],
                    "admissionMs": data.get("admissionMs"), "preflightMs": data.get("preflightMs"),
                    "cargoMs": step["durationMs"], "wireBytes": size,
                    "jobId": data["jobId"], "command": step["command"], "evidence": step["evidence"],
                })
        assert rows[0]["jobId"] != rows[1]["jobId"], "completed result reused"
        return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--cargo", type=Path, required=True)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--baseline-report", type=Path)
    args = parser.parse_args()
    root = args.fixture.resolve()
    hashes = fixture(root)
    rows = []
    for repetition in range(3):
        for debounce in ((500, 0) if repetition % 2 == 0 else (0, 500)):
            rows.extend(run(args.binary.resolve(), args.cargo.resolve(), root, debounce))
    assert fixture(root) == hashes
    summary = {}
    for debounce in (500, 0):
        summary[str(debounce)] = {}
        for cache in ("cold", "warm"):
            selected = [r for r in rows if r["debounceMs"] == debounce and r["cache"] == cache]
            summary[str(debounce)][cache] = {key: statistics.median(r[key] for r in selected)
                for key in ("wallMs", "cargoMs", "queueMs")}
    report = {"binarySha256": digest(args.binary), "fixtureSha256": hashes,
              "rust": "1.88.0", "sccache": False, "incremental": False,
              "samples": rows, "summary": summary}
    if args.baseline_report:
        baseline = json.loads(args.baseline_report.read_text())
        for key in ("fixtureSha256", "rust", "sccache", "incremental"):
            assert baseline[key] == report[key], f"conditions differ: {key}"
        assert len(baseline["samples"]) == len(rows) == 12
        for before, after in zip(baseline["samples"], rows):
            for key in ("cache", "debounceMs", "evidence", "command"):
                assert before[key] == after[key], f"comparison differs: {key}"
        report["comparison"] = {"baselineBinarySha256": baseline["binarySha256"],
                                "before": baseline["summary"], "after": summary}
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(summary))


if __name__ == "__main__":
    main()
