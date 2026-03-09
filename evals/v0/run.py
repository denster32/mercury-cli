#!/usr/bin/env python3
import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List

REPO_ROOT = Path(__file__).resolve().parents[2]
HARNESS_ROOT = Path(__file__).resolve().parent
MANIFEST_PATH = HARNESS_ROOT / "manifest.json"
DEFAULT_REPORT_DIR = HARNESS_ROOT / "reports"
RUN_ID_FORMAT = "%Y%m%dT%H%M%SZ"


@dataclass(frozen=True)
class Case:
    id: str
    title: str
    path: str
    failure_class: str
    failure_stage: str
    verifier_command: List[str]
    expected_exit_codes: List[int]
    expected_patterns: List[str]
    source_files: List[str]


@dataclass(frozen=True)
class Manifest:
    schema_version: str
    suite_id: str
    language: str
    version: int
    artifact_schema_version: str
    supported_modes: List[str]
    cases: List[Case]


def _require_keys(payload: Dict[str, Any], keys: List[str], label: str) -> None:
    missing = [key for key in keys if key not in payload]
    if missing:
        raise ValueError(f"{label} missing required keys: {', '.join(missing)}")


def load_manifest() -> Manifest:
    payload = json.loads(MANIFEST_PATH.read_text())
    _require_keys(
        payload,
        [
            "schema_version",
            "suite_id",
            "language",
            "version",
            "artifact_schema_version",
            "supported_modes",
            "cases",
        ],
        "manifest",
    )

    cases: List[Case] = []
    for case_payload in payload["cases"]:
        _require_keys(
            case_payload,
            [
                "id",
                "title",
                "path",
                "failure_class",
                "failure_stage",
                "verifier_command",
                "expected_exit_codes",
                "expected_patterns",
                "source_files",
            ],
            f"case {case_payload.get('id', '<unknown>')}",
        )
        cases.append(Case(**case_payload))

    return Manifest(cases=cases, **{key: payload[key] for key in payload if key != "cases"})


def run_case(case: Case, output_dir: Path, shared_target_dir: Path) -> Dict[str, Any]:
    case_dir = HARNESS_ROOT / case.path
    env = os.environ.copy()
    env.setdefault("CARGO_TARGET_DIR", str(shared_target_dir))

    started = time.perf_counter()
    proc = subprocess.run(
        case.verifier_command,
        cwd=case_dir,
        env=env,
        capture_output=True,
        text=True,
    )
    duration = round(time.perf_counter() - started, 3)
    combined = f"{proc.stdout}\n{proc.stderr}"
    matched_patterns = [pattern for pattern in case.expected_patterns if pattern in combined]
    baseline_ok = proc.returncode in case.expected_exit_codes and len(matched_patterns) == len(
        case.expected_patterns
    )

    case_output_dir = output_dir / "cases" / case.id
    case_output_dir.mkdir(parents=True, exist_ok=True)
    (case_output_dir / "stdout.txt").write_text(proc.stdout)
    (case_output_dir / "stderr.txt").write_text(proc.stderr)

    result = {
        "id": case.id,
        "title": case.title,
        "path": case.path,
        "failure_class": case.failure_class,
        "failure_stage": case.failure_stage,
        "verifier_command": case.verifier_command,
        "expected_exit_codes": case.expected_exit_codes,
        "source_files": case.source_files,
        "exit_code": proc.returncode,
        "duration_seconds": duration,
        "matched_patterns": matched_patterns,
        "baseline_ok": baseline_ok,
    }
    (case_output_dir / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def write_run_bundle(manifest: Manifest, results: List[Dict[str, Any]], output_root: Path) -> Path:
    output_root.mkdir(parents=True, exist_ok=True)
    run_id = datetime.now(timezone.utc).strftime(RUN_ID_FORMAT)
    run_dir = output_root / f"run-{run_id}"
    run_dir.mkdir(parents=True, exist_ok=True)

    totals = {
        "cases": len(results),
        "baseline_ok": sum(1 for result in results if result["baseline_ok"]),
        "baseline_failed": sum(1 for result in results if not result["baseline_ok"]),
    }

    manifest_snapshot = json.loads(MANIFEST_PATH.read_text())
    (run_dir / "manifest.snapshot.json").write_text(json.dumps(manifest_snapshot, indent=2) + "\n")
    environment = {
        "repo_root": str(REPO_ROOT),
        "python": sys.version,
        "cwd": os.getcwd(),
    }
    (run_dir / "environment.json").write_text(json.dumps(environment, indent=2) + "\n")

    report = {
        "schema_version": manifest.artifact_schema_version,
        "run_id": run_dir.name,
        "suite_id": manifest.suite_id,
        "generated_at": run_id,
        "mode": "baseline",
        "language": manifest.language,
        "totals": totals,
        "results": results,
    }
    (run_dir / "report.json").write_text(json.dumps(report, indent=2) + "\n")

    lines = [
        "# Mercury Eval Report v0",
        "",
        f"Run ID: {run_dir.name}",
        f"Suite: {manifest.suite_id}",
        f"Language: {manifest.language}",
        f"Cases: {totals['cases']}",
        f"Baseline OK: {totals['baseline_ok']}",
        f"Baseline Failed: {totals['baseline_failed']}",
        "",
        "| Case | Stage | Class | Exit | Baseline | Duration(s) |",
        "| --- | --- | --- | ---: | --- | ---: |",
    ]
    for result in results:
        lines.append(
            f"| {result['id']} | {result['failure_stage']} | {result['failure_class']} | "
            f"{result['exit_code']} | {'PASS' if result['baseline_ok'] else 'FAIL'} | "
            f"{result['duration_seconds']:.3f} |"
        )
    (run_dir / "summary.md").write_text("\n".join(lines) + "\n")
    return run_dir


def main() -> int:
    parser = argparse.ArgumentParser(description="Run Mercury eval harness v0")
    parser.add_argument("--case", action="append", dest="case_ids", help="Run only selected case ids")
    parser.add_argument("--list", action="store_true", help="List available case ids and exit")
    parser.add_argument(
        "--output-dir",
        default=str(DEFAULT_REPORT_DIR),
        help="Directory for run bundle output",
    )
    parser.add_argument(
        "--clean-output",
        action="store_true",
        help="Remove an existing output directory before writing a new run bundle",
    )
    args = parser.parse_args()

    manifest = load_manifest()
    if args.list:
        for case in manifest.cases:
            print(case.id)
        return 0

    selected = manifest.cases
    if args.case_ids:
        selected_ids = set(args.case_ids)
        selected = [case for case in manifest.cases if case.id in selected_ids]
        missing = sorted(selected_ids - {case.id for case in selected})
        if missing:
            print(f"unknown case ids: {', '.join(missing)}", file=sys.stderr)
            return 2

    output_root = Path(args.output_dir).resolve()
    if args.clean_output and output_root.exists():
        shutil.rmtree(output_root)

    shared_target_dir = output_root / "_cargo-target"
    results = [run_case(case, output_root, shared_target_dir) for case in selected]
    run_dir = write_run_bundle(manifest, results, output_root)

    failing = [result["id"] for result in results if not result["baseline_ok"]]
    print(f"wrote run bundle: {run_dir}")
    if failing:
        print("baseline validation failed for:")
        for case_id in failing:
            print(f"- {case_id}")
        return 1

    print(f"baseline validation passed for {len(results)} case(s)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
