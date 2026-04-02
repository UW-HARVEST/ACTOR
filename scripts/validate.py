#!/usr/bin/env python3
"""Validate kiro-cli translations against checked-in expected results.

Runs MIT's runtests on each battery and compares results against stored
summary.json files. Any deviation (regression or improvement) fails CI —
to update expected results, run with --update and commit the changes.

Usage:
    ./scripts/validate.py                       # check all batteries
    ./scripts/validate.py B01_synthetic         # check one battery
    ./scripts/validate.py --update              # run tests and update expected results
    ./scripts/validate.py --update B01_organic  # update one battery

A "battery" is a test suite directory name under results/ (e.g. B01_synthetic,
B02_organic). Each battery contains case directories with translated_rust/ and
test_vectors/ subdirectories. Available batteries match those in
test-corpus/Public-Tests/.

summary.json schema (per-battery and overall):
    {
      "cases_tested": int,      # cases with test vectors that were run
      "cases_passed": int,      # cases where all non-skipped vectors passed
      "vectors_passed": int,    # individual test vectors that passed
      "vectors_failed": int,    # individual test vectors that failed
      "vectors_skipped": int,   # test vectors skipped (e.g. has_ub)
      "failed_cases": [str]     # names of cases with any failing vectors
    }
"""
import subprocess, json, re, sys, os, glob
from typing import Optional

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RESULTS_DIR = os.path.join(REPO_ROOT, "results", "kiro")
CORPUS_DIR = os.path.join(REPO_ROOT, "test-corpus")


def discover_batteries() -> list[str]:
    """Return sorted list of battery names that have translated results."""
    return sorted(
        d for d in os.listdir(RESULTS_DIR)
        if os.path.isdir(os.path.join(RESULTS_DIR, d))
        and glob.glob(os.path.join(RESULTS_DIR, d, "*/translated_rust"))
    )


def run_battery(battery: str) -> tuple[dict, dict[str, dict]]:
    """Run MIT runtests on a battery.

    Returns:
        (summary, per_case) where summary matches the summary.json schema
        and per_case maps case name -> {"case", "battery", "vectors_failed", "passed"}.
    """
    results_dir = os.path.join(RESULTS_DIR, battery)

    # Clean build artifacts to prevent stale cached binaries
    for case in os.listdir(results_dir):
        target = os.path.join(results_dir, case, "translated_rust", "target")
        if os.path.isdir(target):
            import shutil
            shutil.rmtree(target)

    env = os.environ.copy()
    env["PYTHONPATH"] = os.path.join(CORPUS_DIR, "deployment/scripts/github-actions") + ":" + env.get("PYTHONPATH", "")

    result = subprocess.run(
        ["python3", "-m", "runtests.rust", "--root", results_dir, "--subset", results_dir, "--keep-going", "--verbose"],
        capture_output=True, text=True, cwd=CORPUS_DIR, env=env,
    )
    output = result.stdout + result.stderr
    print(output)

    def extract(pattern: str) -> int:
        m = re.search(pattern, output)
        if not m:
            print(f"   ⚠️  Could not parse test output.")
            sys.exit(1)
        return int(m.group(1))

    # Count per-case vector failures
    failed_cases: dict[str, int] = {}
    for m in re.finditer(r"^- (\S+): Test failed \((\S+):.*?\)$", output, re.MULTILINE):
        case = m.group(1)
        failed_cases[case] = failed_cases.get(case, 0) + 1

    # Build per-case results from executed cases
    per_case: dict[str, dict] = {}
    for case in re.findall(r"Executing (\S+)", output):
        vf = failed_cases.get(case, 0)
        per_case[case] = {
            "case": case,
            "battery": battery,
            "vectors_failed": vf,
            "passed": vf == 0,
        }

    summary = {
        "cases_tested": extract(r"Test Cases Tested:\s+(\d+)"),
        "cases_passed": max(0, extract(r"Test Cases Tested:\s+(\d+)") - extract(r"Test Cases Failed:\s+(\d+)")),
        "vectors_passed": extract(r"Test Vectors Passed:\s+(\d+)"),
        "vectors_failed": extract(r"Test Vectors Failed:\s+(\d+)"),
        "vectors_skipped": extract(r"Test Vectors Skipped:\s+(\d+)"),
        "failed_cases": sorted(set(re.findall(r"^- (\S+): Test failed", output, re.MULTILINE))),
    }
    return summary, per_case


def load_summary(battery: str) -> dict:
    """Load stored summary.json for a battery, or return empty expected results."""
    path = os.path.join(RESULTS_DIR, battery, "summary.json")
    if os.path.exists(path):
        with open(path) as f:
            return json.load(f)
    return {"cases_tested": 0, "cases_passed": 0, "vectors_passed": 0,
            "vectors_failed": 0, "vectors_skipped": 0, "failed_cases": []}


