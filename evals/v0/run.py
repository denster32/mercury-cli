#!/usr/bin/env python3
import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from collections import Counter
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional

REPO_ROOT = Path(__file__).resolve().parents[2]
HARNESS_ROOT = Path(__file__).resolve().parent
MANIFEST_PATH = HARNESS_ROOT / "manifest.json"
DEFAULT_REPORT_DIR = HARNESS_ROOT / "reports"
RUN_ID_FORMAT = "%Y%m%dT%H%M%SZ"
DEFAULT_TIMEOUT_SECONDS = 120
VALID_FAILURE_STAGES = {"parse", "compile", "test", "lint"}
VALID_DIFFICULTIES = {"easy", "medium", "hard"}
VALID_DEMO_TRACKS = {"docs", "extended", "none"}


def _default_provenance() -> Dict[str, str]:
    return {
        "origin": "seeded",
        "suite": "rust-v0.3-seeded",
        "variant": "seed",
        "generator": "manual-fixture",
    }


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
    provenance: Dict[str, Any] = field(default_factory=_default_provenance)
    difficulty: str = "medium"
    tags: List[str] = field(default_factory=list)
    timeout_seconds: int = DEFAULT_TIMEOUT_SECONDS
    demo_track: Optional[str] = None


@dataclass(frozen=True)
class Manifest:
    schema_version: str
    suite_id: str
    language: str
    version: int
    artifact_schema_version: str
    supported_modes: List[str]
    cases: List[Case]
    description: str = ""
    default_timeout_seconds: int = DEFAULT_TIMEOUT_SECONDS


def _require_keys(payload: Dict[str, Any], keys: List[str], label: str) -> None:
    missing = [key for key in keys if key not in payload]
    if missing:
        raise ValueError(f"{label} missing required keys: {', '.join(missing)}")


def _normalize_case(case_payload: Dict[str, Any], manifest: Dict[str, Any]) -> Case:
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
    payload = dict(case_payload)
    payload.setdefault("provenance", _default_provenance())
    payload.setdefault("difficulty", "medium")
    payload.setdefault("tags", [])
    payload.setdefault("timeout_seconds", manifest.get("default_timeout_seconds", DEFAULT_TIMEOUT_SECONDS))
    payload.setdefault("demo_track", None)
    case = Case(**payload)
    _validate_case(case)
    return case


def _require_non_empty_strings(values: List[str], label: str) -> None:
    if not values or any(not isinstance(value, str) or not value.strip() for value in values):
        raise ValueError(f"{label} must contain non-empty strings")


def _validate_case(case: Case) -> None:
    if case.failure_stage not in VALID_FAILURE_STAGES:
        raise ValueError(f"case {case.id} has unsupported failure_stage: {case.failure_stage}")
    if case.difficulty not in VALID_DIFFICULTIES:
        raise ValueError(f"case {case.id} has unsupported difficulty: {case.difficulty}")
    if case.demo_track is not None and case.demo_track not in VALID_DEMO_TRACKS:
        raise ValueError(f"case {case.id} has unsupported demo_track: {case.demo_track}")
    _require_non_empty_strings(case.verifier_command, f"case {case.id} verifier_command")
    _require_non_empty_strings(case.expected_patterns, f"case {case.id} expected_patterns")
    _require_non_empty_strings(case.source_files, f"case {case.id} source_files")
    if not case.expected_exit_codes or any(code < 0 for code in case.expected_exit_codes):
        raise ValueError(f"case {case.id} expected_exit_codes must contain non-negative integers")
    if case.timeout_seconds <= 0:
        raise ValueError(f"case {case.id} timeout_seconds must be positive")
    if not isinstance(case.provenance, dict):
        raise ValueError(f"case {case.id} provenance must be an object")
    for key in ("origin", "suite", "variant", "generator"):
        value = case.provenance.get(key)
        if not isinstance(value, str) or not value.strip():
            raise ValueError(f"case {case.id} provenance.{key} must be a non-empty string")
    if not case.tags or any(not isinstance(tag, str) or not tag.strip() for tag in case.tags):
        raise ValueError(f"case {case.id} tags must contain non-empty strings")

    case_path = Path(case.path)
    if case_path.is_absolute():
        raise ValueError(f"case {case.id} path must be relative: {case.path}")
    case_dir = HARNESS_ROOT / case_path
    if not case_dir.is_dir():
        raise ValueError(f"case {case.id} path does not exist: {case.path}")
    if not (case_dir / "Cargo.toml").is_file():
        raise ValueError(f"case {case.id} is missing Cargo.toml under {case.path}")
    for source_file in case.source_files:
        source_path = case_dir / source_file
        if not source_path.is_file():
            raise ValueError(f"case {case.id} is missing source file: {source_file}")


