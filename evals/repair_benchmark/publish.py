#!/usr/bin/env python3
import argparse
import json
import re
import shutil
import sys
from pathlib import Path
from typing import Any, Optional

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT_DIR = REPO_ROOT / "docs" / "benchmarks"
BENCHMARK_SCHEMA_VERSION = "mercury-repair-benchmark-v1"
TRACK_TITLE = "Rust V0 Repair Benchmark"
TRACK_MARKDOWN = "rust-v0-repair-benchmark.md"
QUALITY_REPORT_NAME = "rust-v0-quality.report.json"
AGENT_SWEEP_REPORT_NAME = "rust-v0-agent-sweep.report.json"
PENDING_STATUS = "pending first checked-in secret-backed Tier 1 Rust beta run."
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


def read_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def write_text(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def write_json(path: Path, payload: Any) -> None:
    write_text(path, json.dumps(payload, indent=2, sort_keys=True) + "\n")


def validate_report(path: Path, expected_mode: str) -> dict[str, Any]:
    payload = read_json(path)
    if payload.get("schema_version") != BENCHMARK_SCHEMA_VERSION:
        raise ValueError(
            f"{path} must declare {BENCHMARK_SCHEMA_VERSION}, got {payload.get('schema_version')!r}"
        )
    if payload.get("mode") != expected_mode:
        raise ValueError(f"{path} must be mode={expected_mode!r}, got {payload.get('mode')!r}")
    return payload


def repo_relative_path(value: Any) -> Any:
    if not isinstance(value, str) or not value:
        return value
    try:
        return str(Path(value).resolve().relative_to(REPO_ROOT))
    except ValueError:
        return None


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


def repair_outcome_distribution(results: list[dict[str, Any]]) -> dict[str, int]:
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
    verified_repairs = sum(1 for result in results if result.get("verified_repair"))
    accepted_patches = sum(1 for result in results if result.get("accepted_patch"))
    false_greens = sum(1 for result in results if result.get("false_green"))
    return {
        "attempted_cases": attempted_cases,
        "verified_repairs": verified_repairs,
        "accepted_patches": accepted_patches,
        "false_greens": false_greens,
        "verified_repair_rate": verified_repairs / attempted_cases if attempted_cases else 0.0,
        "accepted_patch_rate": accepted_patches / attempted_cases if attempted_cases else 0.0,
        "false_green_rate": false_greens / attempted_cases if attempted_cases else 0.0,
        "repair_outcome_distribution": repair_outcome_distribution(results),
    }


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


def compute_difficulty_breakdown(results: list[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    return compute_breakdown_by_label(
        results,
        lambda result: normalize_difficulty_label(result.get("difficulty")),
        DIFFICULTY_ORDER,
    )


def compute_execution_diagnostics(results: list[dict[str, Any]]) -> dict[str, int]:
    counts = normalized_execution_diagnostics({})
    for result in results:
        diagnostics = normalized_execution_diagnostics(result)
        for field in EXECUTION_DIAGNOSTIC_FIELDS:
            counts[field] += diagnostics[field]
    return counts


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


def sanitize_public_report(report: dict[str, Any]) -> dict[str, Any]:
    sanitized = dict(report)
    for field in ["run_root", "binary_path", "api_key_env"]:
        sanitized.pop(field, None)

    selection = dict(sanitized.get("selection", {}))
    if "manifest_path" in selection:
        manifest_path = repo_relative_path(selection["manifest_path"])
        if manifest_path is None:
            selection.pop("manifest_path", None)
        else:
            selection["manifest_path"] = manifest_path
    sanitized["selection"] = selection

    sanitized_results: list[dict[str, Any]] = []
    for result in sanitized.get("results", []):
        public_result = normalized_case_result(dict(result))
        public_result.pop("benchmark_run_path", None)
        public_result.pop("candidate_workspace", None)
        sanitized_results.append(public_result)
    sanitized["results"] = sanitized_results
    sanitized["repair_outcome_distribution"] = repair_outcome_distribution(sanitized_results)
    sanitized["execution_diagnostics"] = compute_execution_diagnostics(sanitized_results)
    sanitized["difficulty_breakdown"] = compute_difficulty_breakdown(sanitized_results)
    sanitized["tier_breakdown"] = compute_breakdown_by_label(
        sanitized_results,
        lambda result: normalize_tier_label(result.get("tier")),
        TIER_ORDER,
    )
    sanitized["verifier_class_breakdown"] = compute_breakdown_by_label(
        sanitized_results,
        lambda result: (
            result.get("verifier_class")
            if isinstance(result.get("verifier_class"), str)
            else "unknown"
        ),
        VERIFIER_CLASS_ORDER,
    )
    sanitized["candidate_lineage_breakdown"] = compute_breakdown_by_label(
        sanitized_results,
        lambda result: (
            result.get("candidate_lineage")
            if isinstance(result.get("candidate_lineage"), str)
            else "unknown"
        ),
        CANDIDATE_LINEAGE_ORDER,
    )
    sanitized["candidate_attempt_breakdown"] = compute_candidate_attempt_breakdown(
        sanitized_results
    )
    sanitized["failure_attribution"] = compute_failure_attribution(sanitized_results)
    return sanitized


def assert_same_track(quality: dict[str, Any], agent_sweep: dict[str, Any]) -> None:
    for key in ["suite_id", "language"]:
        if quality.get(key) != agent_sweep.get(key):
            raise ValueError(
                f"quality and agent-sweep reports must agree on {key}: "
                f"{quality.get(key)!r} != {agent_sweep.get(key)!r}"
            )


def format_metric(value: Any, precision: int = 3) -> str:
    if value is None:
        return "n/a"
    if isinstance(value, float):
        return f"{value:.{precision}f}"
    return str(value)


def selection_summary(report: dict[str, Any]) -> list[str]:
    selection = report["selection"]
    manifest_path = selection.get("manifest_path", "n/a")
    return [
        f"- Manifest: `{manifest_path}`",
        f"- Selected cases: `{selection['selected_count']}`",
        f"- Unique fixture paths: `{selection['selected_unique_fixture_paths']}`",
        f"- Requested stages: `{', '.join(selection['requested_stages']) if selection['requested_stages'] else 'all selected Rust stages'}`",
        f"- Requested difficulties: `{', '.join(selection.get('requested_difficulties', [])) if selection.get('requested_difficulties') else 'all difficulties'}`",
        f"- Requested limit: `{selection['requested_limit'] if selection['requested_limit'] is not None else 'none'}`",
    ]


def render_curve_table(entries: list[dict[str, Any]], cost: bool) -> list[str]:
    if cost:
        lines = [
            "| agents | attempted | median cost usd | mean cost usd |",
            "| --- | ---: | ---: | ---: |",
        ]
        for entry in entries:
            lines.append(
                "| {agent_count} | {attempted_cases} | {median} | {mean} |".format(
                    agent_count=entry["agent_count"],
                    attempted_cases=entry["attempted_cases"],
                    median=format_metric(entry["median_total_cost_usd"]),
                    mean=format_metric(entry["mean_total_cost_usd"]),
                )
            )
        return lines

    lines = [
        "| agents | attempted | verified | median duration ms | median verified ms | speedup vs baseline |",
        "| --- | ---: | ---: | ---: | ---: | ---: |",
    ]
    for entry in entries:
        lines.append(
            "| {agent_count} | {attempted_cases} | {verified_repairs} | {median_duration} | {median_verified} | {speedup} |".format(
                agent_count=entry["agent_count"],
                attempted_cases=entry["attempted_cases"],
                verified_repairs=entry["verified_repairs"],
                median_duration=format_metric(entry["median_duration_ms"]),
                median_verified=format_metric(entry["median_time_to_verified_repair_ms"]),
                speedup=format_metric(entry["speedup_vs_baseline"]),
            )
        )
    return lines


def render_outcome_table(quality: dict[str, Any], agent_sweep: dict[str, Any]) -> list[str]:
    quality_counts = quality["repair_outcome_distribution"]
    agent_counts = agent_sweep["repair_outcome_distribution"]
    outcomes = sorted(set(quality_counts) | set(agent_counts))
    lines = [
        "| outcome | quality count | agent-sweep count |",
        "| --- | ---: | ---: |",
    ]
    for outcome in outcomes:
        lines.append(
            "| {outcome} | {quality_count} | {agent_count} |".format(
                outcome=outcome,
                quality_count=quality_counts.get(outcome, 0),
                agent_count=agent_counts.get(outcome, 0),
            )
        )
    return lines


def ordered_labels(
    primary_order: list[str], first: dict[str, Any], second: dict[str, Any]
) -> list[str]:
    labels = set(first) | set(second)
    ordered = [label for label in primary_order if label in labels]
    extras = sorted(label for label in labels if label not in primary_order)
    return ordered + extras


def render_breakdown_table(
    title: str,
    report: dict[str, Any],
    breakdown_key: str,
    order: list[str],
    label_title: str,
) -> list[str]:
    breakdown = report.get(breakdown_key, {})
    lines = [
        f"### {title}",
        "",
        f"| {label_title} | attempted | verified | accepted | false greens | verified rate | accepted rate | false-green rate | outcomes |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |",
    ]
    for label in ordered_labels(order, breakdown, {}):
        bucket = breakdown[label]
        outcome_summary = ", ".join(
            f"{outcome}={count}" for outcome, count in bucket["repair_outcome_distribution"].items()
        )
        lines.append(
            "| {label} | {attempted} | {verified} | {accepted} | {false_greens} | {verified_rate} | {accepted_rate} | {false_green_rate} | {outcomes} |".format(
                label=label,
                attempted=bucket["attempted_cases"],
                verified=bucket["verified_repairs"],
                accepted=bucket["accepted_patches"],
                false_greens=bucket["false_greens"],
                verified_rate=format_metric(bucket["verified_repair_rate"]),
                accepted_rate=format_metric(bucket["accepted_patch_rate"]),
                false_green_rate=format_metric(bucket["false_green_rate"]),
                outcomes=outcome_summary or "n/a",
            )
        )
    return lines


def render_failure_attribution_table(
    quality: dict[str, Any], agent_sweep: dict[str, Any]
) -> list[str]:
    quality_counts = quality.get("failure_attribution", {})
    agent_counts = agent_sweep.get("failure_attribution", {})
    labels = ordered_labels(FAILURE_ATTRIBUTION_ORDER, quality_counts, agent_counts)
    lines = [
        "| failure attribution | quality count | agent-sweep count |",
        "| --- | ---: | ---: |",
    ]
    for label in labels:
        lines.append(
            "| {label} | {quality_count} | {agent_count} |".format(
                label=label,
                quality_count=quality_counts.get(label, 0),
                agent_count=agent_counts.get(label, 0),
            )
        )
    return lines


def render_execution_diagnostics_table(
    quality: dict[str, Any], agent_sweep: dict[str, Any]
) -> list[str]:
    quality_counts = quality.get("execution_diagnostics", {})
    agent_counts = agent_sweep.get("execution_diagnostics", {})
    lines = [
        "| execution diagnostic | quality count | agent-sweep count |",
        "| --- | ---: | ---: |",
    ]
    for field in EXECUTION_DIAGNOSTIC_FIELDS:
        lines.append(
            "| {field} | {quality_count} | {agent_count} |".format(
                field=field,
                quality_count=quality_counts.get(field, 0),
                agent_count=agent_counts.get(field, 0),
            )
        )
    return lines


def render_candidate_attempt_table(
    quality: dict[str, Any], agent_sweep: dict[str, Any]
) -> list[str]:
    quality_counts = quality.get("candidate_attempt_breakdown", {})
    agent_counts = agent_sweep.get("candidate_attempt_breakdown", {})
    lines = [
        "| candidate lineage | quality attempts | quality accepted steps | agent-sweep attempts | agent-sweep accepted steps |",
        "| --- | ---: | ---: | ---: | ---: |",
    ]
    for label in ordered_labels(CANDIDATE_LINEAGE_ORDER, quality_counts, agent_counts):
        quality_bucket = quality_counts.get(label, {})
        agent_bucket = agent_counts.get(label, {})
        lines.append(
            "| {label} | {quality_attempts} | {quality_accepted} | {agent_attempts} | {agent_accepted} |".format(
                label=label,
                quality_attempts=quality_bucket.get("attempts", 0),
                quality_accepted=quality_bucket.get("accepted_steps", 0),
                agent_attempts=agent_bucket.get("attempts", 0),
                agent_accepted=agent_bucket.get("accepted_steps", 0),
            )
        )
    return lines


def render_pending_markdown() -> str:
    lines = [
        f"# {TRACK_TITLE}",
        "",
        f"Status: {PENDING_STATUS}",
        "",
        "This file is generated by `python3 evals/repair_benchmark/publish.py`.",
        "The checked-in benchmark publication surface is ready, but the repository does not yet include a committed secret-backed Tier 1 Rust beta benchmark run with real metrics.",
        "TypeScript remains an experimental frozen lane until the Rust beta lane is producing verified wins on the constrained corpus.",
        "",
        "## Publish Command",
        "",
        "```bash",
        "python3 evals/repair_benchmark/publish.py \\",
        "  --quality-report evals/repair_benchmark/reports/quality/run-<id>/report.json \\",
        "  --agent-sweep-report evals/repair_benchmark/reports/agent-sweep/run-<id>/report.json \\",
        "  --output-dir docs/benchmarks",
        "```",
        "",
        "## Publication Inputs",
        "",
        f"- Quality aggregate target: `{QUALITY_REPORT_NAME}`",
        f"- Agent-sweep aggregate target: `{AGENT_SWEEP_REPORT_NAME}`",
        f"- Aggregate schema: `{BENCHMARK_SCHEMA_VERSION}`",
        "",
        "## Methodology Contract",
        "",
        "- Suite: `evals/v0/tier1-manifest.json`",
        "- Runner: `python3 evals/repair_benchmark/run.py`",
        "- Publication step: `python3 evals/repair_benchmark/publish.py`",
        "- Quality mode: full Tier 1 Rust beta lane at a fixed `--max-agents` setting",
        "- Agent-sweep mode: deterministic representative Tier 1 Rust subset at `--max-agents 1,2,4,8`",
        "- Acceptance rule: non-empty accepted patch plus an independent verifier rerun against the sandboxed `final-bundle`",
        "- False-green rule: Mercury marked the run verified, but the independent rerun failed or timed out",
        "",
        "## Required Published Metrics",
        "",
        "- attempted cases",
        "- verified repair rate",
        "- accepted patch rate",
        "- false-green rate",
        "- repair outcome distribution",
        "- difficulty-class breakdown",
        "- tier breakdown",
        "- verifier-class breakdown",
        "- candidate-lineage breakdown",
        "- candidate-lineage attempt and accepted-step totals",
        "- failure attribution",
        "- execution diagnostics for generation, safety, candidate verification, and final bundle failures",
        "- median time to first candidate",
        "- median time to verified repair",
        "- median and mean cost per attempted case",
        "- median and mean cost per verified repair",
        "- `--max-agents` speedup curve",
        "- `--max-agents` cost curve",
        "",
        "## Notes",
        "",
        "- The workflow at `.github/workflows/repair-benchmark.yml` already emits `report.json` artifacts and now renders this public surface from them.",
        "- The public Tier 1 report should explain misses, not just summarize outcomes, so execution diagnostics are part of the checked-in contract.",
        "- Do not treat this file as published proof until the raw reports above are checked in alongside real benchmark numbers.",
        "",
    ]
    return "\n".join(lines)


def render_published_markdown(quality: dict[str, Any], agent_sweep: dict[str, Any]) -> str:
    quality_metrics = quality["metrics"]
    agent_metrics = agent_sweep["metrics"]
    lines = [
        f"# {TRACK_TITLE}",
        "",
        "Status: published from benchmark runner artifacts.",
        "",
        "This file is generated by `python3 evals/repair_benchmark/publish.py` from raw `report.json` bundles emitted by `python3 evals/repair_benchmark/run.py`.",
        "These numbers apply only to the Tier 1 Rust beta lane and the exact run ids listed below.",
        "",
        "## Publication Inputs",
        "",
        f"- Quality report: `{QUALITY_REPORT_NAME}`",
        f"  Run id: `{quality['run_id']}` | Generated at: `{quality['generated_at']}` | Agent counts: `{', '.join(str(value) for value in quality['agent_counts'])}`",
        f"- Agent-sweep report: `{AGENT_SWEEP_REPORT_NAME}`",
        f"  Run id: `{agent_sweep['run_id']}` | Generated at: `{agent_sweep['generated_at']}` | Agent counts: `{', '.join(str(value) for value in agent_sweep['agent_counts'])}`",
        f"- Aggregate schema: `{BENCHMARK_SCHEMA_VERSION}`",
        "",
        "## Quality Metrics",
        "",
        f"- Attempted cases: `{quality_metrics['attempted_cases']}`",
        f"- Verified repairs: `{quality_metrics['verified_repairs']}`",
        f"- Accepted patches: `{quality_metrics['accepted_patches']}`",
        f"- Verified repair rate: `{format_metric(quality_metrics['verified_repair_rate'])}`",
        f"- Accepted patch rate: `{format_metric(quality_metrics['accepted_patch_rate'])}`",
        f"- False-green rate: `{format_metric(quality_metrics['false_green_rate'])}`",
        f"- Median time to first candidate (ms): `{format_metric(quality_metrics['median_time_to_first_candidate_ms'])}`",
        f"- Median time to verified repair (ms): `{format_metric(quality_metrics['median_time_to_verified_repair_ms'])}`",
        f"- Median cost per attempted case (USD): `{format_metric(quality_metrics['median_cost_per_attempted_case_usd'])}`",
        f"- Mean cost per attempted case (USD): `{format_metric(quality_metrics['mean_cost_per_attempted_case_usd'])}`",
        f"- Median cost per verified repair (USD): `{format_metric(quality_metrics['median_cost_per_verified_repair_usd'])}`",
        f"- Mean cost per verified repair (USD): `{format_metric(quality_metrics['mean_cost_per_verified_repair_usd'])}`",
        "",
        "## Repair Outcome Distribution",
        "",
    ]
    lines.extend(render_outcome_table(quality, agent_sweep))
    lines.extend(["", "## Difficulty-Class Breakdown", ""])
    lines.extend(
        render_breakdown_table(
            "Quality report",
            quality,
            "difficulty_breakdown",
            DIFFICULTY_ORDER,
            "difficulty",
        )
    )
    lines.extend([""])
    lines.extend(
        render_breakdown_table(
            "Agent-sweep report",
            agent_sweep,
            "difficulty_breakdown",
            DIFFICULTY_ORDER,
            "difficulty",
        )
    )
    lines.extend(["", "## Tier Breakdown", ""])
    lines.extend(
        render_breakdown_table("Quality report", quality, "tier_breakdown", TIER_ORDER, "tier")
    )
    lines.extend([""])
    lines.extend(
        render_breakdown_table(
            "Agent-sweep report",
            agent_sweep,
            "tier_breakdown",
            TIER_ORDER,
            "tier",
        )
    )
    lines.extend(["", "## Verifier-Class Breakdown", ""])
    lines.extend(
        render_breakdown_table(
            "Quality report",
            quality,
            "verifier_class_breakdown",
            VERIFIER_CLASS_ORDER,
            "verifier class",
        )
    )
    lines.extend([""])
    lines.extend(
        render_breakdown_table(
            "Agent-sweep report",
            agent_sweep,
            "verifier_class_breakdown",
            VERIFIER_CLASS_ORDER,
            "verifier class",
        )
    )
    lines.extend(["", "## Candidate Lineage Breakdown", ""])
    lines.extend(
        render_breakdown_table(
            "Quality report",
            quality,
            "candidate_lineage_breakdown",
            CANDIDATE_LINEAGE_ORDER,
            "candidate lineage",
        )
    )
    lines.extend([""])
    lines.extend(
        render_breakdown_table(
            "Agent-sweep report",
            agent_sweep,
            "candidate_lineage_breakdown",
            CANDIDATE_LINEAGE_ORDER,
            "candidate lineage",
        )
    )
    lines.extend(["", "## Candidate Lineage Attempts", ""])
    lines.extend(render_candidate_attempt_table(quality, agent_sweep))
    lines.extend(["", "## Failure Attribution", ""])
    lines.extend(render_failure_attribution_table(quality, agent_sweep))
    lines.extend(["", "## Execution Diagnostics", ""])
    lines.extend(render_execution_diagnostics_table(quality, agent_sweep))
    lines.extend(["", "## `--max-agents` Speedup Curve", ""])
    lines.extend(render_curve_table(agent_sweep["speedup_curve"], cost=False))
    lines.extend(["", "## `--max-agents` Cost Curve", ""])
    lines.extend(render_curve_table(agent_sweep["cost_curve"], cost=True))
    lines.extend(["", "## Corpus Selection", ""])
    lines.extend(selection_summary(quality))
    lines.extend(
        [
            f"- Agent-sweep representative cases: `{agent_sweep['selection']['selected_count']}`",
            "",
            "## False-Green Policy",
            "",
            "- A run counts as false green only when Mercury reported a verified final bundle but the independent verifier rerun against the sandboxed `final-bundle` failed or timed out.",
            f"- Published false-greens in the quality report: `{quality_metrics['false_greens']}`",
            f"- Published false-greens in the agent-sweep report: `{agent_metrics['false_greens']}`",
            "",
            "## Caveats",
            "",
            "- This is a Rust-first Tier 1 beta benchmark publication surface based on `evals/v0/tier1-manifest.json`; it does not claim broader Rust coverage or TypeScript repair-quality parity.",
            "- The TypeScript lane is intentionally frozen as experimental until the constrained Rust beta lane is producing verified repairs.",
            "- `--max-agents` curves apply only to the representative subset and exact verifier mix captured in the published agent-sweep report.",
            "- The raw JSON reports above are the machine-readable source of truth for downstream analysis.",
            "",
        ]
    )
    return "\n".join(lines)


def remove_output(path: Path) -> None:
    if not path.exists():
        return
    if path.is_dir():
        shutil.rmtree(path)
        return
    path.unlink()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Render the public Tier 1 Rust repair benchmark surface from aggregate benchmark reports."
    )
    parser.add_argument("--quality-report", type=Path, help="Path to a quality-mode report.json bundle.")
    parser.add_argument(
        "--agent-sweep-report",
        type=Path,
        help="Path to an agent-sweep-mode report.json bundle.",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=DEFAULT_OUTPUT_DIR,
        help="Directory where the public benchmark surface should be written.",
    )
    parser.add_argument(
        "--pending",
        action="store_true",
        help="Write the checked-in pending placeholder instead of a populated publication.",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.pending:
        if args.quality_report or args.agent_sweep_report:
            raise SystemExit("--pending cannot be combined with report inputs")
        remove_output(args.output_dir / QUALITY_REPORT_NAME)
        remove_output(args.output_dir / AGENT_SWEEP_REPORT_NAME)
        write_text(args.output_dir / TRACK_MARKDOWN, render_pending_markdown())
        return 0

    if not args.quality_report or not args.agent_sweep_report:
        raise SystemExit("provide both --quality-report and --agent-sweep-report, or use --pending")

    quality = validate_report(args.quality_report.resolve(), "quality")
    agent_sweep = validate_report(args.agent_sweep_report.resolve(), "agent-sweep")
    assert_same_track(quality, agent_sweep)
    public_quality = sanitize_public_report(quality)
    public_agent_sweep = sanitize_public_report(agent_sweep)

    args.output_dir.mkdir(parents=True, exist_ok=True)
    write_json(args.output_dir / QUALITY_REPORT_NAME, public_quality)
    write_json(args.output_dir / AGENT_SWEEP_REPORT_NAME, public_agent_sweep)
    write_text(
        args.output_dir / TRACK_MARKDOWN,
        render_published_markdown(public_quality, public_agent_sweep),
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except ValueError as exc:
        print(str(exc), file=sys.stderr)
        raise SystemExit(2)
