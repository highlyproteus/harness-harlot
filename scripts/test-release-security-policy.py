#!/usr/bin/env python3
"""Fail closed if release credentials or trust roots drift back into build code."""

from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RELEASE = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
EDGE = (ROOT / ".github/workflows/edge.yml").read_text(encoding="utf-8")
CI = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
MAC_PACKAGE = (ROOT / "scripts/package-macos-release.sh").read_text(encoding="utf-8")
LINUX_PACKAGE = (ROOT / "scripts/package-linux-release.sh").read_text(encoding="utf-8")
CANONICAL_SIGNER = (ROOT / "scripts/isolated-ed25519-sign.sh").read_text(encoding="utf-8").rstrip("\n")


def job(workflow: str, name: str) -> str:
    match = re.search(
        rf"^  {re.escape(name)}:\n(?P<body>.*?)(?=^  [A-Za-z0-9_-]+:\n|\Z)",
        workflow,
        flags=re.MULTILINE | re.DOTALL,
    )
    assert match is not None, f"missing workflow job: {name}"
    return match.group("body")


def has_no_build_surface(block: str, label: str) -> None:
    forbidden = ("actions/checkout", "cargo ", "scripts/", "npm ", "make ")
    for token in forbidden:
        assert token not in block, f"{label} executes target-controlled build surface: {token}"


def embedded_signers(workflow: str) -> list[str]:
    matches = re.findall(
        r"^          # BEGIN ISOLATED SIGNER V1\n"
        r"^          cat > .*?\n"
        r"(?P<body>.*?)"
        r"^          SIGNER\n"
        r"^          # END ISOLATED SIGNER V1$",
        workflow,
        flags=re.MULTILINE | re.DOTALL,
    )
    return ["\n".join(line[10:] for line in body.rstrip("\n").splitlines()) for body in matches]


assert "release-tag-allowed-signers" not in RELEASE, (
    "tag trust must come from hosted rules, not the candidate checkout"
)
for workflow_name, workflow in (("release", RELEASE), ("edge", EDGE), ("CI", CI)):
    for token in ("cef_sha1", "CEF_SHA1", "sha1sum", "shasum -a 1"):
        assert token not in workflow, f"{workflow_name} retains SHA-1 CEF trust: {token}"

for package_name in ("package", "package-linux"):
    block = job(RELEASE, package_name)
    for token in ("SIGNING_SEED", "SIGNING_KEY_FILE", "HH_UPDATE_PUBLIC_KEY"):
        assert token not in block, f"{package_name} receives release authority: {token}"
    assert "HH_RELEASE_UNSIGNED: 1" in block, f"{package_name} is not explicitly unsigned"

linux_package = job(RELEASE, "package-linux")
checkout_trust_match = re.search(
    r"^      - name: Install checkout trust dependencies\n"
    r"(?P<body>.*?)(?=^      - )",
    linux_package,
    flags=re.MULTILINE | re.DOTALL,
)
assert checkout_trust_match is not None, "missing Linux checkout trust dependency step"
checkout_trust_step = checkout_trust_match.group("body")
hosted_tag_check_start = linux_package.index(
    "- name: Validate protected tag, commit identity, and runner architecture"
)
first_hosted_api_use = linux_package.index("gh api")
assert checkout_trust_match.end() < hosted_tag_check_start < first_hosted_api_use, (
    "Linux release container must finish installing trust tools before hosted tag verification"
)
assert re.search(
    r"apt-get install.*?ca-certificates git gh openssh-client",
    checkout_trust_step,
    flags=re.DOTALL,
), "Linux release container must install gh in its checkout trust step"

stable = job(RELEASE, "sign-stable-v2")
assert "environment: stable-signing-v2" in stable
assert "HH_UPDATE_SIGNING_SEED" in stable
assert "contents: write" not in stable
assert "git/tags/$tag_object" in stable
assert "commit.verification.verified" in stable
assert "commit_author" in stable
has_no_build_surface(stable, "sign-stable-v2")
assert "assert set(by_name) == expected_files" in stable, (
    "stable signing must reject every extra unsigned release input"
)