def _validate_manifest(payload: Dict[str, Any], cases: List[Case]) -> None:
    supported_modes = payload.get("supported_modes", [])
    if "baseline" not in supported_modes:
        raise ValueError("manifest supported_modes must include baseline")
    if payload.get("default_timeout_seconds", DEFAULT_TIMEOUT_SECONDS) <= 0:
        raise ValueError("manifest default_timeout_seconds must be positive")
    seen_ids = set()
    duplicates = []
    for case in cases:
        if case.id in seen_ids:
            duplicates.append(case.id)
        seen_ids.add(case.id)
    if duplicates:
        raise ValueError(f"manifest contains duplicate case ids: {', '.join(sorted(duplicates))}")


def _corpus_metadata(cases: List[Case]) -> Dict[str, Any]:
    fixture_paths = Counter(case.path for case in cases)
    return {
        "manifest_case_count": len(cases),
        "unique_fixture_paths": len(fixture_paths),
        "fixture_path_reuse": dict(sorted(fixture_paths.items())),
    }


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

    payload.setdefault("description", "")
    payload.setdefault("default_timeout_seconds", DEFAULT_TIMEOUT_SECONDS)
    cases = [_normalize_case(case_payload, payload) for case_payload in payload["cases"]]
    _validate_manifest(payload, cases)

    return Manifest(cases=cases, **{key: payload[key] for key in payload if key != "cases"})


def _select_cases(
    manifest: Manifest,
    case_ids: Optional[List[str]],
    stages: Optional[List[str]],
    failure_classes: Optional[List[str]],
    tags: Optional[List[str]],
    limit: Optional[int],
) -> List[Case]:
    selected = manifest.cases

    if case_ids:
        selected_ids = set(case_ids)
        selected = [case for case in selected if case.id in selected_ids]
        missing = sorted(selected_ids - {case.id for case in selected})
        if missing:
            raise ValueError(f"unknown case ids: {', '.join(missing)}")

    if stages:
        stage_set = set(stages)
        selected = [case for case in selected if case.failure_stage in stage_set]

    if failure_classes:
        class_set = set(failure_classes)
        selected = [case for case in selected if case.failure_class in class_set]

    if tags:
        tag_set = set(tags)
        selected = [case for case in selected if tag_set.intersection(case.tags)]

    if limit is not None:
        selected = selected[:limit]

    return selected


