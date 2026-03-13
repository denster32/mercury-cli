#!/usr/bin/env python3
import argparse
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import traceback
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable, Optional, Union

REPO_ROOT = Path(__file__).resolve().parents[2]
HARNESS_ROOT = Path(__file__).resolve().parent
DEFAULT_MANIFEST_PATH = REPO_ROOT / "evals" / "v0" / "tier1-manifest.json"
DEFAULT_OUTPUT_DIR = HARNESS_ROOT / "reports"
RUN_ID_FORMAT = "%Y%m%dT%H%M%SZ"
BENCHMARK_SCHEMA_VERSION = "mercury-repair-benchmark-v1"
BENCHMARK_RESULT_SCHEMA_VERSION = "mercury-repair-benchmark-case-v1"
DEFAULT_TIMEOUT_SECONDS = 300
DEFAULT_MAX_COST_USD = 0.5
DEFAULT_QUALITY_AGENT_COUNT = 4
DEFAULT_AGENT_SWEEP = [1, 2, 4, 8]
DEFAULT_REPRESENTATIVE_COUNT = 10
DIFFICULTY_ORDER = ["easy", "medium", "hard", "unknown"]
TIER_ORDER = ["tier0", "tier1", "tier2", "unknown"]
VERIFIER_CLASS_ORDER = ["cargo_test", "cargo_check", "cargo_clippy", "unknown"]
CANDIDATE_LINEAGE_ORDER = [
    "apply_edit",
    "grounded_next_edit",
    "critique_retry",
    "exploratory_next_edit",
    "mixed",
    "unknown",
]
FAILURE_ATTRIBUTION_ORDER = [
    "baseline_not_reproduced",
    "missing_benchmark_run",
    "fix_failed",
    "rejected_allowlist",
    "patch_generation_failed",
    "candidate_failed_safety",
    "candidate_failed_verifier",
    "final_bundle_verification_failed",
    "no_patch_emitted",
    "accepted_patch_failed_independent_rerun",
    "mercury_verified_but_independent_rerun_failed",
    "runner_error",
]
EXECUTION_DIAGNOSTIC_FIELDS = [
    "generation_failures",
    "safety_failures",
    "candidate_verification_failures",
    "final_bundle_failures",
]
CANDIDATE_LINEAGE_FIELDS = {
    "apply_edit": ("apply_edit_attempts", "apply_edit_accepted_steps"),
    "grounded_next_edit": (
        "grounded_next_edit_attempts",
        "grounded_next_edit_accepted_steps",
    ),
    "critique_retry": ("critique_retry_attempts", "critique_retry_accepted_steps"),
    "exploratory_next_edit": (
        "exploratory_next_edit_attempts",
        "exploratory_next_edit_accepted_steps",
    ),
}
CANDIDATE_ATTEMPT_FIELDS = [fields[0] for fields in CANDIDATE_LINEAGE_FIELDS.values()]
CANDIDATE_ACCEPTED_FIELDS = [fields[1] for fields in CANDIDATE_LINEAGE_FIELDS.values()]
TIER_PATTERN = re.compile(r"tier[\s:_-]*([012])", re.IGNORECASE)


def now_utc() -> datetime:
    return datetime.now(timezone.utc)


def isoformat_utc(value: datetime) -> str:
    return value.isoformat().replace("+00:00", "Z")


def normalize_text(content: Union[str, bytes, None, object]) -> str:
    if content is None:
        return ""
    if isinstance(content, bytes):
        return content.decode("utf-8", errors="replace")
    if isinstance(content, str):
        return content
    return str(content)


