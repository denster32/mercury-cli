#!/usr/bin/env python3
import argparse
import json
import shutil
import sys
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT_DIR = REPO_ROOT / "docs" / "benchmarks"
BENCHMARK_SCHEMA_VERSION = "mercury-repair-benchmark-v1"
TRACK_TITLE = "Rust V0 Repair Benchmark"
TRACK_MARKDOWN = "rust-v0-repair-benchmark.md"
QUALITY_REPORT_NAME = "rust-v0-quality.report.json"
AGENT_SWEEP_REPORT_NAME = "rust-v0-agent-sweep.report.json"
PENDING_STATUS = "pending first checked-in secret-backed run."


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
        public_result = dict(result)
        public_result.pop("benchmark_run_path", None)
        public_result.pop("candidate_workspace", None)
        sanitized_results.append(public_result)
    sanitized["results"] = sanitized_results
    sanitized["repair_outcome_distribution"] = repair_outcome_distribution(sanitized_results)

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
    return [
        f"- Manifest: `evals/v0/manifest.json`",
        f"- Selected cases: `{selection['selected_count']}`",
        f"- Unique fixture paths: `{selection['selected_unique_fixture_paths']}`",
        f"- Requested stages: `{', '.join(selection['requested_stages']) if selection['requested_stages'] else 'all selected Rust stages'}`",
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


def repair_outcome_distribution(results: list[dict[str, Any]]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for result in results:
        outcome = result.get("outcome")
        label = outcome if isinstance(outcome, str) and outcome else "unknown"
        counts[label] = counts.get(label, 0) + 1
    return dict(sorted(counts.items()))


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


def render_pending_markdown() -> str:
    lines = [
        f"# {TRACK_TITLE}",
        "",
        f"Status: {PENDING_STATUS}",
        "",
        "This file is generated by `python3 evals/repair_benchmark/publish.py`.",
        "The checked-in benchmark publication surface is ready, but the repository does not yet include a committed secret-backed Rust benchmark run with real metrics.",
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
        "- Suite: `evals/v0/manifest.json`",
        "- Runner: `python3 evals/repair_benchmark/run.py`",
        "- Publication step: `python3 evals/repair_benchmark/publish.py`",
        "- Quality mode: full selected Rust suite at a fixed `--max-agents` setting",
        "- Agent-sweep mode: deterministic representative Rust subset at `--max-agents 1,2,4,8`",
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
        "These numbers apply only to the selected Rust corpus and the exact run ids listed below.",
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
    lines.extend(
        [
            "",
        "## `--max-agents` Speedup Curve",
        "",
        ]
    )
    lines.extend(render_curve_table(agent_sweep["speedup_curve"], cost=False))
    lines.extend(
        [
            "",
            "## `--max-agents` Cost Curve",
            "",
        ]
    )
    lines.extend(render_curve_table(agent_sweep["cost_curve"], cost=True))
    lines.extend(
        [
            "",
            "## Corpus Selection",
            "",
        ]
    )
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
            "- This is a Rust-first benchmark publication surface based on `evals/v0`; it does not claim TypeScript repair-quality parity.",
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
        description="Render the public Rust repair benchmark surface from aggregate benchmark reports."
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
