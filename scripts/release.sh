#!/bin/sh
# One command from "release branch ready" to "published", resumable per stage.
#
#   scripts/release.sh X.Y.Z --title "..." [--upgrade-smoke-passed]
#       prepare + watch-pr + publish + watch-release + land
#   scripts/release.sh prepare X.Y.Z --title "..." [--upgrade-smoke-passed] [--dry-run [--skip-preflight]]
#   scripts/release.sh watch-pr N
#   scripts/release.sh publish X.Y.Z
#   scripts/release.sh watch-release vX.Y.Z
#   scripts/release.sh land vX.Y.Z
#
# Never prompts: starting a release is the approval to publish once CI is green.
# All GitHub calls use the highlyproteus account token (resolved once, never printed).
#
# prepare   Must be on release/vX.Y.Z (or creates it from HEAD). Refuses if the tag
#           exists locally/on github/origin, if the tree is dirty (a tree that only
#           holds this release's own bump edits is accepted, so a failed prepare can
#           be re-run), if CHANGELOG [Unreleased] is empty, or if the diff since the
#           previous tag touches crates/updater, crates/protocol or
#           crates/session-service without --upgrade-smoke-passed (see
#           scripts/build-upgrade-fixtures.sh). Then: bumps Cargo.toml, Cargo.lock
#           (hh-* entries, offline) and THIRD_PARTY_NOTICES.md, dates the CHANGELOG
#           section, runs scripts/preflight.sh, commits "Release X.Y.Z: <title>",
#           pushes the branch to github and creates the PR (or reuses/updates the open
#           one) with the changelog section as body.
#           --dry-run: performs every check and runs preflight on the UNEDITED tree,
#           prints the planned edits instead of applying them; nothing is written,
#           committed, pushed or created, and failed checks are reported ("would
#           refuse") instead of aborting. --skip-preflight (dry-run only) skips preflight.
# watch-pr  Polls Actions runs for the PR head sha every 20 s. CI and Security are
#           required; Packaging Assurance is required once it has triggered (it is
#           path filtered; a release bumps Cargo.toml so it normally does). Prints
#           each job as it completes; exits 1 at the first failed job.
# publish   Requires the PR green (one check; run watch-pr first), squash-merges with
#           --match-head-commit, fetches github main, reports whether the merge tree
#           equals the PR head tree, creates the signed tag "Harness Harlot X.Y.Z" at the
#           merge commit, pushes the tag to github and origin, and pushes the merge
#           commit to origin main (fast-forward only). Idempotent for resume.
# watch-release  Finds the Release run for the tag, prints jobs as they complete
#           (latest attempt only), exits 1 at the first failed job, and on success
#           confirms the GitHub release is published.
# land      Users only get a release once the stable-v2 aliases are re-signed and the
#           website is synced (otherwise up to ~24 h delay). Dispatches
#           refresh-stable-v2.yml here, then sync-release.yml in
#           highlyproteus/harness-harlot-landing, waits for each run (first failed job
#           reported immediately), then polls harnessharlot.com/releases/stable-macos.json
#           and stable-linux.json until their "version" is X.Y.Z (15 min timeout).
set -eu

REPO=highlyproteus/harness-harlot
GITHUB_REMOTE=github
MIRROR_REMOTE=origin
POLL_SECONDS=${HH_RELEASE_POLL_SECONDS:-20}

script_path=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/$(basename -- "$0")
repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repository_root"

usage() {
  sed -n '2,/^set -eu/p' "$script_path" | sed '$d' | sed 's/^# \{0,1\}//' >&2
  exit "${1:-2}"
}
die() { echo "error: $*" >&2; exit 1; }
note() { echo "==> $*"; }

init_gh() {
  command -v gh >/dev/null 2>&1 || die "gh not installed"
  GH_TOKEN=$(gh auth token --user highlyproteus 2>/dev/null) || die "no gh login for highlyproteus"
  export GH_TOKEN
}

check_version() {
  case "$1" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) die "version must look like X.Y.Z (got '$1')" ;;
  esac
  case "$1" in
    *[!0-9.]*) die "version must be numeric X.Y.Z (got '$1')" ;;
  esac
}

# ---------------------------------------------------------------- prepare

# problem MESSAGE: fatal, except under --dry-run where it is reported and we continue.
problem() {
  if [ "$dry_run" -eq 1 ]; then
    echo "DRY-RUN would refuse: $*" >&2
    dry_problems=$((dry_problems + 1))
  else
    die "$*"
  fi
}

