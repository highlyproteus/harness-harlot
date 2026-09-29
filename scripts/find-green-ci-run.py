#!/usr/bin/env python3
"""Find a successful workflow run that already tested exactly this commit's tree.

A run counts only when all of these hold:
- it belongs to this repository's own workflow file (no fork runs, no other workflows);
- it ran on a push of this exact commit to main, or on a same-repository pull
  request that was merged into this commit and whose head has the identical tree;
- every required job exists under its exact name and concluded "success"
  (a skipped job never counts).

Pull request runs in this repository check out the PR head commit (not GitHub's
synthetic merge ref), so the tested tree is the head commit's tree. Tree equality
is the new trust link: a squash merge of an up-to-date branch produces the same
tree that CI already tested, so the same checks need not run again.

Exit status: 0 and a JSON summary on stdout when a run is found; 1 when none is
found (after waiting, if --wait-minutes is set); 2 on usage or API errors.
Requires the GitHub CLI (`gh`) with GH_TOKEN, or an authenticated gh session.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time


def api(path: str) -> object:
    result = subprocess.run(
        ["gh", "api", "-H", "Accept: application/vnd.github+json", path],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(f"gh api {path} failed: {result.stderr.strip()}")
    return json.loads(result.stdout)


def tree_of(repo: str, commit: str) -> str:
    return api(f"repos/{repo}/git/commits/{commit}")["tree"]["sha"]


def candidate_heads(repo: str, commit: str, tree: str) -> list[tuple[str, str]]:
    """Return (event, head_sha) pairs whose runs may have tested `tree`."""
    heads = [("push", commit)]
    for pull in api(f"repos/{repo}/commits/{commit}/pulls?per_page=100"):
        if pull.get("merged_at") is None or pull["base"]["ref"] != "main":
            continue
        head_repo = (pull["head"].get("repo") or {}).get("full_name")
        if head_repo != repo:
            continue
        head_sha = pull["head"]["sha"]
        if head_sha == commit or tree_of(repo, head_sha) == tree:
            heads.append(("pull_request", head_sha))
    return heads


def runs_for(repo: str, workflow: str, event: str, head_sha: str) -> list[dict]:
    query = f"head_sha={head_sha}&event={event}&per_page=100"
    if event == "push":
        query += "&branch=main"
    runs = api(f"repos/{repo}/actions/workflows/{workflow}/runs?{query}")["workflow_runs"]
    return [
        run
        for run in runs
        if run["head_sha"] == head_sha
        and run["path"].split("@", 1)[0] == f".github/workflows/{workflow}"
        and run["repository"]["full_name"] == repo
        and (run.get("head_repository") or {}).get("full_name") == repo
    ]


def missing_jobs(repo: str, run: dict, required: list[str]) -> list[str]:
    jobs = api(f"repos/{repo}/actions/runs/{run['id']}/jobs?per_page=100")["jobs"]
    succeeded = {job["name"] for job in jobs if job["conclusion"] == "success"}
    return [name for name in required if name not in succeeded]


def search(repo: str, commit: str, workflow: str, required: list[str], exclude: set[int]):
    tree = tree_of(repo, commit)
    pending = []
    rejected = []
    for event, head_sha in candidate_heads(repo, commit, tree):
        for run in runs_for(repo, workflow, event, head_sha):
            if run["id"] in exclude:
                continue
            if run["status"] != "completed":
                pending.append(run["html_url"])
                continue
            if run["conclusion"] != "success":
                rejected.append(f"{run['html_url']} concluded {run['conclusion']}")
                continue
            missing = missing_jobs(repo, run, required)
            if missing:
                rejected.append(f"{run['html_url']} lacks successful jobs: {', '.join(missing)}")
                continue
            return {
                "run_id": run["id"],
                "run_url": run["html_url"],
                "event": event,
                "head_sha": head_sha,
                "commit": commit,
                "tree": tree,
                "jobs": required,
            }, pending, rejected
    return None, pending, rejected


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--workflow", required=True, help="workflow file name, e.g. ci.yml")
    parser.add_argument("--job", action="append", required=True, dest="jobs")
    parser.add_argument("--exclude-run", action="append", type=int, default=[])
    parser.add_argument("--wait-minutes", type=float, default=0)
    args = parser.parse_args()

    deadline = time.monotonic() + args.wait_minutes * 60
    try:
        while True:
            found, pending, rejected = search(
                args.repo, args.commit, args.workflow, args.jobs, set(args.exclude_run)
            )
            if found:
                print(json.dumps(found, indent=2))
                return 0
            if not pending or time.monotonic() >= deadline:
                break
            print(f"waiting for {len(pending)} run(s): {' '.join(pending)}", file=sys.stderr)
            time.sleep(30)
    except (RuntimeError, KeyError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    for reason in rejected:
        print(f"rejected: {reason}", file=sys.stderr)
    for url in pending:
        print(f"still running: {url}", file=sys.stderr)
    print(
        f"no successful {args.workflow} run tested the tree of {args.commit}",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
