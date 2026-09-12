"""Measure actual MCP response bytes against one unchanged compiler fixture.

Use --binary twice (baseline then candidate) with the same --fixture directory.
Pass the first report as --baseline-report on the candidate run to verify that
compiler evidence is unchanged before reporting any response-size reduction.
This measures transport volume, not token use or compiler/runtime speed.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import tempfile


from mcp_stdio import McpSession


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def fixture_at(path):
    path.mkdir(parents=True, exist_ok=True)
    sources = {
        "Cargo.toml": '[package]\nname="compact-fixture"\nversion="0.1.0"\nedition="2024"\n',
        "Cargo.lock": 'version = 4\n[[package]]\nname = "compact-fixture"\nversion = "0.1.0"\n',
        "src/lib.rs": "\n".join(
            f'pub fn failure_{i}() -> u8 {{ "wrong-type-{i}" }}' for i in range(40)
        ) + "\n",
    }
    for name, content in sources.items():
        target = path / name
        target.parent.mkdir(parents=True, exist_ok=True)
        if target.exists():
            if target.read_text() != content:
                raise ValueError(f"fixture changed: {name}")
        else:
            target.write_text(content)
    return {name: digest(path / name) for name in sources}


def sample(binary, cargo, fixture, state):
    env = {k: v for k, v in os.environ.items() if not k.startswith("AGZ_RUST_MCP_")}
    env.pop("CARGO_TARGET_DIR", None)
    env["RUSTUP_TOOLCHAIN"] = "1.88.0"
    args = [str(binary), "--allow-root", str(fixture), "--cargo-path", str(cargo),
            "--gate-cache", "isolated", "--gate-cache-dir", str(state / "gate"),
            "--gate-lease-dir", str(state / "leases"), "--telemetry-enabled", "false"]
    with McpSession(args, cwd=fixture, env=env, stderr=state / "stderr.log") as client:
        result, size = client.call("check", {"dir": str(fixture), "detail": "compact"})
        data = result["structuredContent"]
        assert data["status"] == "FAIL", (data["status"], data["data"].get("reason"))
        step = data["data"]["steps"][0]
        assert step["evidence"]["buildSuccess"] is False
        diagnostics = step["diagnostics"]
        assert len(diagnostics) == 5
        assert {d["code"] for d in diagnostics} == {"E0308"}
        return {"wireBytes": size, "truncated": data["truncated"],
                "status": data["status"], "diagnostics": diagnostics,
                "omitted": step["diagnosticsOmitted"], "evidence": step["evidence"]}


def compare_reports(baseline, report):
    if baseline["fixtureSha256"] != report["fixtureSha256"] or len(baseline["samples"]) != len(report["samples"]):
        raise ValueError("baseline fixture or sample count does not match")
    for before, after in zip(baseline["samples"], report["samples"]):
        for key in ("status", "diagnostics", "omitted", "evidence"):
            if before[key] != after[key]:
                raise ValueError(f"compiler evidence changed: {key}")
    before = statistics.median(s["wireBytes"] for s in baseline["samples"])
    after = statistics.median(s["wireBytes"] for s in report["samples"])
    if before <= 0:
        raise ValueError("baseline response size must be positive")
    return {
        "baselineBinarySha256": baseline["binarySha256"],
        "compilerEvidenceIdentical": True,
        "baselineMedianBytes": before, "candidateMedianBytes": after,
        "reductionPercent": round(100 * (before - after) / before, 2),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--cargo", type=Path, required=True)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--baseline-report", type=Path)
    args = parser.parse_args()
    fixture = args.fixture.resolve()
    hashes = fixture_at(fixture)
    with tempfile.TemporaryDirectory(prefix="agz-compact-state-") as directory:
        samples = [sample(args.binary.resolve(), args.cargo.resolve(), fixture, Path(directory))
                   for _ in range(3)]
    assert fixture_at(fixture) == hashes
    report = {"metric": "MCP JSON-RPC response bytes, not tokens or runtime speed",
              "binarySha256": digest(args.binary), "fixtureSha256": hashes, "samples": samples}
    if args.baseline_report:
        baseline = json.loads(args.baseline_report.read_text())
        report["comparison"] = compare_reports(baseline, report)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"wireBytes": [s["wireBytes"] for s in samples],
                      "truncated": [s["truncated"] for s in samples],
                      "comparison": report.get("comparison")}))


if __name__ == "__main__":
    main()