def check_battery(expected: dict, actual: dict) -> list[str]:
    """Compare expected vs actual battery results. Returns list of differences."""
    errors = []
    for key in ["vectors_passed", "vectors_failed", "cases_passed", "cases_tested"]:
        if actual.get(key) != expected.get(key):
            errors.append(f"{key}: {expected.get(key)} -> {actual.get(key)}")
    added = set(actual["failed_cases"]) - set(expected["failed_cases"])
    removed = set(expected["failed_cases"]) - set(actual["failed_cases"])
    if added:
        errors.append(f"new failures: {', '.join(sorted(added))}")
    if removed:
        errors.append(f"no longer failing: {', '.join(sorted(removed))}")
    return errors


def write_summaries(all_batteries: dict[str, dict]) -> None:
    """Write per-battery summary.json and overall results/summary.json."""
    for battery, data in all_batteries.items():
        path = os.path.join(RESULTS_DIR, battery, "summary.json")
        with open(path, "w") as f:
            json.dump(data, f, indent=2)
            f.write("\n")

    total = {"cases_tested": 0, "cases_passed": 0, "vectors_passed": 0,
             "vectors_failed": 0, "vectors_skipped": 0, "failed_cases": []}
    for battery, data in sorted(all_batteries.items()):
        for k in ["cases_tested", "cases_passed", "vectors_passed", "vectors_failed", "vectors_skipped"]:
            total[k] += data[k]
        total["failed_cases"].extend(f"{battery}/{c}" for c in data["failed_cases"])

    summary = {"batteries": dict(sorted(all_batteries.items())), "total": total}
    with open(os.path.join(RESULTS_DIR, "summary.json"), "w") as f:
        json.dump(summary, f, indent=2)
        f.write("\n")

    print("\n========================================")
    print("  Overall Summary")
    print("========================================")
    for b, d in sorted(all_batteries.items()):
        vt = d["vectors_passed"] + d["vectors_failed"]
        pct = f"{100 * d['vectors_passed'] / vt:.1f}%" if vt else "N/A"
        print(f"  {b}: {d['cases_passed']}/{d['cases_tested']} cases, {d['vectors_passed']}/{vt} vectors ({pct})")
    vt = total["vectors_passed"] + total["vectors_failed"]
    pct = f"{100 * total['vectors_passed'] / vt:.1f}%" if vt else "N/A"
    print(f"  ────────────────────────────────")
    print(f"  TOTAL: {total['cases_passed']}/{total['cases_tested']} cases, {total['vectors_passed']}/{vt} vectors ({pct})")
    print("========================================\n")


def main() -> None:
    global RESULTS_DIR
    update_mode = "--update" in sys.argv
    raw_args = [a for a in sys.argv[1:] if a != "--update"]

    # Parse --results-dir <path> (relative to REPO_ROOT)
    args = []
    i = 0
    while i < len(raw_args):
        if raw_args[i] == "--results-dir" and i + 1 < len(raw_args):
            RESULTS_DIR = os.path.join(REPO_ROOT, raw_args[i + 1])
            i += 2
        else:
            args.append(raw_args[i])
            i += 1

    batteries = args or discover_batteries()
    all_ok = True
    all_results: dict[str, dict] = {}

    for battery in batteries:
        battery_dir = os.path.join(RESULTS_DIR, battery)
        if not os.path.isdir(battery_dir):
            print(f"⚠️  {battery}: no results directory, skipping")
            continue

        expected = load_summary(battery)

        print(f"🔍 {battery}: running tests...", flush=True)
        actual, per_case = run_battery(battery)
        all_results[battery] = actual

        if update_mode:
            for case, data in per_case.items():
                case_dir = os.path.join(battery_dir, case)
                if os.path.isdir(case_dir):
                    with open(os.path.join(case_dir, "result.json"), "w") as f:
                        json.dump(data, f, indent=2)
                        f.write("\n")
            vt = actual["vectors_passed"] + actual["vectors_failed"]
            print(f"   📝 Updated: {actual['cases_passed']}/{actual['cases_tested']} cases, {actual['vectors_passed']}/{vt} vectors")
        else:
            evt = expected["vectors_passed"] + expected["vectors_failed"]
            avt = actual["vectors_passed"] + actual["vectors_failed"]
            print(f"   stored:  {expected['cases_passed']}/{expected['cases_tested']} cases, {expected['vectors_passed']}/{evt} vectors")
            print(f"   actual:  {actual['cases_passed']}/{actual['cases_tested']} cases, {actual['vectors_passed']}/{avt} vectors")

            errors = check_battery(expected, actual)
            if errors:
                print(f"   ❌ MISMATCH: {'; '.join(errors)}")
                if expected["cases_tested"] != actual["cases_tested"]:
                    print(f"   ℹ️  cases_tested changed: {expected['cases_tested']} -> {actual['cases_tested']} (build failures may differ across environments)")
                if expected.get("vectors_skipped", 0) != actual.get("vectors_skipped", 0):
                    print(f"   ℹ️  vectors_skipped changed: {expected.get('vectors_skipped', 0)} -> {actual.get('vectors_skipped', 0)}")
                all_ok = False
            else:
                print(f"   ✅ OK")

    write_summaries(all_results)

    if update_mode:
        print(f"✅ Summaries updated")

    if not update_mode:
        sys.exit(0 if all_ok else 1)


if __name__ == "__main__":
    main()