workspace_version() {
  python3 - <<'PY'
import re
text = open("Cargo.toml").read()
section = re.search(r'(?ms)^\[workspace\.package\]\n(.*?)(?=^\[|\Z)', text)
print(re.search(r'(?m)^version = "([^"]+)"', section.group(1)).group(1))
PY
}

# changelog_state VERSION -> prints: empty | dated | pending
changelog_state() {
  CL_VERSION=$1 python3 - <<'PY'
import os, re
version = os.environ["CL_VERSION"]
text = open("CHANGELOG.md").read()
if re.search(r'(?m)^## \[%s\] - ' % re.escape(version), text):
    print("dated")
else:
    m = re.search(r'(?ms)^## \[Unreleased\]\n(.*?)(?=^## \[|\Z)', text)
    print("pending" if m and m.group(1).strip() else "empty")
PY
}

# changelog_section VERSION [DATE] -> body of the (would-be) release section; with a
# date it also edits CHANGELOG.md in place unless CL_WRITE=0.
changelog_apply() {
  CL_VERSION=$1 CL_DATE=$2 CL_WRITE=$3 python3 - <<'PY'
import os, re
version, date, write = os.environ["CL_VERSION"], os.environ["CL_DATE"], os.environ["CL_WRITE"] == "1"
text = open("CHANGELOG.md").read()
dated = re.search(r'(?ms)^## \[%s\] - [^\n]*\n(.*?)(?=^## \[|\Z)' % re.escape(version), text)
if dated:
    print(dated.group(1).strip())
else:
    m = re.search(r'(?ms)^## \[Unreleased\]\n(.*?)(?=^## \[|\Z)', text)
    body = m.group(1).strip()
    if write:
        head = text[: m.start(1)]
        rest = text[m.end(1):]
        text = "%s\n## [%s] - %s\n\n%s\n\n%s" % (head, version, date, body, rest)
        open("CHANGELOG.md", "w").write(text)
    print(body)
PY
}

apply_bump() {
  BUMP_FROM=$1 BUMP_TO=$2 python3 - <<'PY'
import os, re
old, new = os.environ["BUMP_FROM"], os.environ["BUMP_TO"]

text = open("Cargo.toml").read()
def fix(m):
    return m.group(1) + re.sub(r'(?m)^version = "%s"$' % re.escape(old), 'version = "%s"' % new, m.group(2), count=1)
text, n = re.subn(r'(?ms)(^\[workspace\.package\]\n)(.*?)(?=^\[|\Z)', fix, text, count=1)
assert n == 1 and 'version = "%s"' % new in text, "workspace version not updated in Cargo.toml"
open("Cargo.toml", "w").write(text)

lock = open("Cargo.lock").read()
lock, n = re.subn(r'(name = "hh-[a-z0-9-]+"\nversion = ")%s(")' % re.escape(old), r'\g<1>%s\g<2>' % new, lock)
assert n == 10, "expected 10 hh-* packages in Cargo.lock, updated %d" % n
open("Cargo.lock", "w").write(lock)

notices = open("THIRD_PARTY_NOTICES.md").read()
notices, n = re.subn(r'(?m)^(- \[hh-([a-z0-9-]+)) %s(\]\(https://crates\.io/crates/hh-\2\) — MIT)$' % re.escape(old), r'\1 %s\3' % new, notices)
assert n == 10, "expected 10 hh-* lines in THIRD_PARTY_NOTICES.md, updated %d" % n
open("THIRD_PARTY_NOTICES.md", "w").write(notices)
PY
}

previous_tag_for() {
  git tag --list 'v[0-9]*' --sort=-v:refname | grep -vx "v$1" |
    python3 -c '
import sys
target = tuple(int(p) for p in sys.argv[1].split("."))
for line in sys.stdin:
    tag = line.strip()
    try:
        v = tuple(int(p) for p in tag[1:].split("."))
    except ValueError:
        continue
    if v < target:
        print(tag)
        break
' "$1"
}

find_pr() { # BRANCH STATE -> "number<TAB>state<TAB>headSha" of newest PR for the head branch, or empty
  gh pr list --repo "$REPO" --head "$1" --state "$2" --limit 1 --json number,state,headRefOid \
    --jq '.[0] | select(.) | "\(.number)\t\(.state)\t\(.headRefOid)"'
}

