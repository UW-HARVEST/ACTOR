#!/usr/bin/env python3
"""Refuse a workflow file GitHub will reject.

A workflow GitHub cannot parse fails in ZERO seconds, with no job, no step and no log -- which
looks exactly like a pre-existing red X. type-safety.yaml was unparseable for 8 commits that way,
so none of its gates ran: not fmt, not clippy, not the unit tests, not the architecture rules, not
compile_fail, not check_paths, and not the comment budget that three of those commits were pruned
to satisfy. Nothing said so; the run just took no time.

`yaml.safe_load` is NOT a guard against it. Given two identical keys it silently keeps the LAST and
reports success, while GitHub refuses the document -- the duplicate `run:` that caused the outage
parsed clean locally and was pushed on that evidence. So this refuses duplicate keys outright, and
checks the shape GitHub requires beyond parsing: a job with steps, and a step that does exactly one
of `run` or `uses`.

Every workflow is checked by EVERY workflow that invokes this. That mutual arrangement is the point:
a workflow cannot police its own syntax, because when it is broken it does not run at all. Its
sibling is what catches it.
"""

import glob
import sys

import yaml


class Loader(yaml.SafeLoader):
    """A loader that refuses what `safe_load` tolerates."""


def no_duplicate_keys(loader, node, deep=False):
    seen = set()
    for key_node, _ in node.value:
        key = loader.construct_object(key_node, deep=deep)
        if key in seen:
            raise yaml.YAMLError(
                f"line {key_node.start_mark.line + 1}: the key {key!r} appears twice in one "
                f"mapping. GitHub rejects the whole file; PyYAML keeps only the last one."
            )
        seen.add(key)
    return yaml.SafeLoader.construct_mapping(loader, node, deep)


Loader.add_constructor(yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG, no_duplicate_keys)


def problems(path, doc):
    if not isinstance(doc, dict):
        return [f"{path}: not a mapping"]
    found = []
    # `on` parses as the boolean True in YAML 1.1, which is why this looks for either spelling.
    if "on" not in doc and True not in doc:
        found.append(f"{path}: no trigger, so it can never run")
    jobs = doc.get("jobs") or {}
    if not jobs:
        found.append(f"{path}: no jobs")
    for name, job in jobs.items():
        steps = job.get("steps") if isinstance(job, dict) else None
        if not steps:
            if isinstance(job, dict) and job.get("uses"):
                continue
            found.append(f"{path}: job {name!r} has no steps")
            continue
        for i, step in enumerate(steps):
            does = [k for k in ("run", "uses") if k in step]
            if len(does) != 1:
                label = step.get("name", f"#{i + 1}")
                found.append(
                    f"{path}: job {name!r} step {label!r} has {does or 'neither'} -- a step "
                    f"does exactly one of `run` or `uses`"
                )
    return found


def main():
    paths = sorted(
        set(glob.glob(".github/workflows/*.yml")) | set(glob.glob(".github/workflows/*.yaml"))
    )
    if not paths:
        sys.exit("check_workflows: found no workflow to check, so this proves nothing")
    failures, steps = [], 0
    for path in paths:
        try:
            with open(path) as f:
                doc = yaml.load(f, Loader)
        except yaml.YAMLError as e:
            failures.append(f"{path}: {e}")
            continue
        failures += problems(path, doc)
        if isinstance(doc, dict):
            steps += sum(
                len(j.get("steps") or [])
                for j in (doc.get("jobs") or {}).values()
                if isinstance(j, dict)
            )
    if not steps:
        sys.exit("check_workflows: inspected no steps at all, so this proves nothing")
    if failures:
        print("\n".join(f"  ❌ {f}" for f in failures))
        sys.exit(f"check_workflows: {len(failures)} workflow problem(s)")
    print(f"check_workflows: {len(paths)} workflow(s), {steps} steps, no duplicate keys")


if __name__ == "__main__":
    main()
