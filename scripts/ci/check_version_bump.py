#!/usr/bin/env python3
"""Require a version bump when Rust sources change.

Collects the *effective* version of every crate in the cargo workspace, plus
``[workspace.package].version`` itself, and fails unless all of them are
strictly greater (semver ``major.minor.patch``) than on the base ref. Crates
using ``version.workspace = true`` report the inherited workspace version, so a
crate that stops inheriting cannot quietly pin a stale version.

Internal ``ascii-rat-*`` dependencies that declare a ``version`` (needed once
the crates are published to crates.io) must also match the workspace version,
so a release cannot go out requiring a stale sibling crate.

Usage:
    check_version_bump.py [--base <git ref>]

The base ref defaults to ``origin/$GITHUB_BASE_REF``, or ``origin/main`` when
that is not set.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import tomllib

ROOT_MANIFEST = "Cargo.toml"
WORKSPACE_KEY = "[workspace.package]"
INTERNAL_CRATE_PREFIX = "ascii-rat"
DEPENDENCY_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")


def read_manifest(path: str, ref: str | None) -> dict | None:
    """Parse a manifest from a git ref, or from the working tree if ref is None.

    Returns None when the manifest does not exist on that ref.
    """
    if ref is None:
        try:
            with open(path, "rb") as handle:
                content = handle.read()
        except FileNotFoundError:
            return None
    else:
        result = subprocess.run(
            ["git", "show", f"{ref}:{path}"],
            capture_output=True,
        )
        if result.returncode != 0:
            return None
        content = result.stdout

    try:
        return tomllib.loads(content.decode())
    except (UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        where = f"{ref}:{path}" if ref else path
        sys.exit(f"::error::Failed to parse {where}: {error}")


def member_paths(root: dict, ref: str | None) -> list[str]:
    """Return the workspace member directories listed in the root manifest."""
    members = root.get("workspace", {}).get("members", [])
    for member in members:
        if "*" in member:
            sys.exit(
                f"::error::Workspace member glob '{member}' is not supported by "
                "check_version_bump.py - list members explicitly or teach the "
                "script to expand globs."
            )
    if not members:
        where = f"{ref}:{ROOT_MANIFEST}" if ref else ROOT_MANIFEST
        sys.exit(f"::error::No [workspace] members found in {where}")
    return members


def collect_versions(ref: str | None) -> dict[str, str]:
    """Map a human readable label to every version declared on this ref."""
    root = read_manifest(ROOT_MANIFEST, ref)
    if root is None:
        where = f"{ref}:{ROOT_MANIFEST}" if ref else ROOT_MANIFEST
        sys.exit(f"::error::Could not read {where}")

    versions: dict[str, tuple[str, str]] = {}

    workspace_version = root.get("workspace", {}).get("package", {}).get("version")
    if not isinstance(workspace_version, str):
        sys.exit(
            "::error::Could not find [workspace.package].version in the root "
            "Cargo.toml"
        )
    versions[WORKSPACE_KEY] = (
        workspace_version,
        f"[workspace.package].version in {ROOT_MANIFEST}",
    )

    for member in member_paths(root, ref):
        manifest_path = f"{member}/{ROOT_MANIFEST}"
        manifest = read_manifest(manifest_path, ref)
        if manifest is None:
            continue
        package = manifest.get("package", {})
        name = package.get("name", member)
        version = package.get("version")
        # A string is an explicit version; `version.workspace = true` parses to
        # a table and means the crate inherits the workspace version. Either
        # way we record the crate's *effective* version, so a crate that stops
        # inheriting cannot quietly pin a stale version.
        if isinstance(version, str):
            versions[name] = (version, f"[package].version in {manifest_path}")
        else:
            versions[name] = (
                workspace_version,
                f"inherited from [workspace.package] ({manifest_path})",
            )

    return versions


def collect_internal_dependency_versions(ref: str | None) -> dict[str, str]:
    """Map a label to each declared version of an internal ascii-rat crate."""
    root = read_manifest(ROOT_MANIFEST, ref)
    if root is None:
        return {}

    found: dict[str, str] = {}

    def scan(table: dict, label_prefix: str) -> None:
        for name, spec in table.items():
            if not name.startswith(INTERNAL_CRATE_PREFIX):
                continue
            version = spec if isinstance(spec, str) else None
            if isinstance(spec, dict):
                version = spec.get("version")
            if isinstance(version, str):
                found[f"{label_prefix}.{name}"] = version

    scan(
        root.get("workspace", {}).get("dependencies", {}),
        f"[workspace.dependencies] ({ROOT_MANIFEST})",
    )

    for member in member_paths(root, ref):
        manifest_path = f"{member}/{ROOT_MANIFEST}"
        manifest = read_manifest(manifest_path, ref)
        if manifest is None:
            continue
        for table_name in DEPENDENCY_TABLES:
            scan(manifest.get(table_name, {}), f"[{table_name}] ({manifest_path})")

    return found


def parse_semver(version: str) -> tuple[int, int, int]:
    """Return (major, minor, patch), ignoring pre-release and build metadata."""
    match = re.match(r"^(\d+)\.(\d+)\.(\d+)", version)
    if not match:
        sys.exit(f"::error::Invalid semver format: {version}")
    return int(match.group(1)), int(match.group(2)), int(match.group(3))


def is_bumped(old_version: str, new_version: str) -> bool:
    """True when new_version is a strict semver increase over old_version."""
    return parse_semver(new_version) > parse_semver(old_version)


def check_lockstep(versions: dict[str, tuple[str, str]]) -> list[str]:
    """Internal dependency versions must equal the workspace version."""
    workspace_version = versions[WORKSPACE_KEY][0]
    problems = []
    for label, version in collect_internal_dependency_versions(None).items():
        if version != workspace_version:
            problems.append(
                f"{label}: declares {version}, workspace version is "
                f"{workspace_version}"
            )
    return problems


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--base",
        default=f"origin/{os.environ.get('GITHUB_BASE_REF', 'main')}",
        help="git ref to compare against (default: origin/$GITHUB_BASE_REF)",
    )
    args = parser.parse_args()

    old_versions = collect_versions(args.base)
    new_versions = collect_versions(None)

    print("::group::effective versions")
    for key in sorted(new_versions.keys() | old_versions.keys()):
        old = old_versions.get(key, ("-", ""))[0]
        new, source = new_versions.get(key, ("-", "gone on this branch"))
        print(f"{key}: {old} -> {new}  ({source})")
    print("::endgroup::")

    print("::group::check internal dependency lockstep")
    lockstep_problems = check_lockstep(new_versions)
    for problem in lockstep_problems:
        print(problem)
    if not lockstep_problems:
        print("✅ Internal ascii-rat dependency versions are in lockstep.")
    print("::endgroup::")

    print("::group::compare versions using semver")
    stale = []
    for key, (new_version, source) in new_versions.items():
        if key not in old_versions:
            print(f"➕ {key}: new on this branch ({new_version}), no bump needed.")
            continue
        old_version = old_versions[key][0]
        if is_bumped(old_version, new_version):
            print(f"✅ {key}: {old_version} -> {new_version}")
        else:
            stale.append(
                f"{key}: still {new_version} (base has {old_version}) - {source}"
            )
    print("::endgroup::")

    if lockstep_problems or stale:
        lines = [
            "::error::Rust sources changed but versions are not release ready!%0A"
        ]
        if stale:
            lines.append("Versions that were not increased:%0A")
            lines += [f"- {problem}%0A" for problem in stale]
            lines.append(
                "%0AEvery version in the workspace must be bumped when any .rs "
                "file changes.%0AAll crates inherit "
                "`version.workspace = true`, so bumping "
                "[workspace.package].version in the root Cargo.toml normally "
                "covers them all.%0A"
            )
        if lockstep_problems:
            lines.append("%0AInternal dependency versions out of lockstep:%0A")
            lines += [f"- {problem}%0A" for problem in lockstep_problems]
            lines.append(
                "%0AInternal ascii-rat dependencies that declare a version must "
                "match [workspace.package].version.%0A"
            )
        print("".join(lines))
        sys.exit(1)

    print("✅ All workspace versions were bumped.")


if __name__ == "__main__":
    main()
