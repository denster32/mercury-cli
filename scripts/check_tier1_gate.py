#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tomllib
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_QUALITY_REPORT = REPO_ROOT / "docs" / "benchmarks" / "rust-v0-quality.report.json"
DEFAULT_AGENT_SWEEP_REPORT = REPO_ROOT / "docs" / "benchmarks" / "rust-v0-agent-sweep.report.json"
EXPECTED_MANIFEST = "evals/v0/tier1-manifest.json"
EXPECTED_SCHEMA = "mercury-repair-benchmark-v1"
EXPECTED_MODES = {
    "quality": "quality",
    "agent_sweep": "agent-sweep",
}
REQUIRED_METRICS = (
    "verified_repair_rate",
    "accepted_patch_rate",
    "false_green_rate",
)
REQUIRED_BREAKDOWNS = (
    "verifier_class_breakdown",
    "candidate_lineage_breakdown",
    "failure_attribution",
    "tier_breakdown",
)
REQUIRED_VERIFIER_CLASSES = (
    "cargo_test",
    "cargo_check",
    "cargo_clippy",
)
SUPPORTED_RELEASE_MATRIX = ("macos-arm64", "linux-x86_64")
EPSILON = 1e-12


def load_json(path: Path) -> dict[str, Any]:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as exc:
        raise SystemExit(f"missing Tier 1 report: {path}") from exc
    except json.JSONDecodeError as exc:
        raise SystemExit(f"invalid JSON in Tier 1 report {path}: {exc}") from exc


def git_stdout(args: list[str]) -> str:
    try:
        return subprocess.check_output(args, cwd=REPO_ROOT, text=True, stderr=subprocess.DEVNULL)
    except subprocess.CalledProcessError as exc:
        raise SystemExit(f"git command failed: {' '.join(args)}") from exc