legacy = job(RELEASE, "sign-legacy-bridge")
assert "environment: release" in legacy
assert "github.ref_name == 'v0.1.16'" in legacy
assert "github.ref_name != 'v0.1.16'" in legacy
assert "HH_UPDATE_SIGNING_SEED" in legacy
assert "contents: write" not in legacy
has_no_build_surface(legacy, "sign-legacy-bridge")
assert "published.replace(year=published.year + 10)" in legacy, (
    "the one-time legacy bridge must outlive the seven-day normal feed"
)
assert "Download immutable v0.1.16 legacy bridge" in legacy, (
    "future latest releases must carry the old-client bridge without retaining its seed"
)
assert 'assert manifest["version"] == "0.1.16"' in legacy
assert "release-legacy-v016-bridge" in legacy
assert "0.1.14" not in legacy and "0.1.15" not in legacy

publish = job(RELEASE, "publish")
assert "SIGNING_SEED" not in publish
assert "refs/tags/$GITHUB_REF_NAME^{}" in publish
assert "sign-stable-v2" in publish and "sign-legacy-bridge" in publish
assert "--latest=false" in publish and "--latest" in publish
assert "vars.HH_UPDATE_LEGACY_PUBLIC_KEY_V1" in publish
assert "assert set(by_name) == expected_files" in publish, (
    "stable publication must reject every extra release asset"
)

edge_build = job(EDGE, "build-edge")
assert "HH_RELEASE_UNSIGNED: 1" in edge_build
assert "SIGNING_SEED" not in edge_build and "SIGNING_KEY_FILE" not in edge_build

edge_sign = job(EDGE, "sign-edge")
assert "environment: edge-signing-v1" in edge_sign
assert "HH_UPDATE_SIGNING_SEED" in edge_sign
assert "contents: write" not in edge_sign
has_no_build_surface(edge_sign, "sign-edge")
assert "assert set(by_name) == expected_files" in edge_sign, (
    "edge signing must reject every extra unsigned release input"
)

edge_publish = job(EDGE, "publish-edge")
assert "SIGNING_SEED" not in edge_publish
assert "contents: write" in edge_publish
assert "assert set(by_name) == expected_files" in edge_publish, (
    "edge publication must reject every extra release asset"
)

signer_copies = embedded_signers(RELEASE) + embedded_signers(EDGE)
assert len(signer_copies) == 3, f"expected three isolated signer copies, found {len(signer_copies)}"
assert all(copy == CANONICAL_SIGNER for copy in signer_copies), (
    "workflow signer copy drifted from the tested canonical implementation"
)

for package_name, source in (("macOS", MAC_PACKAGE), ("Linux", LINUX_PACKAGE)):
    assert "HH_RELEASE_UNSIGNED" in source, f"{package_name} package lacks unsigned mode"
    assert "-v2.update.json" in source, f"{package_name} package lacks rotated feed name"
    assert "git verify-tag" not in source, (
        f"{package_name} package incorrectly depends on runner-local SSH allowed signers"
    )

expected_sha256 = {
    "70c8b97c4dead81b67a8fb29b80da12681e008d4ae9a9778f59e5a2f2adc4e08",
    "7ab55b3e45d7a89088d498a5fb6b231c3d3bd17fc1a3eb2aee6c8875f7bd842d",
    "554c2c107a4ca8d555273c0c0d0c1efdfbbb5a2d9ba3a2387dbdf3b622bdb24c",
    "c3acf4f408759cf39c274fc80dc4dee8054e1f4de46bfe15e71b4ada1aa4c664",
}
assert expected_sha256 <= set(re.findall(r"[0-9a-f]{64}", RELEASE + EDGE)), (
    "one or more independently measured CEF SHA-256 pins are missing"
)
assert "554c2c107a4ca8d555273c0c0d0c1efdfbbb5a2d9ba3a2387dbdf3b622bdb24c" in CI, (
    "CI must pin the independently verified Linux x86_64 CEF SHA-256"
)

print("release secret isolation, tag binding, channel separation, and SHA-256 pins are enforced")
