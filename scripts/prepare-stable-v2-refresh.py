#!/usr/bin/env python3
"""Verify and renew the four stable-v2 release manifest aliases.

This helper never signs. It authenticates immutable release inputs with the
compiled stable-v2 public key, verifies their artifact bytes, and emits only
canonical manifests plus a deterministic source-binding descriptor.
"""

from __future__ import annotations

import argparse
import base64
import datetime as dt
import hashlib
import json
import os
import re
import subprocess
import tempfile
from pathlib import Path
from typing import Any, NoReturn, cast

ALIASES = {
    "manifest-linux-arm64-v2.update.json": ("linux", "arm64", "tar.gz"),
    "manifest-linux-x86_64-v2.update.json": ("linux", "x86_64", "tar.gz"),
    "manifest-macos-community-arm64-v2.update.json": ("macos", "arm64", "dmg"),
    "manifest-macos-community-x86_64-v2.update.json": ("macos", "x86_64", "dmg"),
}
MAX_MANIFEST_BYTES = 1024 * 1024
MAX_SIGNATURE_BYTES = 4 * 1024
MAX_ARTIFACT_BYTES = 2 * 1024 * 1024 * 1024
TAG_RE = re.compile(r"v(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)")
COMMIT_RE = re.compile(r"[0-9a-f]{40}")
TIME_RE = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z")
PUBLIC_DER_PREFIX = bytes.fromhex("302a300506032b6570032100")


def fail(message: str) -> NoReturn:
    raise SystemExit(message)


def regular_file(path: Path, maximum: int, label: str) -> bytes:
    if path.is_symlink() or not path.is_file():
        fail(f"{label} must be a regular non-symlink file: {path.name}")
    size = path.stat().st_size
    if size <= 0 or size > maximum:
        fail(f"{label} size is outside policy: {path.name}")
    return path.read_bytes()