def run_case(case: Case, output_dir: Path, shared_target_dir: Path, timeout_override: Optional[int]) -> Dict[str, Any]:
    case_dir = HARNESS_ROOT / case.path
    env = os.environ.copy()
    env.setdefault("CARGO_TARGET_DIR", str(shared_target_dir))
    timeout_seconds = timeout_override or case.timeout_seconds

    started = time.perf_counter()
    timed_out = False
    try:
        proc = subprocess.run(
            case.verifier_command,
            cwd=case_dir,
            env=env,
            capture_output=True,
            text=True,
            timeout=timeout_seconds,
        )
        return_code = proc.returncode
        stdout = proc.stdout
        stderr = proc.stderr
    except subprocess.TimeoutExpired as exc:
        timed_out = True
        return_code = None
        stdout = exc.stdout or ""
        stderr = (exc.stderr or "") + f"\nprocess timed out after {timeout_seconds} seconds\n"

    duration = round(time.perf_counter() - started, 3)
    combined = f"{stdout}\n{stderr}"
    matched_patterns = [pattern for pattern in case.expected_patterns if pattern in combined]
    baseline_ok = (
        not timed_out
        and return_code in case.expected_exit_codes
        and len(matched_patterns) == len(case.expected_patterns)
    )

    case_output_dir = output_dir / "cases" / case.id
    case_output_dir.mkdir(parents=True, exist_ok=True)
    (case_output_dir / "stdout.txt").write_text(stdout)
    (case_output_dir / "stderr.txt").write_text(stderr)

    result = {
        "id": case.id,
        "title": case.title,
        "path": case.path,
        "failure_class": case.failure_class,
        "failure_stage": case.failure_stage,
        "verifier_command": case.verifier_command,
        "expected_exit_codes": case.expected_exit_codes,
        "expected_patterns": case.expected_patterns,
        "source_files": case.source_files,
        "provenance": case.provenance,
        "difficulty": case.difficulty,
        "tags": case.tags,
        "timeout_seconds": timeout_seconds,
        "demo_track": case.demo_track,
        "exit_code": return_code,
        "duration_seconds": duration,
        "matched_patterns": matched_patterns,
        "timed_out": timed_out,
        "baseline_ok": baseline_ok,
    }
    (case_output_dir / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def _selection_metadata(
    selected: List[Case],
    case_ids: Optional[List[str]],
    stages: Optional[List[str]],
    failure_classes: Optional[List[str]],
    tags: Optional[List[str]],
    limit: Optional[int],
) -> Dict[str, Any]:
    return {
        "requested_case_ids": case_ids or [],
        "requested_stages": stages or [],
        "requested_failure_classes": failure_classes or [],
        "requested_tags": tags or [],
        "requested_limit": limit,
        "selected_case_ids": [case.id for case in selected],
        "selected_count": len(selected),
    }


def write_run_bundle(
    manifest: Manifest,
    results: List[Dict[str, Any]],
    output_root: Path,
    run_id: Optional[str],
    selection: Dict[str, Any],
) -> Path:
    output_root.mkdir(parents=True, exist_ok=True)
    resolved_run_id = run_id or datetime.now(timezone.utc).strftime(RUN_ID_FORMAT)
    run_dir = output_root / f"run-{resolved_run_id}"
    run_dir.mkdir(parents=True, exist_ok=True)

    stage_counts = Counter(result["failure_stage"] for result in results)
    class_counts = Counter(result["failure_class"] for result in results)
    corpus = _corpus_metadata(manifest.cases)
    totals = {
        "cases": len(results),
        "baseline_ok": sum(1 for result in results if result["baseline_ok"]),
        "baseline_failed": sum(1 for result in results if not result["baseline_ok"]),
        "timed_out": sum(1 for result in results if result["timed_out"]),
    }

    manifest_snapshot = json.loads(MANIFEST_PATH.read_text())
    (run_dir / "manifest.snapshot.json").write_text(json.dumps(manifest_snapshot, indent=2) + "\n")
    environment = {
        "repo_root": str(REPO_ROOT),
        "harness_root": str(HARNESS_ROOT),
        "cwd": os.getcwd(),
        "python_version": sys.version,
    }
    (run_dir / "environment.json").write_text(json.dumps(environment, indent=2) + "\n")
    (run_dir / "selection.json").write_text(json.dumps(selection, indent=2) + "\n")

    report = {
        "schema_version": manifest.artifact_schema_version,
        "run_id": run_dir.name,
        "suite_id": manifest.suite_id,
        "generated_at": resolved_run_id,
        "mode": "baseline",
        "language": manifest.language,
        "manifest": {
            "schema_version": manifest.schema_version,
            "version": manifest.version,
            "default_timeout_seconds": manifest.default_timeout_seconds,
            "supported_modes": manifest.supported_modes,
        },
        "corpus": corpus,
        "selection": selection,
        "totals": totals,
        "by_stage": dict(stage_counts),
        "by_failure_class": dict(class_counts),
        "results": results,
    }
    (run_dir / "report.json").write_text(json.dumps(report, indent=2) + "\n")

    lines = [
        "# Mercury Eval Report v0",
        "",
        f"Run ID: {run_dir.name}",
        f"Suite: {manifest.suite_id}",
        f"Language: {manifest.language}",
        f"Manifest Schema: {manifest.schema_version}",
        f"Manifest Version: {manifest.version}",
        f"Artifact Schema: {manifest.artifact_schema_version}",
        f"Corpus Cases: {corpus['manifest_case_count']}",
        f"Unique Fixture Paths: {corpus['unique_fixture_paths']}",
        f"Cases: {totals['cases']}",
        f"Baseline OK: {totals['baseline_ok']}",
        f"Baseline Failed: {totals['baseline_failed']}",
        f"Timed Out: {totals['timed_out']}",
        "",
        "## Selection",
        "",
        f"- Requested case ids: {', '.join(selection['requested_case_ids']) or '(all)'}",
        f"- Requested stages: {', '.join(selection['requested_stages']) or '(all)'}",
        f"- Requested failure classes: {', '.join(selection['requested_failure_classes']) or '(all)'}",
        f"- Requested tags: {', '.join(selection['requested_tags']) or '(all)'}",
        f"- Requested limit: {selection['requested_limit'] if selection['requested_limit'] is not None else '(none)'}",
        "",
        "## Stage Summary",
        "",
    ]
    for stage, count in sorted(stage_counts.items()):
        lines.append(f"- {stage}: {count}")
    lines.extend(["", "## Failure Class Summary", ""])
    for failure_class, count in sorted(class_counts.items()):
        lines.append(f"- {failure_class}: {count}")
    lines.extend(
        [
            "",
            "## Case Results",
            "",
            "| Case | Stage | Class | Exit | Baseline | Duration(s) | Tags |",
            "| --- | --- | --- | ---: | --- | ---: | --- |",
        ]
    )
    for result in results:
        lines.append(
            f"| {result['id']} | {result['failure_stage']} | {result['failure_class']} | "
            f"{result['exit_code'] if result['exit_code'] is not None else 'timeout'} | "
            f"{'PASS' if result['baseline_ok'] else 'FAIL'} | "
            f"{result['duration_seconds']:.3f} | "
            f"{', '.join(result['tags']) or '-'} |"
        )
    (run_dir / "summary.md").write_text("\n".join(lines) + "\n")
    return run_dir


def _list_cases(cases: List[Case], json_output: bool) -> int:
    if json_output:
        payload = [asdict(case) for case in cases]
        print(json.dumps(payload, indent=2))
        return 0

    for case in cases:
        print(case.id)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Run Mercury eval harness v0")
    parser.add_argument("--case", action="append", dest="case_ids", help="Run only selected case ids")
    parser.add_argument("--stage", action="append", dest="stages", help="Filter by failure stage")
    parser.add_argument(
        "--failure-class",
        action="append",
        dest="failure_classes",
        help="Filter by failure class",
    )
    parser.add_argument("--tag", action="append", dest="tags", help="Filter by case tag")
    parser.add_argument("--limit", type=int, help="Limit the number of selected cases")
    parser.add_argument("--list", action="store_true", help="List available case ids and exit")
    parser.add_argument("--list-json", action="store_true", help="List selected cases as JSON and exit")
    parser.add_argument("--run-id", help="Override the generated run id for a reproducible output path")
    parser.add_argument(
        "--timeout-seconds",
        type=int,
        help="Override the timeout for each selected case",
    )
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
    try:
        selected = _select_cases(
            manifest,
            case_ids=args.case_ids,
            stages=args.stages,
            failure_classes=args.failure_classes,
            tags=args.tags,
            limit=args.limit,
        )
    except ValueError as exc:
        print(str(exc), file=sys.stderr)
        return 2

    if args.list or args.list_json:
        return _list_cases(selected, json_output=args.list_json)

    if not selected:
        print("no cases selected", file=sys.stderr)
        return 2

    output_root = Path(args.output_dir).resolve()
    if args.clean_output and output_root.exists():
        shutil.rmtree(output_root)

    resolved_run_id = args.run_id or datetime.now(timezone.utc).strftime(RUN_ID_FORMAT)
    run_output_dir = output_root / f"run-{resolved_run_id}"
    run_output_dir.mkdir(parents=True, exist_ok=True)
    shared_target_dir = output_root / "_cargo-target"
    results = [run_case(case, run_output_dir, shared_target_dir, args.timeout_seconds) for case in selected]
    selection = _selection_metadata(
        selected,
        case_ids=args.case_ids,
        stages=args.stages,
        failure_classes=args.failure_classes,
        tags=args.tags,
        limit=args.limit,
    )
    run_dir = write_run_bundle(
        manifest,
        results,
        output_root,
        run_id=resolved_run_id,
        selection=selection,
    )

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