cmd_prepare() {
  version=
  title=
  smoke=0
  dry_run=0
  skip_preflight=0
  dry_problems=0
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --title) [ "$#" -ge 2 ] || usage; title=$2; shift 2 ;;
      --upgrade-smoke-passed) smoke=1; shift ;;
      --dry-run) dry_run=1; shift ;;
      --skip-preflight) skip_preflight=1; shift ;;
      -*) echo "unknown option: $1" >&2; usage ;;
      *) [ -z "$version" ] || usage; version=$1; shift ;;
    esac
  done
  { [ -n "$version" ] && [ -n "$title" ]; } || usage
  [ "$skip_preflight" -eq 0 ] || [ "$dry_run" -eq 1 ] || die "--skip-preflight is only allowed with --dry-run"
  check_version "$version"
  init_gh
  tag=v$version
  branch=release/$tag
  [ "$dry_run" -eq 0 ] || note "DRY RUN: nothing will be edited, committed, pushed or created"

  git fetch --quiet "$GITHUB_REMOTE" 'refs/tags/v*:refs/tags/v*'
  if git rev-parse --verify --quiet "refs/tags/$tag" >/dev/null; then problem "tag $tag already exists locally"; fi
  for remote in "$GITHUB_REMOTE" "$MIRROR_REMOTE"; do
    if [ -n "$(git ls-remote --tags "$remote" "refs/tags/$tag")" ]; then problem "tag $tag already exists on $remote"; fi
  done

  current_branch=$(git branch --show-current)
  if [ "$current_branch" != "$branch" ]; then
    if git rev-parse --verify --quiet "refs/heads/$branch" >/dev/null; then
      problem "branch $branch exists but is not checked out (on '${current_branch:-detached HEAD}')"
    else
      echo "will create $branch from HEAD $(git rev-parse --short HEAD)"
    fi
  fi

  current_version=$(workspace_version)
  bump_needed=1
  if [ "$current_version" = "$version" ]; then
    bump_needed=0
    echo "workspace is already at $version (resuming)"
  elif python3 -c 'import sys; a,b=(tuple(map(int,v.split("."))) for v in sys.argv[1:]); sys.exit(0 if b>a else 1)' "$current_version" "$version"; then
    :
  else
    problem "$version is not greater than the current workspace version $current_version"
  fi

  dirty=$(git status --porcelain --untracked-files=all)
  if [ -n "$dirty" ]; then
    resumable=0
    if [ "$bump_needed" -eq 0 ]; then
      resumable=1
      for path in $(git status --porcelain --untracked-files=all | sed 's/^...//'); do
        case "$path" in
          Cargo.toml | Cargo.lock | THIRD_PARTY_NOTICES.md | CHANGELOG.md) ;;
          *) resumable=0 ;;
        esac
      done
    fi
    [ "$resumable" -eq 1 ] || problem "working tree is not clean:
$dirty"
  fi

  state=$(changelog_state "$version")
  [ "$state" != empty ] || problem "CHANGELOG.md [Unreleased] has no content"

  previous=$(previous_tag_for "$version")
  if [ -z "$previous" ]; then
    echo "no previous release tag found; skipping upgrade-smoke gate"
  else
    triggers=$(git diff --name-only "$previous..HEAD" -- crates/updater/ crates/protocol/ crates/session-service/)
    if [ -n "$triggers" ]; then
      echo "upgrade-smoke trigger: $previous..HEAD touches:"
      echo "$triggers" | sed 's/^/  /'
      if [ "$smoke" -eq 1 ]; then
        echo "  --upgrade-smoke-passed given: accepted"
      else
        problem "upgrade smoke test required. Build the fixture pair with
  scripts/build-upgrade-fixtures.sh $previous --next-version $version