def write_text(path: Path, content: Union[str, bytes, None, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(normalize_text(content), encoding="utf-8")


def write_json(path: Path, payload: Any) -> None:
    write_text(path, json.dumps(payload, indent=2, sort_keys=True) + "\n")


def read_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def shell_join(parts: list[str]) -> str:
    return subprocess.list2cmdline(parts) if os.name == "nt" else subprocess.list2cmdline(parts).replace('"', "'")


def command_text(parts: list[str]) -> str:
    if hasattr(shutil, "which") and os.name != "nt":
        try:
            import shlex

            return shlex.join(parts)
        except Exception:
            return " ".join(parts)
    return " ".join(parts)


def require_manifest_keys(payload: dict[str, Any], keys: Iterable[str], label: str) -> None:
    missing = [key for key in keys if key not in payload]
    if missing:
        raise ValueError(f"{label} missing required keys: {', '.join(missing)}")


def load_manifest(path: Path) -> dict[str, Any]:
    payload = read_json(path)
    require_manifest_keys(
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
    if payload["schema_version"] != "mercury-evals-v0":
        raise ValueError(
            f"repair benchmark runner expects mercury-evals-v0 manifests, got {payload['schema_version']}"
        )
    if not isinstance(payload["cases"], list) or not payload["cases"]:
        raise ValueError("manifest cases must be a non-empty array")
    return payload


def select_cases(
    manifest: dict[str, Any],
    case_ids: Optional[list[str]],
    stages: Optional[list[str]],
    failure_classes: Optional[list[str]],
    difficulties: Optional[list[str]],
    tags: Optional[list[str]],
    limit: Optional[int],
) -> list[dict[str, Any]]:
    selected = list(manifest["cases"])

    if case_ids:
        expected = set(case_ids)
        selected = [case for case in selected if case["id"] in expected]
        missing = sorted(expected - {case["id"] for case in selected})
        if missing:
            raise ValueError(f"unknown case ids: {', '.join(missing)}")

    if stages:
        selected = [case for case in selected if case["failure_stage"] in set(stages)]

    if failure_classes:
        selected = [case for case in selected if case["failure_class"] in set(failure_classes)]

    if difficulties:
        requested_difficulties = {normalize_difficulty_label(value) for value in difficulties}
        selected = [
            case
            for case in selected
            if normalize_difficulty_label(case.get("difficulty")) in requested_difficulties
        ]

    if tags:
        tag_set = set(tags)
        selected = [
            case
            for case in selected
            if tag_set.intersection(set(case.get("tags", [])))
        ]

    if limit is not None:
        selected = selected[:limit]

    if not selected:
        raise ValueError("selection produced zero benchmark cases")

    return selected


def representative_cases(
    cases: list[dict[str, Any]], representative_count: int
) -> list[dict[str, Any]]:
    selected: list[dict[str, Any]] = []
    seen_paths: set[str] = set()

    for case in cases:
        case_path = case["path"]
        if case_path in seen_paths:
            continue
        selected.append(case)
        seen_paths.add(case_path)
        if len(selected) >= representative_count:
            return selected

    if len(selected) < representative_count:
        for case in cases:
            if case in selected:
                continue
            selected.append(case)
            if len(selected) >= representative_count:
                return selected

    return selected


def find_api_key_env() -> str:
    if os.environ.get("INCEPTION_API_KEY"):
        return "INCEPTION_API_KEY"
    if os.environ.get("MERCURY_API_KEY"):
        return "MERCURY_API_KEY"
    raise RuntimeError(
        "repair benchmark requires INCEPTION_API_KEY or MERCURY_API_KEY for live Mercury runs"
    )


def run_command(
    argv: list[str],
    cwd: Path,
    timeout_seconds: int,
    env: Optional[dict[str, str]] = None,
) -> dict[str, Any]:
    started = now_utc()
    completed: Optional[subprocess.CompletedProcess[str]] = None
    timed_out = False
    error: Optional[str] = None

    try:
        completed = subprocess.run(
            argv,
            cwd=str(cwd),
            env=env,
            text=True,
            capture_output=True,
            timeout=timeout_seconds,
        )
    except subprocess.TimeoutExpired as exc:
        timed_out = True
        stdout = exc.stdout or ""
        stderr = exc.stderr or ""
        exit_code = None
    except OSError as exc:
        stdout = ""
        stderr = ""
        exit_code = None
        error = str(exc)
    else:
        stdout = completed.stdout
        stderr = completed.stderr
        exit_code = completed.returncode

    finished = now_utc()
    payload = {
        "command": argv,
        "command_text": command_text(argv),
        "cwd": str(cwd),
        "started_at": isoformat_utc(started),
        "finished_at": isoformat_utc(finished),
        "duration_ms": int((finished - started).total_seconds() * 1000),
        "timeout_seconds": timeout_seconds,
        "timed_out": timed_out,
        "exit_code": exit_code,
        "stdout": stdout,
        "stderr": stderr,
        "success": (exit_code == 0) and not timed_out and error is None,
        "error": error,
    }
    return payload


def latest_mercury_run(workspace: Path) -> Optional[Path]:
    runs_root = workspace / ".mercury" / "runs"
    if not runs_root.exists():
        return None
    run_dirs = [path for path in runs_root.iterdir() if path.is_dir()]
    if not run_dirs:
        return None
    return max(run_dirs, key=lambda path: path.stat().st_mtime_ns)


def upsert_section_value(text: str, section: str, key: str, rendered_value: str) -> str:
    header = f"[{section}]"
    replacement = f"{key} = {rendered_value}"
    lines = text.splitlines()
    out: list[str] = []
    in_section = False
    section_found = False
    key_written = False

    for index, line in enumerate(lines):
        stripped = line.strip()
        is_header = stripped.startswith("[") and stripped.endswith("]")
        if is_header:
            if in_section and not key_written:
                out.append(replacement)
                key_written = True
            in_section = stripped == header
            section_found = section_found or in_section
            out.append(line)
            continue
        if in_section and stripped.startswith(f"{key} ="):
            if not key_written:
                out.append(replacement)
                key_written = True
            continue
        out.append(line)
        if index == len(lines) - 1 and in_section and not key_written:
            out.append(replacement)
            key_written = True

    if section_found:
        return "\n".join(out).rstrip() + "\n"

    prefix = text.rstrip()
    if prefix:
        prefix += "\n\n"
    return f"{prefix}[{section}]\n{replacement}\n"


def configure_mercury(workspace: Path, case: dict[str, Any], api_key_env: str) -> Path:
    config_path = workspace / ".mercury" / "config.toml"
    text = config_path.read_text(encoding="utf-8") if config_path.exists() else ""
    verifier_command = json.dumps(command_text(case["verifier_command"]))
    text = upsert_section_value(text, "api", "api_key_env", json.dumps(api_key_env))
    text = upsert_section_value(text, "verification", "parse_before_write", "true")
    text = upsert_section_value(
        text,
        "verification",
        "mercury2_critique_on_failure",
        "true",
    )

    if case["failure_stage"] == "lint":
        text = upsert_section_value(text, "verification", "test_after_write", "false")
        text = upsert_section_value(text, "verification", "lint_after_write", "true")
        text = upsert_section_value(text, "verification", "lint_command", verifier_command)
    else:
        text = upsert_section_value(text, "verification", "test_after_write", "true")
        text = upsert_section_value(text, "verification", "test_command", verifier_command)
        text = upsert_section_value(text, "verification", "lint_after_write", "false")

    write_text(config_path, text)
    return config_path


def write_command_logs(root: Path, stem: str, result: dict[str, Any]) -> None:
    write_text(root / f"{stem}.stdout.log", result.get("stdout", ""))
    write_text(root / f"{stem}.stderr.log", result.get("stderr", ""))
    serialized = dict(result)
    serialized.pop("stdout", None)
    serialized.pop("stderr", None)
    write_json(root / f"{stem}.json", serialized)


def candidate_workspace_from_benchmark_run(benchmark_run: dict[str, Any]) -> Optional[Path]:
    sandbox_run_root = benchmark_run.get("sandbox_run_root")
    if not sandbox_run_root:
        return None
    candidate = Path(sandbox_run_root) / "final-bundle"
    if candidate.exists():
        return candidate
    return None


def sanitized_benchmark_run(
    benchmark_run: dict[str, Any], keep_workspaces: bool
) -> dict[str, Any]:
    copied = dict(benchmark_run)
    if not keep_workspaces:
        copied.pop("sandbox_run_root", None)
    return copied


def classify_case_outcome(
    benchmark_run: dict[str, Any],
    accepted_patch: bool,
    mercury_verified: bool,
    rerun_success: bool,
) -> str:
    raw_outcome = benchmark_run.get("outcome")
    if mercury_verified and not rerun_success:
        return "false_green"
    if accepted_patch and rerun_success:
        return "verified_repair"
    if accepted_patch:
        return "accepted_patch_unverified"
    if isinstance(raw_outcome, str) and raw_outcome:
        return raw_outcome
    return "missing_benchmark_run"


def mean_or_none(values: list[float]) -> Optional[float]:
    if not values:
        return None
    return sum(values) / len(values)


def median_or_none(values: list[Union[float, int]]) -> Optional[float]:
    if not values:
        return None
    return float(statistics.median(values))


def numeric_count(value: Any) -> int:
    return int(value) if isinstance(value, (int, float)) else 0


def normalize_difficulty_label(value: Any) -> str:
    if isinstance(value, str) and value in {"easy", "medium", "hard"}:
        return value
    return "unknown"


def normalize_tier_label(value: Any) -> str:
    if isinstance(value, (int, float)) and int(value) in {0, 1, 2}:
        return f"tier{int(value)}"
    if isinstance(value, str):
        lowered = value.strip().lower()
        compact = lowered.replace(" ", "").replace("-", "").replace("_", "")
        if compact in {"tier0", "0"}:
            return "tier0"
        if compact in {"tier1", "1"}:
            return "tier1"
        if compact in {"tier2", "2"}:
            return "tier2"
        match = TIER_PATTERN.search(lowered)
        if match:
            return f"tier{match.group(1)}"
    return "unknown"


def derive_tier(
    case: dict[str, Any],
    suite_id: Optional[str] = None,
    manifest_path: Optional[Union[str, Path]] = None,
) -> str:
    explicit = normalize_tier_label(case.get("tier"))
    if explicit != "unknown":
        return explicit

    for tag in case.get("tags", []):
        normalized = normalize_tier_label(tag)
        if normalized != "unknown":
            return normalized

    for candidate in (
        suite_id,
        str(manifest_path) if manifest_path is not None else None,
        case.get("id"),
        case.get("path"),
        case.get("title"),
    ):
        normalized = normalize_tier_label(candidate)
        if normalized != "unknown":
            return normalized

    return "unknown"


def classify_verifier_command(argv: Any) -> str:
    parts: list[str] = []
    if isinstance(argv, list):
        parts = [part.lower() for part in argv if isinstance(part, str)]
    elif isinstance(argv, str):
        parts = argv.lower().split()

    for index, part in enumerate(parts):
        if part != "cargo" or index + 1 >= len(parts):
            continue
        subcommand = parts[index + 1]
        if subcommand == "test":
            return "cargo_test"
        if subcommand == "check":
            return "cargo_check"
        if subcommand == "clippy":
            return "cargo_clippy"
    return "unknown"


def normalized_execution_diagnostics(source: dict[str, Any]) -> dict[str, int]:
    return {field: numeric_count(source.get(field)) for field in EXECUTION_DIAGNOSTIC_FIELDS}


def normalized_candidate_lineage_counts(source: dict[str, Any]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for attempt_field in CANDIDATE_ATTEMPT_FIELDS:
        counts[attempt_field] = numeric_count(source.get(attempt_field))
    for accepted_field in CANDIDATE_ACCEPTED_FIELDS:
        counts[accepted_field] = numeric_count(source.get(accepted_field))
    return counts


def candidate_lineage_label(source: dict[str, Any]) -> str:
    lineage_counts = normalized_candidate_lineage_counts(source)
    accepted_sources = [
        label
        for label, (_, accepted_field) in CANDIDATE_LINEAGE_FIELDS.items()
        if lineage_counts[accepted_field] > 0
    ]
    if len(accepted_sources) == 1:
        return accepted_sources[0]
    if len(accepted_sources) > 1:
        return "mixed"

    attempted_sources = [
        label
        for label, (attempt_field, _) in CANDIDATE_LINEAGE_FIELDS.items()
        if lineage_counts[attempt_field] > 0
    ]
    if len(attempted_sources) == 1:
        return attempted_sources[0]
    if len(attempted_sources) > 1:
        return "mixed"
    return "unknown"


def normalized_case_result(result: dict[str, Any]) -> dict[str, Any]:
    normalized = dict(result)
    normalized["accepted_patch_bytes"] = numeric_count(normalized.get("accepted_patch_bytes"))
    normalized["tier"] = normalize_tier_label(normalized.get("tier"))
    verifier_class = normalized.get("verifier_class")
    normalized["verifier_class"] = (
        verifier_class
        if isinstance(verifier_class, str) and verifier_class in VERIFIER_CLASS_ORDER
        else "unknown"
    )
    normalized.update(normalized_execution_diagnostics(normalized))
    normalized.update(normalized_candidate_lineage_counts(normalized))
    normalized["candidate_lineage"] = candidate_lineage_label(normalized)
    return normalized


def compute_execution_diagnostics(results: list[dict[str, Any]]) -> dict[str, int]:
    totals = {field: 0 for field in EXECUTION_DIAGNOSTIC_FIELDS}
    for result in results:
        for field, value in normalized_execution_diagnostics(result).items():
            totals[field] += value
    return totals


def compute_repair_outcome_distribution(results: list[dict[str, Any]]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for result in results:
        outcome = result.get("outcome")
        label = outcome if isinstance(outcome, str) and outcome else "unknown"
        counts[label] = counts.get(label, 0) + 1
    return dict(sorted(counts.items()))


def failure_attribution_label(result: dict[str, Any]) -> Optional[str]:
    if result.get("verified_repair"):
        return None

    explicit = result.get("failure_attribution")
    if isinstance(explicit, str) and explicit:
        return explicit

    outcome = result.get("outcome")
    if outcome == "accepted_patch_unverified":
        return "accepted_patch_failed_independent_rerun"
    if outcome == "baseline_not_reproduced":
        return "baseline_not_reproduced"
    if outcome == "false_green":
        return "mercury_verified_but_independent_rerun_failed"
    if outcome == "missing_benchmark_run":
        return "missing_benchmark_run"
    if outcome == "no_patch":
        return "no_patch_emitted"
    if outcome == "runner_error":
        return "runner_error"
    if isinstance(outcome, str) and outcome:
        return outcome
    return "unknown_failure"


def classify_failure_attribution(
    benchmark_run: dict[str, Any],
    outcome: str,
    accepted_patch: bool,
    mercury_verified: bool,
    rerun_success: bool,
) -> Optional[str]:
    if outcome == "verified_repair":
        return None

    explicit = benchmark_run.get("failure_attribution")
    if isinstance(explicit, str) and explicit:
        return explicit

    if mercury_verified and not rerun_success:
        return "mercury_verified_but_independent_rerun_failed"
    if accepted_patch and not rerun_success:
        return "accepted_patch_failed_independent_rerun"
    if outcome == "baseline_not_reproduced":
        return "baseline_not_reproduced"
    if outcome == "missing_benchmark_run":
        return "missing_benchmark_run"
    if outcome == "runner_error":
        return "runner_error"
    if outcome == "no_patch":
        return "no_patch_emitted"
    if isinstance(outcome, str) and outcome:
        return outcome
    return "unknown_failure"


def compute_failure_attribution(results: list[dict[str, Any]]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for result in results:
        label = failure_attribution_label(result)
        if label is None:
            continue
        counts[label] = counts.get(label, 0) + 1

    ordered = [label for label in FAILURE_ATTRIBUTION_ORDER if label in counts]
    extras = sorted(label for label in counts if label not in FAILURE_ATTRIBUTION_ORDER)
    return {label: counts[label] for label in ordered + extras}


def bucket_metrics(results: list[dict[str, Any]]) -> dict[str, Any]:
    attempted_cases = len(results)
    verified_repairs = sum(1 for result in results if result["verified_repair"])
    accepted_patches = sum(1 for result in results if result["accepted_patch"])
    false_greens = sum(1 for result in results if result["false_green"])
    return {
        "attempted_cases": attempted_cases,
        "verified_repairs": verified_repairs,
        "accepted_patches": accepted_patches,
        "false_greens": false_greens,
        "verified_repair_rate": verified_repairs / attempted_cases if attempted_cases else 0.0,
        "accepted_patch_rate": accepted_patches / attempted_cases if attempted_cases else 0.0,
        "false_green_rate": false_greens / attempted_cases if attempted_cases else 0.0,
        "repair_outcome_distribution": compute_repair_outcome_distribution(results),
    }


def compute_difficulty_breakdown(results: list[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    return compute_breakdown_by_label(
        results,
        lambda result: normalize_difficulty_label(result.get("difficulty")),
        DIFFICULTY_ORDER,
    )


def compute_breakdown_by_label(
    results: list[dict[str, Any]],
    label_fn,
    order: list[str],
) -> dict[str, dict[str, Any]]:
    buckets: dict[str, list[dict[str, Any]]] = {}
    for result in results:
        label = label_fn(result)
        buckets.setdefault(label, []).append(result)

    ordered = [label for label in order if label in buckets]
    extras = sorted(label for label in buckets if label not in order)
    return {label: bucket_metrics(buckets[label]) for label in ordered + extras}


def compute_candidate_attempt_breakdown(results: list[dict[str, Any]]) -> dict[str, dict[str, int]]:
    breakdown: dict[str, dict[str, int]] = {}
    for label, (attempt_field, accepted_field) in CANDIDATE_LINEAGE_FIELDS.items():
        breakdown[label] = {
            "attempts": sum(numeric_count(result.get(attempt_field)) for result in results),
            "accepted_steps": sum(
                numeric_count(result.get(accepted_field)) for result in results
            ),
        }
    return breakdown


def compute_agent_curves(results: list[dict[str, Any]]) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    by_agent: dict[int, list[dict[str, Any]]] = {}
    for result in results:
        by_agent.setdefault(int(result["agent_count"]), []).append(result)

    baseline_agent = min(by_agent) if by_agent else None
    baseline_median_duration = None
    if baseline_agent is not None:
        baseline_median_duration = median_or_none(
            [float(entry["fix_duration_ms"]) for entry in by_agent[baseline_agent]]
        )

    speedup_curve: list[dict[str, Any]] = []
    cost_curve: list[dict[str, Any]] = []
    for agent_count in sorted(by_agent):
        bucket = by_agent[agent_count]
        durations = [float(entry["fix_duration_ms"]) for entry in bucket]
        verified_times = [
            float(entry["time_to_verified_repair_ms"])
            for entry in bucket
            if entry["time_to_verified_repair_ms"] is not None
        ]
        costs = [float(entry["total_cost_usd"]) for entry in bucket]
        median_duration = median_or_none(durations)
        speedup_vs_baseline = None
        if (
            baseline_agent is not None
            and baseline_median_duration
            and median_duration
            and median_duration > 0
        ):
            speedup_vs_baseline = baseline_median_duration / median_duration

        speedup_curve.append(
            {
                "agent_count": agent_count,
                "attempted_cases": len(bucket),
                "verified_repairs": sum(
                    1 for entry in bucket if entry["verified_repair"]
                ),
                "median_duration_ms": median_duration,
                "median_time_to_verified_repair_ms": median_or_none(verified_times),
                "speedup_vs_baseline": speedup_vs_baseline,
            }
        )
        cost_curve.append(
            {
                "agent_count": agent_count,
                "attempted_cases": len(bucket),
                "median_total_cost_usd": median_or_none(costs),
                "mean_total_cost_usd": mean_or_none(costs),
            }
        )

    return speedup_curve, cost_curve


def summarize_report(report: dict[str, Any]) -> str:
    metrics = report["metrics"]
    lines = [
        f"# Mercury Repair Benchmark Report ({report['mode']})",
        "",
        f"- Schema: `{report['schema_version']}`",
        f"- Suite: `{report['suite_id']}`",
        f"- Language: `{report['language']}`",
        f"- Agent counts: `{', '.join(str(value) for value in report['agent_counts'])}`",
        f"- Keep workspaces: `{report['keep_workspaces']}`",
        f"- Attempted cases: `{metrics['attempted_cases']}`",
        f"- Verified repairs: `{metrics['verified_repairs']}`",
        f"- Accepted patch rate: `{metrics['accepted_patch_rate']:.3f}`",
        f"- False-green rate: `{metrics['false_green_rate']:.3f}`",
        f"- Median time to first candidate (ms): `{metrics['median_time_to_first_candidate_ms']}`",
        f"- Median time to verified repair (ms): `{metrics['median_time_to_verified_repair_ms']}`",
        f"- Median cost per attempted case (USD): `{metrics['median_cost_per_attempted_case_usd']}`",
        f"- Mean cost per attempted case (USD): `{metrics['mean_cost_per_attempted_case_usd']}`",
        f"- Median cost per verified repair (USD): `{metrics['median_cost_per_verified_repair_usd']}`",
        f"- Mean cost per verified repair (USD): `{metrics['mean_cost_per_verified_repair_usd']}`",
        "",
        "## Repair Outcome Distribution",
        "",
    ]

    for outcome, count in report["repair_outcome_distribution"].items():
        lines.append(f"- {outcome}: {count}")

    lines.extend(
        [
            "",
            "## Difficulty-Class Breakdown",
            "",
        ]
    )

    for difficulty, bucket in report["difficulty_breakdown"].items():
        lines.append(
            f"- {difficulty}: attempted={bucket['attempted_cases']}, verified={bucket['verified_repairs']}, accepted={bucket['accepted_patches']}, false_greens={bucket['false_greens']}, verified_rate={bucket['verified_repair_rate']:.3f}"
        )

    lines.extend(
        [
            "",
            "## Tier Breakdown",
            "",
        ]
    )

    for tier, bucket in report["tier_breakdown"].items():
        lines.append(
            f"- {tier}: attempted={bucket['attempted_cases']}, verified={bucket['verified_repairs']}, accepted={bucket['accepted_patches']}, false_greens={bucket['false_greens']}, verified_rate={bucket['verified_repair_rate']:.3f}"
        )

    lines.extend(
        [
            "",
            "## Verifier-Class Breakdown",
            "",
        ]
    )

    for verifier_class, bucket in report["verifier_class_breakdown"].items():
        lines.append(
            f"- {verifier_class}: attempted={bucket['attempted_cases']}, verified={bucket['verified_repairs']}, accepted={bucket['accepted_patches']}, false_greens={bucket['false_greens']}, verified_rate={bucket['verified_repair_rate']:.3f}"
        )

    lines.extend(
        [
            "",
            "## Candidate Lineage Breakdown",
            "",
        ]
    )

    for lineage, bucket in report["candidate_lineage_breakdown"].items():
        lines.append(
            f"- {lineage}: attempted={bucket['attempted_cases']}, verified={bucket['verified_repairs']}, accepted={bucket['accepted_patches']}, false_greens={bucket['false_greens']}, verified_rate={bucket['verified_repair_rate']:.3f}"
        )

    lines.extend(
        [
            "",
            "## Candidate Lineage Attempts",
            "",
        ]
    )

    for lineage, bucket in report["candidate_attempt_breakdown"].items():
        lines.append(
            f"- {lineage}: attempts={bucket['attempts']}, accepted_steps={bucket['accepted_steps']}"
        )

    lines.extend(
        [
            "",
            "## Failure Attribution",
            "",
        ]
    )

    for label, count in report["failure_attribution"].items():
        lines.append(f"- {label}: {count}")

    lines.extend(
        [
            "",
            "## Execution Diagnostics",
            "",
        ]
    )

    for label in EXECUTION_DIAGNOSTIC_FIELDS:
        lines.append(f"- {label}: {report['execution_diagnostics'].get(label, 0)}")

    lines.extend(
        [
            "",
            "## Speedup Curve",
            "",
        ]
    )

    for entry in report["speedup_curve"]:
        lines.append(
            f"- agents={entry['agent_count']}: attempted={entry['attempted_cases']}, verified={entry['verified_repairs']}, median_duration_ms={entry['median_duration_ms']}, median_verified_ms={entry['median_time_to_verified_repair_ms']}, speedup_vs_baseline={entry['speedup_vs_baseline']}"
        )

    lines.extend(["", "## Cost Curve", ""])
    for entry in report["cost_curve"]:
        lines.append(
            f"- agents={entry['agent_count']}: attempted={entry['attempted_cases']}, median_cost_usd={entry['median_total_cost_usd']}, mean_cost_usd={entry['mean_total_cost_usd']}"
        )

    lines.extend(["", "## Case Results", ""])
    for result in report["results"]:
        lines.append(
            f"- {result['case_id']} [{normalize_difficulty_label(result.get('difficulty'))}/{result.get('tier', 'unknown')}/{result.get('verifier_class', 'unknown')}/{result.get('candidate_lineage', 'unknown')}] @ {result['agent_count']} agents: outcome={result['outcome']}, verified_repair={result['verified_repair']}, false_green={result['false_green']}, total_cost_usd={result['total_cost_usd']}"
        )

    lines.append("")
    return "\n".join(lines)


def render_selection(
    manifest: dict[str, Any],
    selected_cases: list[dict[str, Any]],
    args: argparse.Namespace,
) -> dict[str, Any]:
    fixture_counts: dict[str, int] = {}
    for case in selected_cases:
        fixture_counts[case["path"]] = fixture_counts.get(case["path"], 0) + 1

    return {
        "manifest_path": str(args.suite.resolve()),
        "requested_case_ids": args.case or [],
        "requested_stages": args.stage or [],
        "requested_failure_classes": args.failure_class or [],
        "requested_difficulties": args.difficulty or [],
        "requested_tags": args.tag or [],
        "requested_limit": args.limit,
        "selected_case_ids": [case["id"] for case in selected_cases],
        "selected_count": len(selected_cases),
        "selected_unique_fixture_paths": len(fixture_counts),
        "selected_fixture_path_reuse": dict(sorted(fixture_counts.items())),
        "mode": args.mode,
        "manifest_suite_id": manifest["suite_id"],
    }


def case_result_root(run_root: Path, case: dict[str, Any], agent_count: int) -> Path:
    return run_root / "cases" / case["id"] / f"agents-{agent_count}"


def case_workspace_root(run_root: Path, case: dict[str, Any], agent_count: int) -> Path:
    return run_root / "workspaces" / f"{case['id']}-agents-{agent_count}"


def load_existing_result(result_path: Path) -> Optional[dict[str, Any]]:
    if not result_path.exists():
        return None
    payload = read_json(result_path)
    require_manifest_keys(
        payload,
        [
            "schema_version",
            "case_id",
            "agent_count",
            "outcome",
            "accepted_patch",
            "verified_repair",
            "false_green",
            "fix_duration_ms",
            "total_cost_usd",
        ],
        "benchmark result",
    )
    if payload["schema_version"] != BENCHMARK_RESULT_SCHEMA_VERSION:
        raise ValueError(
            f"benchmark result at {result_path} declared unsupported schema version {payload['schema_version']}"
        )
    return normalized_case_result(payload)


def build_runner_error_result(
    case: dict[str, Any],
    agent_count: int,
    keep_workspaces: bool,
    suite_id: Optional[str],
    manifest_path: Optional[Path],
    message: str,
    traceback_text: str,
) -> dict[str, Any]:
    return {
        "schema_version": BENCHMARK_RESULT_SCHEMA_VERSION,
        "case_id": case["id"],
        "case_title": case["title"],
        "case_path": case["path"],
        "difficulty": normalize_difficulty_label(case.get("difficulty")),
        "tier": derive_tier(case, suite_id, manifest_path),
        "verifier_class": classify_verifier_command(case.get("verifier_command")),
        "failure_stage": case["failure_stage"],
        "failure_class": case["failure_class"],
        "agent_count": agent_count,
        "baseline_reproduced": False,
        "outcome": "runner_error",
        "accepted_patch": False,
        "accepted_patch_bytes": 0,
        "verified_repair": False,
        "false_green": False,
        "failure_attribution": "runner_error",
        "fix_exit_code": None,
        "fix_timed_out": False,
        "fix_duration_ms": 0,
        "time_to_first_candidate_ms": None,
        "time_to_verified_repair_ms": None,
        "total_cost_usd": 0.0,
        "budget_remaining_usd": 0.0,
        "final_bundle_verified": False,
        "applied": False,
        "independent_rerun_success": False,
        "independent_rerun_exit_code": None,
        "independent_rerun_timed_out": False,
        "benchmark_run_path": None,
        "candidate_workspace": None,
        "workspace_preserved": keep_workspaces,
        "runner_error_message": message,
        "runner_error_traceback": traceback_text,
        **normalized_execution_diagnostics({}),
        **normalized_candidate_lineage_counts({}),
        "candidate_lineage": "unknown",
    }


def build_report(
    manifest: dict[str, Any],
    selected_cases: list[dict[str, Any]],
    results: list[dict[str, Any]],
    args: argparse.Namespace,
    run_root: Path,
    started_at: datetime,
    finished_at: datetime,
    api_key_env: Optional[str],
) -> dict[str, Any]:
    normalized_results = [normalized_case_result(result) for result in results]
    attempted_cases = len(normalized_results)
    verified_repairs = sum(1 for result in normalized_results if result["verified_repair"])
    false_greens = sum(1 for result in normalized_results if result["false_green"])
    accepted_patches = sum(1 for result in normalized_results if result["accepted_patch"])
    first_candidate_times = [
        result["time_to_first_candidate_ms"]
        for result in normalized_results
        if result["time_to_first_candidate_ms"] is not None
    ]
    verified_times = [
        result["time_to_verified_repair_ms"]
        for result in normalized_results
        if result["time_to_verified_repair_ms"] is not None and result["verified_repair"]
    ]
    attempted_costs = [float(result["total_cost_usd"]) for result in normalized_results]
    verified_costs = [
        float(result["total_cost_usd"])
        for result in normalized_results
        if result["verified_repair"]
    ]
    repair_outcome_distribution = compute_repair_outcome_distribution(normalized_results)
    difficulty_breakdown = compute_difficulty_breakdown(normalized_results)
    tier_breakdown = compute_breakdown_by_label(
        normalized_results,
        lambda result: normalize_tier_label(result.get("tier")),
        TIER_ORDER,
    )
    verifier_class_breakdown = compute_breakdown_by_label(
        normalized_results,
        lambda result: (
            result.get("verifier_class")
            if isinstance(result.get("verifier_class"), str)
            else "unknown"
        ),
        VERIFIER_CLASS_ORDER,
    )
    candidate_lineage_breakdown = compute_breakdown_by_label(
        normalized_results,
        lambda result: (
            result.get("candidate_lineage")
            if isinstance(result.get("candidate_lineage"), str)
            else "unknown"
        ),
        CANDIDATE_LINEAGE_ORDER,
    )
    candidate_attempt_breakdown = compute_candidate_attempt_breakdown(normalized_results)
    failure_attribution = compute_failure_attribution(normalized_results)
    execution_diagnostics = compute_execution_diagnostics(normalized_results)
    speedup_curve, cost_curve = compute_agent_curves(normalized_results)

    return {
        "schema_version": BENCHMARK_SCHEMA_VERSION,
        "description": "Public Mercury repair benchmark aggregate report",
        "suite_id": manifest["suite_id"],
        "language": manifest["language"],
        "mode": args.mode,
        "generated_at": isoformat_utc(finished_at),
        "run_id": args.run_id,
        "run_root": str(run_root),
        "binary_path": str(args.binary.resolve()),
        "agent_counts": args.agent_count,
        "keep_workspaces": args.keep_workspaces,
        "max_cost_usd": args.max_cost,
        "timeout_seconds": args.timeout_seconds,
        "api_key_env": api_key_env,
        "manifest": {
            "schema_version": manifest["schema_version"],
            "version": manifest["version"],
            "artifact_schema_version": manifest["artifact_schema_version"],
            "supported_modes": manifest["supported_modes"],
        },
        "selection": render_selection(manifest, selected_cases, args),
        "started_at": isoformat_utc(started_at),
        "finished_at": isoformat_utc(finished_at),
        "duration_ms": int((finished_at - started_at).total_seconds() * 1000),
        "metrics": {
            "attempted_cases": attempted_cases,
            "verified_repairs": verified_repairs,
            "accepted_patches": accepted_patches,
            "false_greens": false_greens,
            "verified_repair_rate": verified_repairs / attempted_cases if attempted_cases else 0.0,
            "accepted_patch_rate": accepted_patches / attempted_cases if attempted_cases else 0.0,
            "false_green_rate": false_greens / attempted_cases if attempted_cases else 0.0,
            "median_time_to_first_candidate_ms": median_or_none(first_candidate_times),
            "median_time_to_verified_repair_ms": median_or_none(verified_times),
            "median_cost_per_attempted_case_usd": median_or_none(attempted_costs),
            "mean_cost_per_attempted_case_usd": mean_or_none(attempted_costs),
            "median_cost_per_verified_repair_usd": median_or_none(verified_costs),
            "mean_cost_per_verified_repair_usd": mean_or_none(verified_costs),
        },
        "repair_outcome_distribution": repair_outcome_distribution,
        "difficulty_breakdown": difficulty_breakdown,
        "tier_breakdown": tier_breakdown,
        "verifier_class_breakdown": verifier_class_breakdown,
        "candidate_lineage_breakdown": candidate_lineage_breakdown,
        "candidate_attempt_breakdown": candidate_attempt_breakdown,
        "failure_attribution": failure_attribution,
        "execution_diagnostics": execution_diagnostics,
        "speedup_curve": speedup_curve,
        "cost_curve": cost_curve,
        "results": normalized_results,
    }


def evaluate_case(
    run_root: Path,
    case: dict[str, Any],
    agent_count: int,
    suite_id: str,
    args: argparse.Namespace,
    api_key_env: str,
    env: dict[str, str],
) -> dict[str, Any]:
    workspace_root = case_workspace_root(run_root, case, agent_count)
    result_root = case_result_root(run_root, case, agent_count)
    tier = derive_tier(case, suite_id, args.suite)
    verifier_class = classify_verifier_command(case.get("verifier_command"))
    try:
        if workspace_root.exists():
            shutil.rmtree(workspace_root)
        if result_root.exists():
            shutil.rmtree(result_root)

        shutil.copytree(args.suite.parent / case["path"], workspace_root)
        result_root.mkdir(parents=True, exist_ok=True)

        baseline = run_command(
            list(case["verifier_command"]),
            workspace_root,
            min(int(case.get("timeout_seconds", args.timeout_seconds)), args.timeout_seconds),
            env,
        )
        write_command_logs(result_root, "baseline", baseline)
        if baseline["success"]:
            result = {
                "schema_version": BENCHMARK_RESULT_SCHEMA_VERSION,
                "case_id": case["id"],
                "case_title": case["title"],
                "case_path": case["path"],
                "difficulty": normalize_difficulty_label(case.get("difficulty")),
                "tier": tier,
                "verifier_class": verifier_class,
                "failure_stage": case["failure_stage"],
                "failure_class": case["failure_class"],
                "agent_count": agent_count,
                "baseline_reproduced": False,
                "outcome": "baseline_not_reproduced",
                "accepted_patch": False,
                "accepted_patch_bytes": 0,
                "verified_repair": False,
                "false_green": False,
                "failure_attribution": "baseline_not_reproduced",
                "total_cost_usd": 0.0,
                "fix_duration_ms": 0,
                "time_to_first_candidate_ms": None,
                "time_to_verified_repair_ms": None,
                "candidate_workspace": None,
                "workspace_preserved": args.keep_workspaces,
                **normalized_execution_diagnostics({}),
                **normalized_candidate_lineage_counts({}),
                "candidate_lineage": "unknown",
            }
            write_json(result_root / "result.json", result)
            return result

        run_command([str(args.binary.resolve()), "init"], workspace_root, 60, env)
        configure_mercury(workspace_root, case, api_key_env)

        fix_result = run_command(
            [
                str(args.binary.resolve()),
                "fix",
                case["title"],
                "--max-agents",
                str(agent_count),
                "--max-cost",
                str(args.max_cost),
                "--noninteractive",
            ],
            workspace_root,
            args.timeout_seconds,
            env,
        )
        write_command_logs(result_root, "fix", fix_result)

        latest_run = latest_mercury_run(workspace_root)
        benchmark_run: dict[str, Any] = {}
        if latest_run:
            source_artifact = latest_run / "benchmark-run.json"
            if source_artifact.exists():
                benchmark_run = read_json(source_artifact)
                write_json(
                    result_root / "benchmark-run.json",
                    sanitized_benchmark_run(benchmark_run, args.keep_workspaces),
                )

        accepted_patch = bool(
            benchmark_run.get("accepted_patch", benchmark_run.get("accepted_patch_present"))
        )
        candidate_workspace = candidate_workspace_from_benchmark_run(benchmark_run)
        rerun_timeout = min(int(case.get("timeout_seconds", args.timeout_seconds)), args.timeout_seconds)
        rerun: dict[str, Any]
        if candidate_workspace is not None:
            rerun = run_command(
                list(case["verifier_command"]),
                candidate_workspace,
                rerun_timeout,
                env,
            )
        else:
            rerun = {
                "command": list(case["verifier_command"]),
                "command_text": command_text(list(case["verifier_command"])),
                "cwd": None,
                "started_at": None,
                "finished_at": None,
                "duration_ms": 0,
                "timeout_seconds": rerun_timeout,
                "timed_out": False,
                "exit_code": None,
                "stdout": "",
                "stderr": "",
                "success": False,
                "error": "missing final-bundle candidate workspace",
            }
        write_command_logs(result_root, "independent-rerun", rerun)

        mercury_verified = bool(benchmark_run.get("final_bundle_verified"))
        rerun_success = bool(rerun.get("success"))
        verified_repair = accepted_patch and rerun_success
        false_green = mercury_verified and not rerun_success
        outcome = classify_case_outcome(
            benchmark_run,
            accepted_patch,
            mercury_verified,
            rerun_success,
        )
        failure_attribution = classify_failure_attribution(
            benchmark_run,
            outcome,
            accepted_patch,
            mercury_verified,
            rerun_success,
        )

        result = {
            "schema_version": BENCHMARK_RESULT_SCHEMA_VERSION,
            "case_id": case["id"],
            "case_title": case["title"],
            "case_path": case["path"],
            "difficulty": normalize_difficulty_label(case.get("difficulty")),
            "tier": tier,
            "verifier_class": verifier_class,
            "failure_stage": case["failure_stage"],
            "failure_class": case["failure_class"],
            "agent_count": agent_count,
            "baseline_reproduced": True,
            "outcome": outcome,
            "accepted_patch": accepted_patch,
            "accepted_patch_bytes": int(benchmark_run.get("accepted_patch_bytes") or 0),
            "verified_repair": verified_repair,
            "false_green": false_green,
            "failure_attribution": failure_attribution,
            **normalized_execution_diagnostics(benchmark_run),
            **normalized_candidate_lineage_counts(benchmark_run),
            "fix_exit_code": fix_result.get("exit_code"),
            "fix_timed_out": fix_result.get("timed_out"),
            "fix_duration_ms": int(benchmark_run.get("duration_ms") or fix_result["duration_ms"]),
            "time_to_first_candidate_ms": benchmark_run.get("time_to_first_candidate_ms"),
            "time_to_verified_repair_ms": benchmark_run.get("time_to_verified_repair_ms"),
            "total_cost_usd": float(benchmark_run.get("total_cost_usd") or 0.0),
            "budget_remaining_usd": float(benchmark_run.get("budget_remaining_usd") or 0.0),
            "final_bundle_verified": mercury_verified,
            "applied": bool(benchmark_run.get("applied")),
            "independent_rerun_success": rerun_success,
            "independent_rerun_exit_code": rerun.get("exit_code"),
            "independent_rerun_timed_out": rerun.get("timed_out"),
            "benchmark_run_path": str(result_root / "benchmark-run.json")
            if benchmark_run
            else None,
            "candidate_workspace": str(candidate_workspace)
            if candidate_workspace and args.keep_workspaces
            else None,
            "workspace_preserved": args.keep_workspaces,
        }
        result["candidate_lineage"] = candidate_lineage_label(result)
        write_json(result_root / "result.json", result)
        return result
    finally:
        if not args.keep_workspaces:
            shutil.rmtree(workspace_root, ignore_errors=True)


def write_report_bundle(
    manifest: dict[str, Any],
    selected_cases: list[dict[str, Any]],
    results: list[dict[str, Any]],
    args: argparse.Namespace,
    run_root: Path,
    started_at: datetime,
    finished_at: datetime,
    api_key_env: Optional[str],
    *,
    partial: bool,
) -> None:
    report = build_report(
        manifest,
        selected_cases,
        results,
        args,
        run_root,
        started_at,
        finished_at,
        api_key_env,
    )
    if partial:
        report["status"] = "partial"
        report["expected_attempts"] = len(selected_cases) * len(args.agent_count)
        write_json(run_root / "report.partial.json", report)
        write_text(run_root / "summary.partial.md", summarize_report(report))
        return

    write_json(run_root / "report.json", report)
    write_text(run_root / "summary.md", summarize_report(report))


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run the Mercury repair benchmark over a manifest-selected corpus."
    )
    parser.add_argument(
        "--suite",
        type=Path,
        default=DEFAULT_MANIFEST_PATH,
        help="Path to the eval manifest to benchmark (default: evals/v0/tier1-manifest.json).",
    )
    parser.add_argument(
        "--binary",
        type=Path,
        default=REPO_ROOT / "target" / "release" / "mercury-cli",
        help="Path to the mercury-cli binary to execute.",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=DEFAULT_OUTPUT_DIR,
        help="Directory where benchmark reports should be written.",
    )
    parser.add_argument(
        "--run-id",
        default=now_utc().strftime(RUN_ID_FORMAT),
        help="Stable run identifier used to name the output directory.",
    )
    parser.add_argument(
        "--mode",
        choices=["quality", "agent-sweep"],
        default="quality",
        help="quality runs the selected set once; agent-sweep repeats a representative subset across multiple agent counts.",
    )
    parser.add_argument("--case", action="append", help="Repeatable logical case id filter.")
    parser.add_argument("--stage", action="append", help="Repeatable failure_stage filter.")
    parser.add_argument(
        "--failure-class",
        action="append",
        help="Repeatable failure_class filter.",
    )
    parser.add_argument(
        "--difficulty",
        action="append",
        choices=DIFFICULTY_ORDER,
        help="Repeatable difficulty filter.",
    )
    parser.add_argument("--tag", action="append", help="Repeatable tag filter.")
    parser.add_argument("--limit", type=int, help="Maximum number of selected cases.")
    parser.add_argument(
        "--representative-count",
        type=int,
        default=DEFAULT_REPRESENTATIVE_COUNT,
        help="Representative case count for agent-sweep mode.",
    )
    parser.add_argument(
        "--agent-count",
        action="append",
        type=int,
        help="Repeatable max-agents value. Defaults to 4 for quality and 1/2/4/8 for agent-sweep.",
    )
    parser.add_argument(
        "--timeout-seconds",
        type=int,
        default=DEFAULT_TIMEOUT_SECONDS,
        help="Benchmark timeout for each mercury fix execution.",
    )
    parser.add_argument(
        "--max-cost",
        type=float,
        default=DEFAULT_MAX_COST_USD,
        help="Per-case Mercury spend cap in USD.",
    )
    parser.add_argument(
        "--list",
        action="store_true",
        help="Print selected case ids and titles, then exit.",
    )
    parser.add_argument(
        "--list-json",
        action="store_true",
        help="Print the selected cases as JSON, then exit.",
    )
    parser.add_argument(
        "--clean-output",
        action="store_true",
        help="Delete an existing run directory before writing the new bundle.",
    )
    parser.add_argument(
        "--resume",
        action="store_true",
        help="Reuse existing per-case result.json files under the run directory and continue unfinished work.",
    )
    parser.add_argument(
        "--keep-workspaces",
        action="store_true",
        help="Preserve copied case workspaces and sandbox_run_root metadata for debugging.",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    manifest = load_manifest(args.suite.resolve())
    selected_cases = select_cases(
        manifest,
        args.case,
        args.stage,
        args.failure_class,
        args.difficulty,
        args.tag,
        args.limit,
    )
    if args.mode == "agent-sweep":
        selected_cases = representative_cases(selected_cases, args.representative_count)

    if args.list:
        for case in selected_cases:
            print(f"{case['id']}\t{case['title']}")
        return 0

    if args.list_json:
        print(json.dumps(selected_cases, indent=2, sort_keys=True))
        return 0

    if args.agent_count:
        args.agent_count = sorted(dict.fromkeys(args.agent_count))
    elif args.mode == "agent-sweep":
        args.agent_count = list(DEFAULT_AGENT_SWEEP)
    else:
        args.agent_count = [DEFAULT_QUALITY_AGENT_COUNT]

    if not args.binary.exists():
        raise SystemExit(f"benchmark binary not found: {args.binary}")
    if args.timeout_seconds <= 0:
        raise SystemExit("--timeout-seconds must be positive")
    if args.max_cost <= 0:
        raise SystemExit("--max-cost must be positive")

    api_key_env = find_api_key_env()
    run_root = args.output_dir.resolve() / f"run-{args.run_id}"
    if run_root.exists() and args.clean_output:
        shutil.rmtree(run_root)
    run_root.mkdir(parents=True, exist_ok=True)

    started_at = now_utc()
    env = os.environ.copy()
    results: list[dict[str, Any]] = []

    write_json(run_root / "manifest.snapshot.json", manifest)
    write_json(run_root / "environment.json", {
        "schema_version": BENCHMARK_SCHEMA_VERSION,
        "binary_path": str(args.binary.resolve()),
        "manifest_path": str(args.suite.resolve()),
        "api_key_env": api_key_env,
        "mode": args.mode,
        "agent_counts": args.agent_count,
        "keep_workspaces": args.keep_workspaces,
        "max_cost_usd": args.max_cost,
        "timeout_seconds": args.timeout_seconds,
    })
    write_json(run_root / "selection.json", render_selection(manifest, selected_cases, args))
    write_report_bundle(
        manifest,
        selected_cases,
        results,
        args,
        run_root,
        started_at,
        started_at,
        api_key_env,
        partial=True,
    )

    for agent_count in args.agent_count:
        for case in selected_cases:
            result_root = case_result_root(run_root, case, agent_count)
            result_path = result_root / "result.json"
            if args.resume:
                existing = load_existing_result(result_path)
                if existing is not None:
                    results.append(existing)
                    write_report_bundle(
                        manifest,
                        selected_cases,
                        results,
                        args,
                        run_root,
                        started_at,
                        now_utc(),
                        api_key_env,
                        partial=True,
                    )
                    continue
            try:
                result = evaluate_case(
                    run_root,
                    case,
                    agent_count,
                    manifest["suite_id"],
                    args,
                    api_key_env,
                    env,
                )
            except Exception as exc:
                result_root.mkdir(parents=True, exist_ok=True)
                result = build_runner_error_result(
                    case,
                    agent_count,
                    args.keep_workspaces,
                    manifest["suite_id"],
                    args.suite,
                    str(exc),
                    traceback.format_exc(),
                )
                write_json(result_path, result)
            results.append(result)
            write_report_bundle(
                manifest,
                selected_cases,
                results,
                args,
                run_root,
                started_at,
                now_utc(),
                api_key_env,
                partial=True,
            )

    finished_at = now_utc()
    write_report_bundle(
        manifest,
        selected_cases,
        results,
        args,
        run_root,
        started_at,
        finished_at,
        api_key_env,
        partial=False,
    )
    print(json.dumps({"run_root": str(run_root), "report_path": str(run_root / "report.json")}))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except ValueError as exc:
        print(str(exc), file=sys.stderr)
        raise SystemExit(2)
    except RuntimeError as exc:
        print(str(exc), file=sys.stderr)
        raise SystemExit(2)
