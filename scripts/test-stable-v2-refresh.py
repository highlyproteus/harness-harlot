#!/usr/bin/env python3
"""Exercise deterministic stable-v2 renewal without release credentials."""

from __future__ import annotations

import base64
import hashlib
import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PRODUCER = ROOT / "scripts/prepare-stable-v2-refresh.py"
OPENSSL = Path("/opt/homebrew/opt/openssl@3/bin/openssl")
if not OPENSSL.exists():
    OPENSSL = Path("openssl")
os.environ["HH_OPENSSL"] = str(OPENSSL)
TAG = "v9.8.7"
COMMIT = "a" * 40
PUBLISHED = "2026-08-31T12:00:00Z"
VALID = "2026-09-07T12:00:00Z"
ALIASES = (
    "manifest-linux-arm64-v2.update.json",
    "manifest-linux-x86_64-v2.update.json",
    "manifest-macos-community-arm64-v2.update.json",
    "manifest-macos-community-x86_64-v2.update.json",
)


def run(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(args, check=check, text=True, capture_output=True)


with tempfile.TemporaryDirectory() as temporary:
    root = Path(temporary)
    release = root / "release"
    release.mkdir()
    private = root / "private.pem"
    public_der = root / "public.der"
    run(str(OPENSSL), "genpkey", "-algorithm", "ED25519", "-out", str(private))
    run(
        str(OPENSSL),
        "pkey",
        "-in",
        str(private),
        "-pubout",
        "-outform",
        "DER",
        "-out",
        str(public_der),
    )
    public_key = base64.b64encode(public_der.read_bytes()[-32:]).decode()

    originals: dict[str, dict[str, object]] = {}
    for alias in ALIASES:
        platform = "linux" if "linux" in alias else "macos"
        architecture = "arm64" if "arm64" in alias else "x86_64"
        suffix = "tar.gz" if platform == "linux" else "community.dmg"
        artifact_name = f"Harness-Harlot-9.8.7-b42-{platform}-{architecture}.{suffix}"
        artifact_bytes = f"artifact:{platform}:{architecture}".encode()
        (release / artifact_name).write_bytes(artifact_bytes)
        manifest = {
            "schema": "hh-update-manifest-v2",
            "product": "Harness Harlot",
            "channel": "stable",
            "key_id": "hh-stable-2026-v2",
            "version": "9.8.7",
            "build": 42,
            "published_at": "2026-08-28T00:00:00Z",
            "valid_until": "2026-09-04T00:00:00Z",
            "platform": platform,
            **(
                {"minimum_glibc": "2.35"}
                if platform == "linux"
                else {"minimum_macos": "13.0"}
            ),
            "session_service": {
                "protocol_version": 11,
                "requires_quiescent_service": True,
            },
            "artifacts": [
                {
                    "platform": platform,
                    "architecture": architecture,
                    "format": "tar.gz" if platform == "linux" else "dmg",
                    "file_name": artifact_name,
                    "url": f"https://github.com/highlyproteus/harness-harlot/releases/download/{TAG}/{artifact_name}",
                    "sha256": hashlib.sha256(artifact_bytes).hexdigest(),
                    "size": len(artifact_bytes),
                }
            ],
        }
        originals[alias] = manifest
        manifest_path = release / alias
        manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        signature_raw = release / f"{alias}.raw"
        run(
            str(OPENSSL),
            "pkeyutl",
            "-sign",
            "-inkey",
            str(private),
            "-rawin",
            "-in",
            str(manifest_path),
            "-out",
            str(signature_raw),
        )
        (release / f"{alias}.sig").write_text(
            base64.b64encode(signature_raw.read_bytes()).decode() + "\n", encoding="ascii"
        )
        signature_raw.unlink()

    def produce(
        destination: Path,
        valid: str = VALID,
        source: Path = release,
        check: bool = True,
    ) -> subprocess.CompletedProcess[str]:
        return run(
            "python3",
            str(PRODUCER),
            "--release-dir",
            str(source),
            "--output-dir",
            str(destination),
            "--tag",
            TAG,
            "--commit",
            COMMIT,
            "--published-at",
            PUBLISHED,
            "--valid-until",
            valid,
            "--public-key",
            public_key,
            "--repository",
            "highlyproteus/harness-harlot",
            "--workflow",
            ".github/workflows/refresh-stable-v2.yml",
            "--source-ref",
            "refs/heads/main",
            "--run-id",
            "12345",
            "--run-attempt",
            "2",
            "--head-sha",
            "b" * 40,
            "--release-id",
            "67890",
            check=check,
        )

    first = root / "first"
    second = root / "second"
    produce(first)
    produce(second)
    expected = set(ALIASES) | {"stable-v2-refresh.json"}
    assert {path.name for path in first.iterdir()} == expected
    assert {path.name: path.read_bytes() for path in first.iterdir()} == {
        path.name: path.read_bytes() for path in second.iterdir()
    }
    for alias in ALIASES:
        renewed = json.loads((first / alias).read_text(encoding="utf-8"))
        original = dict(originals[alias])
        assert renewed.pop("published_at") == PUBLISHED
        assert renewed.pop("valid_until") == VALID
        original.pop("published_at")
        original.pop("valid_until")
        assert renewed == original
    descriptor = json.loads(
        (first / "stable-v2-refresh.json").read_text(encoding="utf-8")
    )
    assert descriptor == {
        "schema": "hh-stable-v2-refresh-v1",
        "repository": "highlyproteus/harness-harlot",
        "workflow": ".github/workflows/refresh-stable-v2.yml",
        "source_ref": "refs/heads/main",
        "run_id": 12345,
        "run_attempt": 2,
        "head_sha": "b" * 40,
        "release_id": 67890,
        "release_tag": TAG,
        "generated_at": PUBLISHED,
    }

    assert (
        produce(root / "bad-window", "2026-09-07T11:59:59Z", check=False).returncode
        != 0
    )
    tampered = root / "tampered"
    shutil.copytree(release, tampered)
    (tampered / ALIASES[0]).write_text(
        (tampered / ALIASES[0])
        .read_text(encoding="utf-8")
        .replace('"build": 42', '"build": 43'),
        encoding="utf-8",
    )
    assert produce(root / "bad-signature", source=tampered, check=False).returncode != 0

    noncanonical = root / "noncanonical"
    shutil.copytree(release, noncanonical)
    noncanonical_manifest = noncanonical / ALIASES[0]
    parsed = json.loads(noncanonical_manifest.read_text(encoding="utf-8"))
    noncanonical_manifest.write_text(json.dumps(parsed) + "\n", encoding="utf-8")
    signature_raw = root / "noncanonical-signature.raw"
    run(
        str(OPENSSL),
        "pkeyutl",
        "-sign",
        "-inkey",
        str(private),
        "-rawin",
        "-in",
        str(noncanonical_manifest),
        "-out",
        str(signature_raw),
    )
    (noncanonical / f"{ALIASES[0]}.sig").write_text(
        base64.b64encode(signature_raw.read_bytes()).decode() + "\n", encoding="ascii"
    )
    assert produce(root / "bad-canonical", source=noncanonical, check=False).returncode != 0

print("stable-v2 refresh producer renews only timestamps and rejects invalid inputs")
