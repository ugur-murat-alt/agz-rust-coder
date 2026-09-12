"""Compare the cost of running the same single test with broad/explicit scope.

Use the same --fixture for baseline and candidate. Only the candidate uses
--selected; all runs execute exactly one test. Cold caches and warm repetitions
are reported separately. This does not compare full-workspace correctness.
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


def prepare_fixture(root):
    sources = {"Cargo.toml": '[workspace]\nmembers=["packages/*"]\nresolver="2"\n'}
    lock = "version=4\n"
    for i in range(8):
        prefix = f"packages/scope-{i}"
        sources[f"{prefix}/Cargo.toml"] = (
            f'[package]\nname="scope-{i}"\nversion="0.1.0"\nedition="2024"\n'
        )
        sources[f"{prefix}/src/lib.rs"] = "\n".join(
            f"pub fn value_{n}(v: u64) -> u64 {{ v.wrapping_mul({n + 1}) }}"
            for n in range(500)
        ) + "\n"
        sources[f"{prefix}/src/main.rs"] = f"fn main() {{ assert_eq!(scope_{i}::value_0(7), 7); }}\n"
        name = "scope_focus" if i == 0 else f"unrelated_{i}"
        sources[f"{prefix}/tests/focused.rs"] = (
            f"#[test]\nfn {name}() {{ assert_eq!(scope_{i}::value_499(7), 3500); }}\n"
        )
        lock += f'[[package]]\nname="scope-{i}"\nversion="0.1.0"\n'
    sources["Cargo.lock"] = lock
    for name, content in sources.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        if path.exists() and path.read_text() != content:
            raise ValueError(f"fixture changed: {name}")
        if not path.exists():
            path.write_text(content)
    return {name: digest(root / name) for name in sorted(sources)}


def repetition(binary, cargo, root, selected):
    env = {k: v for k, v in os.environ.items() if not k.startswith("AGZ_RUST_MCP_")}
    env.pop("CARGO_TARGET_DIR", None)
    env.update(RUSTUP_TOOLCHAIN="1.88.0", CARGO_NET_OFFLINE="true",
               CARGO_INCREMENTAL="0", RUSTC_WRAPPER="", RUST_BALANCED_SCCACHE="off")
    options = {"testFilter": "scope_focus"}
    if selected:
        options.update(packages=["scope-0"], cargoTarget={"kind": "test", "name": "focused"})
    with tempfile.TemporaryDirectory(prefix="agz-scope-state-") as directory:
        state = Path(directory)
        command = [str(binary), "--allow-root", str(root), "--cargo-path", str(cargo),
                   "--gate-cache", "isolated", "--gate-cache-dir", str(state / "gate"),
                   "--gate-lease-dir", str(state / "leases"), "--telemetry-enabled", "false"]
        rows = []
        with McpSession(command, cwd=root, env=env, stderr=state / "stderr.log") as client:
            for cache in ("cold", "warm"):
                started = time.monotonic()
                result, size = client.call("check", {"dir": str(root), "target": "test",
                                                     "detail": "compact", "options": options})
                elapsed = time.monotonic() - started
                data = result["structuredContent"]
                if data["status"] != "FAST_PASS":
                    raise RuntimeError(str(data)[:6000])
                step = data["data"]["steps"][0]
                assert step["evidence"]["testsExecuted"] == 1, step["evidence"]
                assert step["exitCode"] == 0, step
                rows.append({"cache": cache, "wallMs": round(elapsed * 1000, 2),
                             "cargoMs": step["durationMs"], "wireBytes": size,
                             "command": step["command"], "build": step["build"],
                             "evidence": step["evidence"], "scope": data["data"]["scope"]})
        return rows


def compare_reports(baseline, report):
    for key in ("fixtureSha256", "rust", "sccache", "incremental"):
        if baseline[key] != report[key]:
            raise ValueError(f"comparison conditions changed: {key}")
    if baseline["selected"] or not report["selected"]:
        raise ValueError("comparison requires broad baseline and selected candidate")
    for cache in ("cold", "warm"):
        before = [s for s in baseline["samples"] if s["cache"] == cache]
        after = [s for s in report["samples"] if s["cache"] == cache]
        if len(before) != 3 or len(after) != 3:
            raise ValueError("comparison requires three samples per cache state")
        if any(s["evidence"]["testsExecuted"] != 1 or not s["evidence"]["buildSuccess"]
               for s in before + after):
            raise ValueError("both arms must build successfully and execute the same one test")
    return {
        "baselineBinarySha256": baseline["binarySha256"],
        "scope": "same selected test; excluded workspace targets are not validated",
        "cacheStates": {cache: {
            "baseline": baseline["summary"][cache], "candidate": report["summary"][cache],
            "cargoReductionPercent": round(100 * (1 - report["summary"][cache]["medianCargoMs"]
                                                 / baseline["summary"][cache]["medianCargoMs"]), 2),
        } for cache in ("cold", "warm")},
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--cargo", type=Path, required=True)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--selected", action="store_true")
    parser.add_argument("--baseline-report", type=Path)
    args = parser.parse_args()
    root = args.fixture.resolve()
    hashes = prepare_fixture(root)
    rows = [row for _ in range(3) for row in repetition(args.binary.resolve(), args.cargo.resolve(), root, args.selected)]
    assert prepare_fixture(root) == hashes
    summary = {cache: {
        "medianWallMs": statistics.median(row["wallMs"] for row in rows if row["cache"] == cache),
        "medianCargoMs": statistics.median(row["cargoMs"] for row in rows if row["cache"] == cache),
    } for cache in ("cold", "warm")}
    report = {"binarySha256": digest(args.binary), "fixtureSha256": hashes,
              "selected": args.selected, "rust": "1.88.0", "sccache": False,
              "incremental": False, "samples": rows, "summary": summary,
              "scope": "same one executed test, not full-workspace correctness"}
    if args.baseline_report:
        report["comparison"] = compare_reports(json.loads(args.baseline_report.read_text()), report)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(summary))


if __name__ == "__main__":
    main()