def parse_time(value: str, label: str) -> dt.datetime:
    if not TIME_RE.fullmatch(value):
        fail(f"{label} must be canonical UTC seconds")
    try:
        parsed = dt.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ")
    except ValueError as error:
        fail(f"invalid {label}: {error}")
    return parsed.replace(tzinfo=dt.timezone.utc)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def verify_signature(manifest: Path, signature: Path, public_key: bytes) -> None:
    encoded = regular_file(signature, MAX_SIGNATURE_BYTES, "signature").strip()
    try:
        raw = base64.b64decode(encoded, validate=True)
    except Exception as error:
        fail(f"invalid signature encoding for {manifest.name}: {error}")
    if len(raw) != 64:
        fail(f"invalid Ed25519 signature size for {manifest.name}")
    openssl = os.environ.get("HH_OPENSSL", "openssl")
    with tempfile.TemporaryDirectory(prefix="hh-refresh-verify-") as temporary:
        root = Path(temporary)
        public_der = root / "public.der"
        public_pem = root / "public.pem"
        signature_raw = root / "signature.bin"
        public_der.write_bytes(PUBLIC_DER_PREFIX + public_key)
        signature_raw.write_bytes(raw)
        try:
            subprocess.run(
                [openssl, "pkey", "-pubin", "-inform", "DER", "-in", str(public_der), "-out", str(public_pem)],
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            subprocess.run(
                [openssl, "pkeyutl", "-verify", "-pubin", "-inkey", str(public_pem), "-rawin", "-in", str(manifest), "-sigfile", str(signature_raw)],
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        except (OSError, subprocess.CalledProcessError) as error:
            fail(f"existing signature is not trusted for {manifest.name}: {error}")


def require_string(mapping: dict[str, Any], key: str) -> str:
    value = mapping.get(key)
    if not isinstance(value, str) or not value:
        fail(f"manifest has invalid {key}")
    return cast(str, value)


def verify_manifest(
    release_dir: Path,
    alias: str,
    expected: tuple[str, str, str],
    tag: str,
    public_key: bytes,
    published_at: str,
    valid_until: str,
) -> tuple[bytes, dict[str, Any]]:
    path = release_dir / alias
    original_bytes = regular_file(path, MAX_MANIFEST_BYTES, "manifest")
    verify_signature(path, release_dir / f"{alias}.sig", public_key)
    try:
        manifest = json.loads(original_bytes)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"invalid manifest JSON in {alias}: {error}")
    if not isinstance(manifest, dict):
        fail(f"manifest root is not an object: {alias}")
    canonical_source = (json.dumps(manifest, indent=2, ensure_ascii=False) + "\n").encode()
    if original_bytes != canonical_source:
        fail(f"source manifest is not canonical JSON: {alias}")
    platform, architecture, artifact_format = expected
    fixed = {
        "schema": "hh-update-manifest-v2",
        "product": "Harness Harlot",
        "channel": "stable",
        "key_id": "hh-stable-2026-v2",
        "platform": platform,
    }
    for key, value in fixed.items():
        if manifest.get(key) != value:
            fail(f"{alias} has unexpected {key}")
    if require_string(manifest, "version") != tag[1:]:
        fail(f"{alias} version does not match release tag")
    build = manifest.get("build")
    if isinstance(build, bool) or not isinstance(build, int) or build <= 0:
        fail(f"{alias} has invalid build")
    artifacts = manifest.get("artifacts")
    if not isinstance(artifacts, list) or len(artifacts) != 1 or not isinstance(artifacts[0], dict):
        fail(f"{alias} must describe exactly one artifact")
    artifact = artifacts[0]
    if artifact.get("platform") != platform or artifact.get("architecture") != architecture:
        fail(f"{alias} artifact identity mismatch")
    if artifact.get("format") != artifact_format:
        fail(f"{alias} artifact format mismatch")
    name = require_string(artifact, "file_name")
    if Path(name).name != name or name.startswith("."):
        fail(f"{alias} artifact name is unsafe")
    expected_url = f"https://github.com/highlyproteus/harness-harlot/releases/download/{tag}/{name}"
    if artifact.get("url") != expected_url:
        fail(f"{alias} artifact URL is not the exact immutable release URL")
    size = artifact.get("size")
    if isinstance(size, bool) or not isinstance(size, int) or not 0 < size <= MAX_ARTIFACT_BYTES:
        fail(f"{alias} artifact size is invalid")
    digest = artifact.get("sha256")
    if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest):
        fail(f"{alias} artifact digest is invalid")
    artifact_bytes = regular_file(release_dir / name, MAX_ARTIFACT_BYTES, "artifact")
    if len(artifact_bytes) != size or sha256(artifact_bytes) != digest:
        fail(f"{alias} artifact bytes do not match signed identity")

    old_published = require_string(manifest, "published_at")
    old_valid = require_string(manifest, "valid_until")
    if parse_time(old_valid, "source valid_until") <= parse_time(
        old_published, "source published_at"
    ):
        fail(f"source manifest validity window is invalid: {alias}")
    renewed = dict(manifest)
    renewed["published_at"] = published_at
    renewed["valid_until"] = valid_until
    original_without_time = {key: value for key, value in manifest.items() if key not in {"published_at", "valid_until"}}
    renewed_without_time = {key: value for key, value in renewed.items() if key not in {"published_at", "valid_until"}}
    if renewed_without_time != original_without_time:
        fail(f"renewal changed fields other than timestamps: {alias}")
    old_published_field = json.dumps("published_at") + ": " + json.dumps(old_published)
    new_published_field = json.dumps("published_at") + ": " + json.dumps(published_at)
    old_valid_field = json.dumps("valid_until") + ": " + json.dumps(old_valid)
    new_valid_field = json.dumps("valid_until") + ": " + json.dumps(valid_until)
    source_text = original_bytes.decode("utf-8")
    if source_text.count(old_published_field) != 1 or source_text.count(old_valid_field) != 1:
        fail(f"source manifest timestamp fields are not unique: {alias}")
    canonical = source_text.replace(old_published_field, new_published_field).replace(
        old_valid_field, new_valid_field
    ).encode()
    return canonical, {
        "name": alias,
        "signature": f"{alias}.sig",
        "sha256": sha256(canonical),
        "size": len(canonical),
        "source_sha256": sha256(original_bytes),
        "artifact": {"name": name, "sha256": digest, "size": size, "url": expected_url},
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--release-dir", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--published-at", required=True)
    parser.add_argument("--valid-until", required=True)
    parser.add_argument("--public-key", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--workflow", required=True)
    parser.add_argument("--source-ref", required=True)
    parser.add_argument("--run-id", required=True, type=int)
    parser.add_argument("--run-attempt", required=True, type=int)
    parser.add_argument("--head-sha", required=True)
    parser.add_argument("--release-id", required=True, type=int)
    args = parser.parse_args()
    if not TAG_RE.fullmatch(args.tag):
        fail("release tag is not canonical")
    if not COMMIT_RE.fullmatch(args.commit):
        fail("release commit is not a lower-case 40-character SHA")
    if args.repository != "highlyproteus/harness-harlot":
        fail("refresh repository is not canonical")
    if args.workflow != ".github/workflows/refresh-stable-v2.yml":
        fail("refresh workflow is not canonical")
    if args.source_ref != "refs/heads/main":
        fail("refresh source ref is not protected main")
    if args.run_id <= 0 or args.run_attempt <= 0 or args.release_id <= 0:
        fail("refresh run or release identity is invalid")
    if not COMMIT_RE.fullmatch(args.head_sha):
        fail("refresh head SHA is invalid")
    published = parse_time(args.published_at, "published_at")
    valid = parse_time(args.valid_until, "valid_until")
    if valid - published != dt.timedelta(days=7):
        fail("stable-v2 validity window must be exactly seven days")
    try:
        public_key = base64.b64decode(args.public_key, validate=True)
    except Exception as error:
        fail(f"stable-v2 public key is not base64: {error}")
    if len(public_key) != 32:
        fail("stable-v2 public key must decode to 32 bytes")
    if args.output_dir.exists():
        fail("output directory must not already exist")
    args.output_dir.mkdir(mode=0o700, parents=False)
    manifests = []
    for alias in sorted(ALIASES):
        canonical, metadata = verify_manifest(
            args.release_dir,
            alias,
            ALIASES[alias],
            args.tag,
            public_key,
            args.published_at,
            args.valid_until,
        )
        (args.output_dir / alias).write_bytes(canonical)
        manifests.append(metadata)
    descriptor = {
        "schema": "hh-stable-v2-refresh-v1",
        "repository": args.repository,
        "workflow": args.workflow,
        "source_ref": args.source_ref,
        "run_id": args.run_id,
        "run_attempt": args.run_attempt,
        "head_sha": args.head_sha,
        "release_id": args.release_id,
        "release_tag": args.tag,
        "generated_at": args.published_at,
    }
    (args.output_dir / "stable-v2-refresh.json").write_text(
        json.dumps(descriptor, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )


if __name__ == "__main__":
    main()