exercise the update, then re-run with --upgrade-smoke-passed"
      fi
    else
      echo "upgrade-smoke gate: $previous..HEAD touches none of crates/updater, crates/protocol, crates/session-service"
    fi
  fi

  date_today=$(date +%Y-%m-%d)
  if [ "$dry_run" -eq 1 ]; then
    echo "planned edits:"
    if [ "$bump_needed" -eq 1 ]; then
      echo "  Cargo.toml [workspace.package] version $current_version -> $version"
      echo "  Cargo.lock: 10 hh-* packages -> $version"
      echo "  THIRD_PARTY_NOTICES.md: 10 hh-* lines -> $version"
    fi
    if [ "$state" = pending ]; then
      echo "  CHANGELOG.md: insert '## [$version] - $date_today' after '## [Unreleased]'"
    fi
    echo "  commit 'Release $version: $title' on $branch, push to $GITHUB_REMOTE, open PR"
    if [ "$state" != empty ]; then
      body=$(changelog_apply "$version" "$date_today" 0)
      echo "PR body would be the changelog section ($(printf '%s\n' "$body" | wc -l | tr -d ' ') lines)"
    fi
    if [ "$skip_preflight" -eq 0 ]; then
      note "running preflight on the unedited tree"
      scripts/preflight.sh
    else
      echo "preflight skipped (--skip-preflight)"
    fi
    echo "dry run finished: $dry_problems check(s) would have refused"
    return 0
  fi

  if [ "$current_branch" != "$branch" ]; then
    git switch -c "$branch"
  fi

  if [ "$bump_needed" -eq 1 ]; then
    note "bumping $current_version -> $version"
    apply_bump "$current_version" "$version"
    cargo metadata --locked --format-version 1 --no-deps >/dev/null || die "Cargo.lock is inconsistent after the bump"
  fi
  body=$(changelog_apply "$version" "$date_today" 1)

  scripts/preflight.sh

  if [ -n "$(git status --porcelain --untracked-files=all)" ]; then
    git add Cargo.toml Cargo.lock THIRD_PARTY_NOTICES.md CHANGELOG.md
    git commit --quiet -m "Release $version: $title"
  fi
  note "pushing $branch to $GITHUB_REMOTE"
  git push --set-upstream "$GITHUB_REMOTE" "$branch"

  body_file=$(mktemp "${TMPDIR:-/tmp}/hh-release-body.XXXXXX")
  printf '%s\n' "$body" > "$body_file"
  existing=$(find_pr "$branch" open)
  if [ -n "$existing" ]; then
    pr_number=${existing%%"$(printf '\t')"*}
    gh pr edit "$pr_number" --repo "$REPO" --title "Release $version: $title" --body-file "$body_file" >/dev/null
    note "updated existing PR #$pr_number"
  else
    gh pr create --repo "$REPO" --base main --head "$branch" \
      --title "Release $version: $title" --body-file "$body_file"
  fi
  rm -f "$body_file"
  pr_number=$(find_pr "$branch" open | cut -f1)
  [ -n "$pr_number" ] || die "could not find the release PR"
  PREPARED_PR=$pr_number
  echo "PR #$pr_number: https://github.com/$REPO/pull/$pr_number"
}

# ---------------------------------------------------------------- watchers