def resolve_current_version(explicit: str | None) -> str:
    if explicit:
        return explicit
    manifest = tomllib.loads((REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    version = manifest.get("package", {}).get("version")
    if not isinstance(version, str) or not version:
        raise SystemExit("Cargo.toml is missing package.version")
    return version


def metric_value(payload: dict[str, Any], name: str, source: str) -> float:
    metrics = payload.get("metrics")
    if not isinstance(metrics, dict):
        raise SystemExit(f"{source} is missing a metrics object")
    value = metrics.get(name)
    if not isinstance(value, (int, float)):
        raise SystemExit(f"{source} is missing numeric metrics.{name}")
    return float(value)


def validate_report_payload(
    payload: dict[str, Any], source: str, expected_mode: str
) -> dict[str, Any]:
    if payload.get("schema_version") != EXPECTED_SCHEMA:
        raise SystemExit(
            f"{source} must declare schema_version={EXPECTED_SCHEMA!r}, got {payload.get('schema_version')!r}"
        )
    if payload.get("mode") != expected_mode:
        raise SystemExit(f"{source} must declare mode={expected_mode!r}, got {payload.get('mode')!r}")

    selection = payload.get("selection")
    if not isinstance(selection, dict):
        raise SystemExit(f"{source} is missing a selection object")
    manifest_path = selection.get("manifest_path")
    if manifest_path != EXPECTED_MANIFEST:
        raise SystemExit(f"{source} must target {EXPECTED_MANIFEST!r}, got {manifest_path!r}")

    for breakdown_name in REQUIRED_BREAKDOWNS:
        breakdown = payload.get(breakdown_name)
        if not isinstance(breakdown, dict) or not breakdown:
            raise SystemExit(f"{source} is missing non-empty {breakdown_name}")

    verifier_breakdown = payload["verifier_class_breakdown"]
    missing_verifiers = [
        verifier for verifier in REQUIRED_VERIFIER_CLASSES if verifier not in verifier_breakdown
    ]
    if missing_verifiers:
        raise SystemExit(
            f"{source} is missing verifier_class_breakdown entries for {', '.join(missing_verifiers)}"
        )

    tier_breakdown = payload["tier_breakdown"]
    if "tier1" not in tier_breakdown:
        raise SystemExit(f"{source} is missing tier_breakdown.tier1")

    run_id = payload.get("run_id")
    generated_at = payload.get("generated_at")
    agent_counts = payload.get("agent_counts")
    if not isinstance(run_id, str) or not run_id:
        raise SystemExit(f"{source} is missing run_id")
    if not isinstance(generated_at, str) or not generated_at:
        raise SystemExit(f"{source} is missing generated_at")
    if not isinstance(agent_counts, list) or not agent_counts:
        raise SystemExit(f"{source} is missing agent_counts")
    if not all(isinstance(value, int) for value in agent_counts):
        raise SystemExit(f"{source} contains non-integer agent_counts")

    metrics = {
        metric_name: metric_value(payload, metric_name, source)
        for metric_name in REQUIRED_METRICS
    }
    attempted_cases = metric_value(payload, "attempted_cases", source)

    return {
        "mode": expected_mode,
        "run_id": run_id,
        "generated_at": generated_at,
        "agent_counts": agent_counts,
        "metrics": metrics,
        "attempted_cases": int(attempted_cases),
        "verifier_classes": sorted(verifier_breakdown.keys()),
    }


def find_prior_baseline(
    current_version: str, report_relpath: Path, expected_mode: str
) -> tuple[str, dict[str, Any]] | None:
    current_tag = f"v{current_version}"
    tags = [
        line.strip()
        for line in git_stdout(["git", "tag", "--sort=-version:refname", "--list", "v*"]).splitlines()
        if line.strip()
    ]
    for tag in tags:
        if tag == current_tag:
            continue
        try:
            raw = subprocess.check_output(
                ["git", "show", f"{tag}:{report_relpath.as_posix()}"],
                cwd=REPO_ROOT,
                text=True,
                stderr=subprocess.DEVNULL,
            )
        except subprocess.CalledProcessError:
            continue
        try:
            payload = json.loads(raw)
        except json.JSONDecodeError:
            continue
        try:
            summary = validate_report_payload(payload, f"{tag}:{report_relpath.as_posix()}", expected_mode)
        except SystemExit:
            continue
        return tag, summary
    return None


def compare_against_baseline(
    current_metrics: dict[str, float], baseline_tag: str, baseline_metrics: dict[str, float]
) -> None:
    failures: list[str] = []
    if current_metrics["verified_repair_rate"] + EPSILON < baseline_metrics["verified_repair_rate"]:
        failures.append(
            "verified_repair_rate regressed "
            f"({current_metrics['verified_repair_rate']:.6f} < {baseline_metrics['verified_repair_rate']:.6f} from {baseline_tag})"
        )
    if current_metrics["accepted_patch_rate"] + EPSILON < baseline_metrics["accepted_patch_rate"]:
        failures.append(
            "accepted_patch_rate regressed "
            f"({current_metrics['accepted_patch_rate']:.6f} < {baseline_metrics['accepted_patch_rate']:.6f} from {baseline_tag})"
        )
    if current_metrics["false_green_rate"] > baseline_metrics["false_green_rate"] + EPSILON:
        failures.append(
            "false_green_rate increased "
            f"({current_metrics['false_green_rate']:.6f} > {baseline_metrics['false_green_rate']:.6f} from {baseline_tag})"
        )
    if failures:
        raise SystemExit("Tier 1 release gate failed:\n- " + "\n- ".join(failures))


def format_rate(value: float) -> str:
    return f"{value * 100:.2f}%"


def format_pp_delta(current: float, baseline: float) -> str:
    delta = (current - baseline) * 100.0
    return f"{delta:+.2f} pp"


def display_path(path: Path) -> str:
    try:
        return path.relative_to(REPO_ROOT).as_posix()
    except ValueError:
        return str(path)


def render_release_notes(
    current_version: str,
    quality_summary: dict[str, Any],
    agent_sweep_summary: dict[str, Any],
    baseline: tuple[str, dict[str, Any]] | None,
) -> str:
    quality_metrics = quality_summary["metrics"]
    agent_metrics = agent_sweep_summary["metrics"]
    verifier_classes = ", ".join(
        verifier.replace("cargo_", "cargo ")
        for verifier in REQUIRED_VERIFIER_CLASSES
    )

    lines = [
        f"# Mercury CLI {current_version}",
        "",
        "## Release Contract",
        "",
        f"- Tier 1 public beta truth: `{DEFAULT_QUALITY_REPORT.relative_to(REPO_ROOT).as_posix()}`",
        f"- Quality run: `{quality_summary['run_id']}` at `{quality_summary['generated_at']}`",
        f"- Agent-sweep run: `{agent_sweep_summary['run_id']}` at `{agent_sweep_summary['generated_at']}`",
        f"- Tier 1 manifest: `{EXPECTED_MANIFEST}`",
        f"- Supported Rust verifier classes: `{verifier_classes}`",
        f"- Supported binary matrix: `{', '.join(SUPPORTED_RELEASE_MATRIX)}`",
        "",
        "## Tier 1 Quality Metrics",
        "",
        f"- Attempted cases: `{quality_summary['attempted_cases']}`",
        f"- Verified repair rate: `{format_rate(quality_metrics['verified_repair_rate'])}`",
        f"- Accepted patch rate: `{format_rate(quality_metrics['accepted_patch_rate'])}`",
        f"- False-green rate: `{format_rate(quality_metrics['false_green_rate'])}`",
        f"- Agent counts: `{', '.join(str(value) for value in quality_summary['agent_counts'])}`",
        "",
        "## Agent-Sweep Diagnostics",
        "",
        f"- Attempted cases: `{agent_sweep_summary['attempted_cases']}`",
        f"- Verified repair rate: `{format_rate(agent_metrics['verified_repair_rate'])}`",
        f"- Accepted patch rate: `{format_rate(agent_metrics['accepted_patch_rate'])}`",
        f"- False-green rate: `{format_rate(agent_metrics['false_green_rate'])}`",
        f"- Agent counts: `{', '.join(str(value) for value in agent_sweep_summary['agent_counts'])}`",
        "",
        "## Benchmark Delta",
        "",
    ]

    if baseline is None:
        lines.append(
            f"- No prior tagged Tier 1 quality report was found. `{current_version}` establishes the regression anchor."
        )
    else:
        baseline_tag, baseline_summary = baseline
        baseline_metrics = baseline_summary["metrics"]
        lines.extend(
            [
                f"- Compared against `{baseline_tag}`.",
                f"- Verified repair rate delta: `{format_pp_delta(quality_metrics['verified_repair_rate'], baseline_metrics['verified_repair_rate'])}`",
                f"- Accepted patch rate delta: `{format_pp_delta(quality_metrics['accepted_patch_rate'], baseline_metrics['accepted_patch_rate'])}`",
                f"- False-green rate delta: `{format_pp_delta(quality_metrics['false_green_rate'], baseline_metrics['false_green_rate'])}`",
            ]
        )
    lines.extend(
        [
            "",
            "## Packaging Notes",
            "",
            "- Tagged prereleases ship only when the checked-in Tier 1 quality report holds steady or improves.",
            "- Tier 0 and Tier 2 remain internal diagnostics; the public beta truth is Tier 1 Rust direct-verifier repair.",
        ]
    )
    return "\n".join(lines) + "\n"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Validate the checked-in Tier 1 Rust benchmark reports and block prerelease publication "
            "when the checked-in quality metrics regress against the most recent tagged Tier 1 baseline."
        )
    )
    parser.add_argument("--current-version", help="Version being released. Defaults to Cargo.toml package.version.")
    parser.add_argument(
        "--quality-report",
        default=str(DEFAULT_QUALITY_REPORT),
        help="Path to the checked-in current Tier 1 quality report.",
    )
    parser.add_argument(
        "--agent-sweep-report",
        default=str(DEFAULT_AGENT_SWEEP_REPORT),
        help="Path to the checked-in current Tier 1 agent-sweep report.",
    )
    parser.add_argument(
        "--release-notes-out",
        help="Optional output path for benchmark-tied release notes.",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    current_version = resolve_current_version(args.current_version)

    quality_report_path = Path(args.quality_report)
    if not quality_report_path.is_absolute():
        quality_report_path = (REPO_ROOT / quality_report_path).resolve()
    quality_report_relpath = quality_report_path.relative_to(REPO_ROOT)

    agent_sweep_report_path = Path(args.agent_sweep_report)
    if not agent_sweep_report_path.is_absolute():
        agent_sweep_report_path = (REPO_ROOT / agent_sweep_report_path).resolve()
    agent_sweep_report_relpath = agent_sweep_report_path.relative_to(REPO_ROOT)

    quality_summary = validate_report_payload(
        load_json(quality_report_path),
        quality_report_relpath.as_posix(),
        EXPECTED_MODES["quality"],
    )
    agent_sweep_summary = validate_report_payload(
        load_json(agent_sweep_report_path),
        agent_sweep_report_relpath.as_posix(),
        EXPECTED_MODES["agent_sweep"],
    )

    print(
        "Validated checked-in Tier 1 reports "
        f"{quality_report_relpath.as_posix()} and {agent_sweep_report_relpath.as_posix()} "
        f"(quality verified_repair_rate={quality_summary['metrics']['verified_repair_rate']:.6f}, "
        f"accepted_patch_rate={quality_summary['metrics']['accepted_patch_rate']:.6f}, "
        f"false_green_rate={quality_summary['metrics']['false_green_rate']:.6f})."
    )

    baseline = None
    if "-" not in current_version:
        print(f"{current_version} is not a prerelease; Tier 1 gate enforced report presence only.")
    else:
        baseline = find_prior_baseline(
            current_version,
            quality_report_relpath,
            EXPECTED_MODES["quality"],
        )
        if baseline is None:
            print(
                "No prior tagged release with a checked-in Tier 1 quality report was found; "
                f"prerelease {current_version} passes without regression comparison."
            )
        else:
            baseline_tag, baseline_summary = baseline
            compare_against_baseline(quality_summary["metrics"], baseline_tag, baseline_summary["metrics"])
            print(
                f"Tier 1 gate passed against {baseline_tag} "
                f"(verified_repair_rate={baseline_summary['metrics']['verified_repair_rate']:.6f}, "
                f"accepted_patch_rate={baseline_summary['metrics']['accepted_patch_rate']:.6f}, "
                f"false_green_rate={baseline_summary['metrics']['false_green_rate']:.6f})."
            )

    if args.release_notes_out:
        release_notes_path = Path(args.release_notes_out)
        if not release_notes_path.is_absolute():
            release_notes_path = (REPO_ROOT / release_notes_path).resolve()
        release_notes_path.write_text(
            render_release_notes(current_version, quality_summary, agent_sweep_summary, baseline),
            encoding="utf-8",
        )
        print(f"Wrote benchmark-tied release notes to {display_path(release_notes_path)}.")

    return 0


if __name__ == "__main__":
    sys.exit(main())
