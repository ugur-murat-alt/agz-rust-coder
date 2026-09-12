"""Measure verify inventory execution, keeping the full requested suite fixed.

Every fixture library, integration target and doctest has one distinct test.
Record repetitions of those tests as well as latency; duplicate execution must
never be mistaken for additional coverage. Both arms use the same source.
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


def fixture_at(root):
    files = {"Cargo.toml": '[workspace]\nmembers=["one","two"]\nresolver="2"\n',
             "Cargo.lock": 'version=4\n[[package]]\nname="verify-one"\nversion="0.1.0"\n[[package]]\nname="verify-two"\nversion="0.1.0"\n'}
    for name in ("one", "two"):
        files[f"{name}/Cargo.toml"] = f'[package]\nname="verify-{name}"\nversion="0.1.0"\nedition="2024"\n'
        files[f"{name}/src/lib.rs"] = (
            f'/// ```\n/// assert_eq!(verify_{name}::answer(), 42);\n/// ```\n'
            f'pub fn answer() -> u8 {{ 42 }}\n'
            f'#[cfg(test)] mod tests {{ #[test] fn unit_{name}() {{ assert_eq!(super::answer(), 42); }} }}\n')
        files[f"{name}/tests/integration.rs"] = f'#[test] fn integration_{name}() {{ assert_eq!(verify_{name}::answer(), 42); }}\n'
    for name, content in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        if path.exists() and path.read_text() != content:
            raise ValueError(f"fixture changed: {name}")
        path.write_text(content)
    return {name: digest(root / name) for name in sorted(files)}


def repetition(binary, cargo, root):
    env = {k: v for k, v in os.environ.items() if not k.startswith("AGZ_RUST_MCP_")}
    env.pop("CARGO_TARGET_DIR", None)
    env.update(RUSTUP_TOOLCHAIN="1.88.0", CARGO_NET_OFFLINE="true", CARGO_INCREMENTAL="0",
               RUSTC_WRAPPER="", RUST_BALANCED_SCCACHE="off")
    with tempfile.TemporaryDirectory(prefix="agz-verify-scope-state-") as directory:
        state = Path(directory)
        command = [str(binary), "--allow-root", str(root), "--cargo-path", str(cargo),
                   "--gate-cache", "isolated", "--gate-cache-dir", str(state / "gate"),
                   "--gate-lease-dir", str(state / "leases"), "--telemetry-enabled", "false"]
        rows = []
        with McpSession(command, cwd=root, env=env, stderr=state / "stderr.log") as client:
            for cache in ("cold", "warm"):
                start = time.monotonic()
                response, size = client.call("verify", {"dir": str(root), "action": "test_run"})
                elapsed = time.monotonic() - start
                result = response["structuredContent"]
                assert result["status"] == "FULL_REQUESTED_SUITE", str(result)[:6000]
                data = result["data"]
                run = data["testRun"]
                inventory = data["testPlan"]["items"]
                assert len(inventory) == 6
                assert run["requested"] == run["completed"] == len(run["items"]), run
                assert run["full"]
                assert all(item["status"] == "PASS" for item in run["items"])
                rows.append({"cache": cache, "wallMs": round(elapsed * 1000, 2), "wireBytes": size,
                             "inventory": inventory, "cargoInvocations": len(run["items"]),
                             "suite": run["suite"], "executedCases": sum(i["testsExecuted"] for i in run["items"]),
                             "items": [{key: item[key] for key in ("item", "status", "testsExecuted", "executedNames", "command", "durationMs")} for item in run["items"]]})
        return rows


def compare_reports(baseline, report):
    for key in ("fixtureSha256", "rust", "sccache", "incremental"):
        if baseline[key] != report[key]:
            raise ValueError(f"conditions differ: {key}")
    if len(baseline["samples"]) != len(report["samples"]):
        raise ValueError("sample count differs")
    for before, after in zip(baseline["samples"], report["samples"]):
        def inventory(sample):
            targets = sample.get("inventory", [i["item"] for i in sample["items"]])
            return sorted((i["package"], i["target"], i["targetKind"]) for i in targets)
        if inventory(before) != inventory(after) or before["suite"] != after["suite"]:
            raise ValueError("requested coverage differs")
        if sum(i["testsExecuted"] for i in after["items"]) != 6:
            raise ValueError("candidate did not run each fixture scope exactly once")
        names = {name for item in after["items"] for name in item["executedNames"]}
        for expected in ("unit_one", "unit_two", "integration_one", "integration_two"):
            if not any(name == expected or name.endswith("::" + expected) for name in names):
                raise ValueError(f"candidate lacks executed evidence for {expected}")
    return {"baselineBinarySha256": baseline["binarySha256"], "sameFullInventory": True,
            "before": baseline["summary"], "after": report["summary"]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--cargo", type=Path, required=True)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--baseline-report", type=Path)
    args = parser.parse_args()
    root = args.fixture.resolve()
    hashes = fixture_at(root)
    rows = [row for _ in range(3) for row in repetition(args.binary.resolve(), args.cargo.resolve(), root)]
    assert fixture_at(root) == hashes
    summary = {cache: {"medianWallMs": statistics.median(r["wallMs"] for r in rows if r["cache"] == cache),
                       "medianExecutedCases": statistics.median(r["executedCases"] for r in rows if r["cache"] == cache)} for cache in ("cold", "warm")}
    report = {"binarySha256": digest(args.binary), "fixtureSha256": hashes, "rust": "1.88.0",
              "sccache": False, "incremental": False, "samples": rows, "summary": summary}
    if args.baseline_report:
        report["comparison"] = compare_reports(json.loads(args.baseline_report.read_text()), report)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(summary))


if __name__ == "__main__":
    main()