# run_watcher MODE REF [EXTRA]: MODE is pr (REF = head sha), release (REF = tag) or
# dispatch (REF = workflow file, EXTRA = epoch seconds of the dispatch); WATCH_REPO overrides the repo.
# Exit 0 = success, 1 = a job failed, 3 = timed out.
run_watcher() {
  python3 - "${WATCH_REPO:-$REPO}" "$POLL_SECONDS" "$@" <<'PY'
import json, os, subprocess, sys, time

repo, interval, mode, ref = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
deadline_seconds = 3 * 3600
once = os.environ.get("HH_WATCH_ONCE") == "1"
start = time.time()


def stamp():
    s = int(time.time() - start)
    return "%02d:%02d" % (s // 60, s % 60)


def gh(path, tries=4):
    last = ""
    for attempt in range(tries):
        p = subprocess.run(["gh", "api", path], capture_output=True, text=True)
        if p.returncode == 0:
            return json.loads(p.stdout)
        last = p.stderr.strip()
        time.sleep(3)
    print("gh api %s failed: %s" % (path, last), file=sys.stderr)
    sys.exit(2)


def newest(runs):
    return max(runs, key=lambda r: r["id"]) if runs else None


def jobs_of(run):
    return gh("repos/%s/actions/runs/%d/jobs?filter=latest&per_page=100" % (repo, run["id"]))["jobs"]


def duration(job):
    if job.get("started_at") and job.get("completed_at"):
        from datetime import datetime
        f = lambda s: datetime.strptime(s, "%Y-%m-%dT%H:%M:%SZ")
        return max(0, int((f(job["completed_at"]) - f(job["started_at"])).total_seconds()))
    return 0


GOOD = ("success", "skipped", "neutral")
printed = set()


def report_jobs(run):
    """Print newly completed jobs; return False after reporting a failed job."""
    for job in jobs_of(run):
        if job["status"] != "completed" or job["id"] in printed:
            continue
        printed.add(job["id"])
        ok = job["conclusion"] in GOOD
        d = duration(job)
        print("[%s] %s %s / %s (%s, %dm%02ds)" % (
            stamp(), "ok  " if ok else "FAIL", run["name"], job["name"], job["conclusion"], d // 60, d % 60), flush=True)
        if not ok:
            steps = [s["name"] for s in job.get("steps", []) if s.get("conclusion") not in GOOD + (None,)]
            print("  workflow: %s (run %d, attempt %d)" % (run["name"], run["id"], run.get("run_attempt", 1)))
            print("  job:      %s" % job["name"])
            print("  failed steps: %s" % (", ".join(steps) or "(none recorded)"))
            print("  url:      %s" % job["html_url"])
            return False
    return True


def run_failed_without_job(run):
    print("[%s] FAIL %s run finished with conclusion %s (no failed job recorded)" % (stamp(), run["name"], run["conclusion"]))
    print("  url: %s" % run["html_url"])


def watch_pr(sha):
    required = ["CI", "Security"]
    optional = "Packaging Assurance"
    while time.time() - start < deadline_seconds:
        runs = gh("repos/%s/actions/runs?head_sha=%s&per_page=100" % (repo, sha))["workflow_runs"]
        runs = [r for r in runs if r["event"] == "pull_request"]
        by_name = {}
        for r in runs:
            if r["name"] not in by_name or r["id"] > by_name[r["name"]]["id"]:
                by_name[r["name"]] = r
        watched = [n for n in required + [optional] if n in by_name]
        for name in watched:
            run = by_name[name]
            if not report_jobs(run):
                return 1
            if run["status"] == "completed" and run["conclusion"] not in GOOD:
                run_failed_without_job(run)
                return 1
        missing = [n for n in required if n not in by_name]
        # Packaging Assurance is path filtered and appears a little after the others.
        settling = optional not in by_name and time.time() - start < 90 and not once
        done = not missing and not settling and all(by_name[n]["status"] == "completed" for n in watched)
        if done:
            print("[%s] all required runs succeeded: %s" % (stamp(), ", ".join(watched)))
            if optional not in by_name:
                print("  (%s did not trigger for this head)" % optional)
            return 0
        if once:
            print("not green yet: %s" % ("missing runs: " + ", ".join(missing) if missing else "runs still in progress"))
            return 1
        time.sleep(interval)
    print("timed out waiting for runs on %s" % sha, file=sys.stderr)
    return 3


def follow(run):
    """Follow one run to completion. Returns (exit code, final run)."""
    while time.time() - start < deadline_seconds:
        run = gh("repos/%s/actions/runs/%d" % (repo, run["id"]))
        if not report_jobs(run):
            print("  note: `gh run rerun %d --failed` only works once the whole run has finished;" % run["id"])
            print("        other jobs may still be running, so wait for the run to end before rerunning.")
            return 1, run
        if run["status"] == "completed":
            if run["conclusion"] != "success":
                run_failed_without_job(run)
                return 1, run
            return 0, run
        time.sleep(interval)
    print("timed out waiting for run %d" % run["id"], file=sys.stderr)
    return 3, run


def watch_dispatch(workflow, since):
    """Follow the workflow_dispatch run of WORKFLOW created at/after SINCE (UTC epoch seconds)."""
    from datetime import datetime, timezone
    parse = lambda s: datetime.strptime(s, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc).timestamp()
    run = None
    while run is None and time.time() - start < 600:
        runs = gh("repos/%s/actions/workflows/%s/runs?event=workflow_dispatch&per_page=20" % (repo, workflow))["workflow_runs"]
        run = newest([r for r in runs if parse(r["created_at"]) >= float(since) - 5])
        if run is None:
            print("[%s] waiting for the %s run to appear" % (stamp(), workflow), flush=True)
            time.sleep(min(interval, 5))
    if run is None:
        print("no %s run appeared" % workflow, file=sys.stderr)
        return 3
    print("%s run %d: %s" % (workflow, run["id"], run["html_url"]), flush=True)
    rc, run = follow(run)
    if not rc:
        print("[%s] %s succeeded" % (stamp(), workflow))
    return rc


def watch_release(tag):
    run = None
    while run is None and time.time() - start < 600:
        runs = gh("repos/%s/actions/runs?event=push&per_page=100" % repo)["workflow_runs"]
        run = newest([r for r in runs if r["name"] == "Release" and r["head_branch"] == tag])
        if run is None:
            print("[%s] waiting for the Release run for %s to appear" % (stamp(), tag), flush=True)
            time.sleep(interval)
    if run is None:
        print("no Release run found for %s" % tag, file=sys.stderr)
        return 3
    print("Release run %d: %s" % (run["id"], run["html_url"]), flush=True)
    rc, run = follow(run)
    if rc:
        return rc
    release = subprocess.run(
        ["gh", "release", "view", tag, "--repo", repo, "--json", "isDraft,isPrerelease,url,publishedAt,tagName"],
        capture_output=True, text=True)
    if release.returncode != 0:
        print("release run succeeded but `gh release view %s` failed: %s" % (tag, release.stderr.strip()))
        return 1
    info = json.loads(release.stdout)
    if info["isDraft"]:
        print("release run succeeded but the release is still a draft: %s" % info["url"])
        return 1
    print("[%s] release %s is published (%s): %s" % (stamp(), info["tagName"], info["publishedAt"], info["url"]))
    from datetime import datetime
    f = lambda s: datetime.strptime(s, "%Y-%m-%dT%H:%M:%SZ")
    total = int((f(run["updated_at"]) - f(run.get("run_started_at") or run["created_at"])).total_seconds())
    print("release workflow total: %dm%02ds" % (total // 60, total % 60))
    return 0


if mode == "pr":
    sys.exit(watch_pr(ref))
elif mode == "release":
    sys.exit(watch_release(ref))
else:
    sys.exit(watch_dispatch(ref, sys.argv[5]))
PY
}

pr_head_sha() {
  gh pr view "$1" --repo "$REPO" --json headRefOid --jq .headRefOid
}

cmd_watch_pr() {
  [ "$#" -eq 1 ] || usage
  case "$1" in *[!0-9]* | '') echo "PR number must be numeric" >&2; usage ;; esac
  init_gh
  sha=$(pr_head_sha "$1")
  note "watching PR #$1 head $sha"
  run_watcher pr "$sha"
}

# ---------------------------------------------------------------- publish

cmd_publish() {
  [ "$#" -eq 1 ] || usage
  version=$1
  check_version "$version"
  init_gh
  tag=v$version
  branch=release/$tag

  info=$(find_pr "$branch" all)
  [ -n "$info" ] || die "no pull request found for $branch"
  tab=$(printf '\t')
  pr_number=${info%%"$tab"*}
  rest=${info#*"$tab"}
  pr_state=${rest%%"$tab"*}
  head_sha=${rest#*"$tab"}

  if [ "$pr_state" = OPEN ]; then
    # Single-shot check with the same rules as watch-pr: exit 0 only when green.
    note "checking PR #$pr_number is green"
    run_watcher_once "$head_sha" || die "PR #$pr_number is not green; run: scripts/release.sh watch-pr $pr_number"
    title=$(gh pr view "$pr_number" --repo "$REPO" --json title --jq .title)
    note "squash-merging PR #$pr_number at $head_sha"
    gh pr merge "$pr_number" --repo "$REPO" --squash --match-head-commit "$head_sha" \
      --subject "$title (#$pr_number)" --body ""
  elif [ "$pr_state" = MERGED ]; then
    echo "PR #$pr_number is already merged (resuming)"
  else
    die "PR #$pr_number is $pr_state"
  fi

  merge_sha=$(gh pr view "$pr_number" --repo "$REPO" --json mergeCommit --jq .mergeCommit.oid)
  [ -n "$merge_sha" ] || die "PR #$pr_number has no merge commit"
  git fetch --quiet "$GITHUB_REMOTE" main
  git merge-base --is-ancestor "$merge_sha" "$GITHUB_REMOTE/main" ||
    die "merge commit $merge_sha is not on $GITHUB_REMOTE/main"
  if ! git cat-file -e "$head_sha^{commit}" 2>/dev/null; then
    git fetch --quiet "$GITHUB_REMOTE" "refs/pull/$pr_number/head"
  fi
  if [ "$(git rev-parse "$merge_sha^{tree}")" = "$(git rev-parse "$head_sha^{tree}")" ]; then
    note "merge commit tree equals the PR head tree: the CI that passed covers the tagged tree"
  else
    note "merge commit tree DIFFERS from the PR head tree: main moved; the release will wait for CI on the main push"
  fi

  if git rev-parse --verify --quiet "refs/tags/$tag" >/dev/null; then
    [ "$(git rev-parse "refs/tags/$tag^{commit}")" = "$merge_sha" ] || die "local tag $tag points elsewhere than $merge_sha"
    echo "tag $tag already exists locally at $merge_sha"
  else
    git tag -s "$tag" -m "Harness Harlot $version" "$merge_sha"
    note "created signed tag $tag at $merge_sha"
  fi
  git push "$GITHUB_REMOTE" "refs/tags/$tag"
  git push "$MIRROR_REMOTE" "refs/tags/$tag"
  git push "$MIRROR_REMOTE" "$merge_sha:refs/heads/main"
  note "published tag $tag; the Release workflow is starting"
}

# run_watcher_once SHA: one non-blocking pass of the PR rules (success only if all done and green).
run_watcher_once() {
  HH_WATCH_ONCE=1 run_watcher pr "$1"
}

# ---------------------------------------------------------------- driver

cmd_watch_release() {
  [ "$#" -eq 1 ] || usage
  case "$1" in v[0-9]*.[0-9]*.[0-9]*) ;; *) echo "expected a tag like v0.1.28" >&2; usage ;; esac
  init_gh
  run_watcher release "$1"
}

# ---------------------------------------------------------------- land

LANDING_REPO=highlyproteus/harness-harlot-landing
SITE=https://harnessharlot.com

dispatch_and_watch() { # REPO WORKFLOW_FILE
  since=$(date +%s)
  note "dispatching $2 in $1"
  gh workflow run "$2" --repo "$1" --ref main
  WATCH_REPO=$1 run_watcher dispatch "$2" "$since"
}

# site_version FILE -> top-level "version" of the published index, empty on any error
site_version() {
  curl -fsS --max-time 20 "$SITE/releases/$1" 2>/dev/null |
    python3 -c 'import json,sys; print(json.load(sys.stdin).get("version",""))' 2>/dev/null || true
}

cmd_land() {
  [ "$#" -eq 1 ] || usage
  case "$1" in v[0-9]*.[0-9]*.[0-9]*) ;; *) echo "expected a tag like v0.1.28" >&2; usage ;; esac
  tag=$1
  version=${tag#v}
  init_gh
  dispatch_and_watch "$REPO" refresh-stable-v2.yml
  dispatch_and_watch "$LANDING_REPO" sync-release.yml
  note "waiting for $SITE to serve $version (up to 15 min)"
  started=$(date +%s)
  while :; do
    macos=$(site_version stable-macos.json)
    linux=$(site_version stable-linux.json)
    if [ "$macos" = "$version" ] && [ "$linux" = "$version" ]; then
      note "site serves $version (macOS and Linux) after $(($(date +%s) - started))s of polling"
      return 0
    fi
    [ $(($(date +%s) - started)) -lt 900 ] || die "timed out: stable-macos.json=${macos:-?} stable-linux.json=${linux:-?}, wanted $version"
    echo "  site: macos=${macos:-?} linux=${linux:-?} (want $version)"
    sleep 30
  done
}

cmd_all() {
  version=$1
  shift
  for arg in "$@"; do
    [ "$arg" != --dry-run ] || die "--dry-run applies to the prepare subcommand only"
  done
  PREPARED_PR=
  cmd_prepare "$version" "$@"
  [ -n "$PREPARED_PR" ] || die "prepare did not report a PR number"
  cmd_watch_pr "$PREPARED_PR"
  cmd_publish "$version"
  cmd_watch_release "v$version"
  cmd_land "v$version"
}

[ "$#" -ge 1 ] || usage
case "$1" in
  -h | --help | help) usage 0 ;;
  prepare) shift; cmd_prepare "$@" ;;
  watch-pr) shift; cmd_watch_pr "$@" ;;
  publish) shift; cmd_publish "$@" ;;
  watch-release) shift; cmd_watch_release "$@" ;;
  land) shift; cmd_land "$@" ;;
  [0-9]*) cmd_all "$@" ;;
  *) echo "unknown subcommand: $1" >&2; usage ;;
esac
