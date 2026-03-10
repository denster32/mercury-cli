#!/usr/bin/env python3
import argparse
import json
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
DEFAULT_TIMEOUT_SECONDS = 60
VALID_FAILURE_STAGES = {"parse", "compile", "test", "lint"}
VALID_DIFFICULTIES = {"easy", "medium", "hard"}
VALID_DEMO_TRACKS = {"docs", "extended", "none"}


def _default_provenance() -> Dict[str, str]:
    return {
        "origin": "seeded",
        "suite": "typescript-v1.0-seeded",
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


def _require_non_empty_strings(values: List[str], label: str) -> None:
    if not values or any(not isinstance(value, str) or not value.strip() for value in values):
        raise ValueError(f"{label} must contain non-empty strings")


def _expected_variant(case_id: str) -> str:
    prefix, marker, suffix = case_id.rpartition("_v")
    if marker and prefix and suffix.isdigit():
        return f"v{suffix}"
    return "seed"


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
    _validate_case(case, manifest)
    return case


def _validate_case(case: Case, manifest: Dict[str, Any]) -> None:
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
    if not (case_dir / "package.json").is_file():
        raise ValueError(f"case {case.id} is missing package.json under {case.path}")
    for source_file in case.source_files:
        source_path = case_dir / source_file
        if not source_path.is_file():
            raise ValueError(f"case {case.id} is missing source file: {source_file}")

    expected_variant = _expected_variant(case.id)
    if case.provenance["suite"] != manifest["suite_id"]:
        raise ValueError(
            f"case {case.id} provenance.suite must match manifest suite_id: {manifest['suite_id']}"
        )
    if case.provenance["variant"] != expected_variant:
        raise ValueError(
            f"case {case.id} provenance.variant must match explicit id suffix contract: {expected_variant}"
        )

    tag_set = set(case.tags)
    required_tags = {
        f"language:{manifest['language']}",
        f"stage:{case.failure_stage}",
        f"failure:{case.failure_class}",
        f"variant:{expected_variant}",
    }
    missing_tags = sorted(required_tags - tag_set)
    if missing_tags:
        raise ValueError(f"case {case.id} is missing required tags: {', '.join(missing_tags)}")

    if expected_variant == "seed":
        if "kind:seed" not in tag_set or "kind:variant" in tag_set:
            raise ValueError(f"case {case.id} seed ids must use kind:seed and omit kind:variant")
    elif "kind:variant" not in tag_set or "kind:seed" in tag_set:
        raise ValueError(f"case {case.id} variant ids must use kind:variant and omit kind:seed")


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


def run_case(case: Case, output_dir: Path, timeout_override: Optional[int]) -> Dict[str, Any]:
    case_dir = HARNESS_ROOT / case.path
    timeout_seconds = timeout_override or case.timeout_seconds

    started = time.perf_counter()
    timed_out = False
    try:
        proc = subprocess.run(
            case.verifier_command,
            cwd=case_dir,
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
    fixture_paths = Counter(case.path for case in selected)
    return {
        "requested_case_ids": case_ids or [],
        "requested_stages": stages or [],
        "requested_failure_classes": failure_classes or [],
        "requested_tags": tags or [],
        "limit": limit,
        "selected_case_ids": [case.id for case in selected],
        "selected_count": len(selected),
        "selected_unique_fixture_paths": len(fixture_paths),
        "selected_fixture_path_reuse": dict(sorted(fixture_paths.items())),
    }


def _summary_markdown(report: Dict[str, Any]) -> str:
    return "\n".join(
        [
            f"# Mercury Eval Summary ({report['suite_id']})",
            "",
            f"- Run ID: `{report['run_id']}`",
            f"- Mode: `{report['mode']}`",
            f"- Manifest cases: `{report['corpus']['manifest_case_count']}`",
            f"- Selected cases: `{report['totals']['cases']}`",
            f"- Baseline expected-red passed: `{report['totals']['baseline_ok']}`",
            f"- Baseline expected-red failed: `{report['totals']['baseline_failed']}`",
            f"- Duration seconds: `{report['timing']['total_duration_seconds']}`",
            "",
            "## Stage Breakdown",
            "",
        ]
        + [
            f"- `{stage}`: cases=`{counts['cases']}`, baseline_ok=`{counts['baseline_ok']}`, baseline_failed=`{counts['baseline_failed']}`"
            for stage, counts in sorted(report["by_failure_stage"].items())
        ]
    ) + "\n"


def _write_json(path: Path, payload: Dict[str, Any]) -> None:
    path.write_text(json.dumps(payload, indent=2) + "\n")


def run_selected_cases(
    manifest: Manifest,
    selected: List[Case],
    output_dir: Path,
    run_id: str,
    timeout_override: Optional[int],
    selection_metadata: Dict[str, Any],
) -> Dict[str, Any]:
    output_dir.mkdir(parents=True, exist_ok=True)

    run_started = datetime.now(timezone.utc)
    started = time.perf_counter()
    case_results = [run_case(case, output_dir, timeout_override) for case in selected]
    duration_seconds = round(time.perf_counter() - started, 3)

    totals = {
        "cases": len(case_results),
        "baseline_ok": sum(1 for result in case_results if result["baseline_ok"]),
    }
    totals["baseline_failed"] = totals["cases"] - totals["baseline_ok"]

    by_failure_stage: Dict[str, Dict[str, int]] = {}
    by_failure_class: Dict[str, Dict[str, int]] = {}
    for result in case_results:
        stage_counts = by_failure_stage.setdefault(
            result["failure_stage"],
            {"cases": 0, "baseline_ok": 0, "baseline_failed": 0},
        )
        stage_counts["cases"] += 1
        stage_counts["baseline_ok"] += int(result["baseline_ok"])
        stage_counts["baseline_failed"] += int(not result["baseline_ok"])

        class_counts = by_failure_class.setdefault(
            result["failure_class"],
            {"cases": 0, "baseline_ok": 0, "baseline_failed": 0},
        )
        class_counts["cases"] += 1
        class_counts["baseline_ok"] += int(result["baseline_ok"])
        class_counts["baseline_failed"] += int(not result["baseline_ok"])

    report = {
        "schema_version": manifest.artifact_schema_version,
        "suite_id": manifest.suite_id,
        "language": manifest.language,
        "mode": "baseline",
        "run_id": run_id,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "manifest": {
            "schema_version": manifest.schema_version,
            "version": manifest.version,
            "supported_modes": manifest.supported_modes,
            "description": manifest.description,
            "default_timeout_seconds": manifest.default_timeout_seconds,
        },
        "corpus": _corpus_metadata(manifest.cases),
        "selection": selection_metadata,
        "timing": {
            "run_started_at": run_started.isoformat(),
            "total_duration_seconds": duration_seconds,
        },
        "totals": totals,
        "by_failure_stage": dict(sorted(by_failure_stage.items())),
        "by_failure_class": dict(sorted(by_failure_class.items())),
        "results": case_results,
    }

    _write_json(output_dir / "manifest.snapshot.json", asdict(manifest))
    _write_json(
        output_dir / "environment.json",
        {
            "python": sys.version,
            "platform": sys.platform,
            "cwd": str(REPO_ROOT),
            "harness_root": str(HARNESS_ROOT),
        },
    )
    _write_json(output_dir / "selection.json", selection_metadata)
    _write_json(output_dir / "report.json", report)
    (output_dir / "summary.md").write_text(_summary_markdown(report))

    return report


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run Mercury TypeScript eval harness")
    parser.add_argument("--case", action="append", dest="cases", help="Specific case id to run")
    parser.add_argument("--stage", action="append", choices=sorted(VALID_FAILURE_STAGES), help="Filter by failure stage")
    parser.add_argument("--failure-class", action="append", dest="failure_classes", help="Filter by failure class")
    parser.add_argument("--tag", action="append", dest="tags", help="Filter by case tag")
    parser.add_argument("--limit", type=int, help="Maximum number of selected cases")
    parser.add_argument("--list", action="store_true", help="List selected case ids and exit")
    parser.add_argument("--list-json", action="store_true", help="Print selected cases as JSON and exit")
    parser.add_argument("--run-id", help="Override generated run id")
    parser.add_argument("--output-dir", type=Path, default=DEFAULT_REPORT_DIR, help="Report output directory")
    parser.add_argument("--clean-output", action="store_true", help="Delete output directory before run")
    parser.add_argument("--timeout", type=int, help="Override timeout seconds for every selected case")
    return parser.parse_args()


def main() -> int:
    args = parse_args()

    if args.limit is not None and args.limit <= 0:
        raise ValueError("--limit must be positive")
    if args.timeout is not None and args.timeout <= 0:
        raise ValueError("--timeout must be positive")

    manifest = load_manifest()
    selected = _select_cases(
        manifest,
        case_ids=args.cases,
        stages=args.stage,
        failure_classes=args.failure_classes,
        tags=args.tags,
        limit=args.limit,
    )

    if args.list or args.list_json:
        payload = [
            {
                "id": case.id,
                "title": case.title,
                "failure_stage": case.failure_stage,
                "failure_class": case.failure_class,
                "path": case.path,
                "tags": case.tags,
            }
            for case in selected
        ]
        if args.list_json:
            print(json.dumps(payload, indent=2))
        else:
            for case in payload:
                print(f"{case['id']}\t{case['failure_stage']}\t{case['failure_class']}\t{case['path']}")
        return 0

    run_id = args.run_id or datetime.now(timezone.utc).strftime(RUN_ID_FORMAT)
    run_dir = args.output_dir / f"run-{run_id}"

    if args.clean_output and args.output_dir.exists():
        shutil.rmtree(args.output_dir)
    run_dir.mkdir(parents=True, exist_ok=True)

    selection_metadata = _selection_metadata(
        selected,
        case_ids=args.cases,
        stages=args.stage,
        failure_classes=args.failure_classes,
        tags=args.tags,
        limit=args.limit,
    )

    report = run_selected_cases(
        manifest=manifest,
        selected=selected,
        output_dir=run_dir,
        run_id=run_id,
        timeout_override=args.timeout,
        selection_metadata=selection_metadata,
    )

    if report["totals"]["baseline_failed"] > 0:
        print(
            f"baseline mismatch: {report['totals']['baseline_failed']} of {report['totals']['cases']} cases did not match expected red-state",
            file=sys.stderr,
        )
        return 1

    print(f"wrote report to {run_dir}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise
