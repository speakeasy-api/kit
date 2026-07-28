#!/usr/bin/env python3
"""Build two isolated release binaries and verify that they are byte-identical."""

import filecmp
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import sys
import tomllib
from datetime import datetime, timezone


ROOT = Path(__file__).resolve().parents[1]
EVIDENCE = ROOT / ".evidence-tmp"
WORK = EVIDENCE / "reproducible-build-work"
SOURCE_A = WORK / "repro-src-a" / "src"
SOURCE_B = WORK / "repro-src-b" / "src"
TARGET_A = WORK / "repro-src-a" / "target"
TARGET_B = WORK / "repro-src-b" / "target"
CARGO_HOME = WORK / "cargo-home"
HOME = WORK / "home"
ENVIRONMENT_FILE = EVIDENCE / "repro-environment.json"
ARTIFACT_FILE = EVIDENCE / "reproducible-build.json"
CLOSURE_FILE = EVIDENCE / "repro-input-closure.json"
FETCH_TIMEOUT = 300
BUILD_TIMEOUT = 600
PROBE_TIMEOUT = 30


def stage(message):
    print(f"stage={message}", flush=True)


def run(command, *, cwd, env=None, timeout, capture=False):
    process = subprocess.Popen(
        command,
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.STDOUT if capture else None,
        text=capture,
        start_new_session=True,
    )
    try:
        output, _ = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired as error:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        raise RuntimeError(
            f"command timed out after {timeout}s: {' '.join(command)}"
        ) from error
    if process.returncode:
        detail = f"\n{output.rstrip()}" if output else ""
        raise RuntimeError(
            f"command failed with exit {process.returncode}: {' '.join(command)}{detail}"
        )
    return output.strip() if output else ""


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def canonical_digest(value):
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def build_input_closure(source_root, env, *, recorded_post_run=False):
    rustc = run(
        ["rustc", "-vV"], cwd=source_root, env=env, timeout=PROBE_TIMEOUT, capture=True
    )
    host = next(
        (line.split(":", 1)[1].strip() for line in rustc.splitlines() if line.startswith("host:")),
        None,
    )
    if not host:
        raise RuntimeError("rustc -vV did not report a host target")
    metadata = json.loads(
        run(
            [
                "cargo", "metadata", "--locked", "--offline", "--format-version", "1",
                "--filter-platform", host,
            ],
            cwd=source_root,
            env=env,
            timeout=FETCH_TIMEOUT,
            capture=True,
        )
    )
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    pending = list(metadata["workspace_default_members"])
    release_ids = set()
    while pending:
        package_id = pending.pop()
        if package_id in release_ids:
            continue
        release_ids.add(package_id)
        for dependency in nodes[package_id]["deps"]:
            if any(kind["kind"] != "dev" for kind in dependency["dep_kinds"]):
                pending.append(dependency["pkg"])

    lock_packages = tomllib.loads((source_root / "Cargo.lock").read_text(encoding="utf-8"))["package"]
    lock_by_package = {
        (item["name"], item["version"], item.get("source")): item for item in lock_packages
    }
    local_paths = {
        source_root / "Cargo.lock",
        source_root / "rust-toolchain.toml",
    }
    release_packages = []
    build_scripts = []
    for package_id in sorted(release_ids):
        package = packages[package_id]
        source = package["source"]
        manifest = Path(package["manifest_path"])
        scripts = [
            Path(target["src_path"])
            for target in package["targets"]
            if "custom-build" in target["kind"]
        ]
        if source is None:
            try:
                manifest_relative = manifest.relative_to(source_root).as_posix()
            except ValueError as error:
                raise RuntimeError(f"path dependency is outside source root: {manifest}") from error
            local_paths.add(manifest)
            for target in package["targets"]:
                if set(target["kind"]) & {"lib", "bin"}:
                    target_root = Path(target["src_path"]).parent
                    local_paths.update(path for path in target_root.rglob("*") if path.is_file())
            local_paths.update(scripts)
            release_packages.append(
                {
                    "id": f"path:{manifest_relative}#{package['name']}@{package['version']}",
                    "source": "path",
                }
            )
            package_root = manifest.parent
            build_scripts.extend(
                {
                    "package": f"path:{manifest_relative}#{package['name']}@{package['version']}",
                    "path": script.relative_to(package_root).as_posix(),
                }
                for script in scripts
            )
        elif source.startswith("registry+"):
            lock = lock_by_package.get((package["name"], package["version"], source))
            checksum = lock.get("checksum") if lock else None
            if not checksum or not re.fullmatch(r"[0-9a-f]{64}", checksum):
                raise RuntimeError(f"registry package is not checksum-bound: {package_id}")
            release_packages.append(
                {
                    "id": f"{source}#{package['name']}@{package['version']}",
                    "sha256": checksum,
                    "source": "registry",
                }
            )
            build_scripts.extend(
                {
                    "package": f"{source}#{package['name']}@{package['version']}",
                    "path": script.relative_to(manifest.parent).as_posix(),
                }
                for script in scripts
            )
        else:
            raise RuntimeError(f"unsupported release dependency source: {source}")

    include_pattern = re.compile(r'\binclude(?:_bytes|_str)?!\s*\(\s*"([^"]+)"\s*\)')
    include_call = re.compile(r"\binclude(?:_bytes|_str)?!\s*\(")
    included_assets = set()
    for path in list(local_paths):
        if path.suffix != ".rs":
            continue
        text = path.read_text(encoding="utf-8")
        matches = include_pattern.findall(text)
        if len(matches) != len(include_call.findall(text)):
            raise RuntimeError(f"non-literal include macro cannot be closed statically: {path}")
        for value in matches:
            included = (path.parent / value).resolve()
            try:
                included.relative_to(source_root.resolve())
            except ValueError as error:
                raise RuntimeError(f"included asset is outside source root: {included}") from error
            if not included.is_file():
                raise RuntimeError(f"included asset does not exist: {included}")
            included_assets.add(included)
    local_paths.update(included_assets)

    entries = []
    for path in sorted(local_paths):
        relative = path.relative_to(source_root).as_posix()
        stat_result = path.stat()
        entries.append(
            {
                "mtime_utc": datetime.fromtimestamp(
                    stat_result.st_mtime, timezone.utc
                ).isoformat().replace("+00:00", "Z"),
                "path": relative,
                "sha256": sha256(path),
                "size": stat_result.st_size,
            }
        )
    entries.extend(
        {
            "path": f"registry/{package['id'].rsplit('#', 1)[1]}.crate",
            "sha256": package["sha256"],
        }
        for package in release_packages
        if package["source"] == "registry"
    )
    entries.sort(key=lambda entry: entry["path"])
    manifest = [{"path": entry["path"], "sha256": entry["sha256"]} for entry in entries]
    return {
        "build_scripts": sorted(build_scripts, key=lambda item: (item["package"], item["path"])),
        "closure_manifest_recorded_post_run": recorded_post_run,
        "entries": entries,
        "included_assets": sorted(path.relative_to(source_root).as_posix() for path in included_assets),
        "release_packages": sorted(release_packages, key=lambda item: item["id"]),
        "schema_version": 1,
        "sha256": canonical_digest(manifest),
        "target": host,
        "type": "cargo_release_build_input_closure",
    }


def clean_work():
    if WORK.exists():
        shutil.rmtree(WORK)


def main():
    stage("remove-stale-work")
    clean_work()
    stage("create-evidence-directory")
    EVIDENCE.mkdir(exist_ok=True)
    stage("remove-stale-evidence")
    for path in (ENVIRONMENT_FILE, ARTIFACT_FILE, CLOSURE_FILE):
        path.unlink(missing_ok=True)

    try:
        ignored = shutil.ignore_patterns(
            ".git", ".evidence-tmp", "target", "__pycache__", "*.pyc", ".DS_Store"
        )
        stage("copy-source-a")
        shutil.copytree(ROOT, SOURCE_A, symlinks=True, ignore=ignored)
        stage("copy-source-b")
        shutil.copytree(ROOT, SOURCE_B, symlinks=True, ignore=ignored)
        stage("create-shared-cargo-cache")
        CARGO_HOME.mkdir(parents=True)
        stage("create-isolated-home")
        HOME.mkdir()
        stage("create-target-a")
        TARGET_A.mkdir()
        stage("create-target-b")
        TARGET_B.mkdir()

        remap_flags = [
            f"--remap-path-prefix={SOURCE_A}=/workspace",
            f"--remap-path-prefix={SOURCE_B}=/workspace",
            f"--remap-path-prefix={TARGET_A}=/target",
            f"--remap-path-prefix={TARGET_B}=/target",
            f"--remap-path-prefix={CARGO_HOME}=/cargo-home",
            f"--remap-path-prefix={ROOT}=/workspace",
            "-C",
            "codegen-units=1",
        ]
        rustup_home = os.environ.get("RUSTUP_HOME", str(Path.home() / ".rustup"))
        common_env = {
            "CARGO_ENCODED_RUSTFLAGS": "\x1f".join(remap_flags),
            "CARGO_HOME": str(CARGO_HOME),
            "CARGO_INCREMENTAL": "0",
            "CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "1",
            "HOME": str(HOME),
            "LANG": "C",
            "LC_ALL": "C",
            "PATH": os.environ["PATH"],
            "RUSTUP_HOME": rustup_home,
            "SOURCE_DATE_EPOCH": "0",
            "TZ": "UTC",
        }

        stage("probe-rustc-version")
        rustc_version = run(
            ["rustc", "--version"],
            cwd=ROOT,
            env=common_env,
            timeout=PROBE_TIMEOUT,
            capture=True,
        )
        stage("probe-cargo-version")
        cargo_version = run(
            ["cargo", "--version"],
            cwd=ROOT,
            env=common_env,
            timeout=PROBE_TIMEOUT,
            capture=True,
        )
        stage("digest-cargo-lock")
        lock_digest = sha256(ROOT / "Cargo.lock")
        environment = {
            "cargo_lock_sha256": lock_digest,
            "platform": f"{platform.system()} {platform.release()} {platform.machine()}",
            "remap_flags": remap_flags,
            "schema_version": 1,
            "timeouts_seconds": {
                "build": BUILD_TIMEOUT,
                "fetch": FETCH_TIMEOUT,
                "probe": PROBE_TIMEOUT,
            },
            "tools": {"cargo": cargo_version, "rustc": rustc_version},
            "variables": {
                key: common_env[key]
                for key in (
                    "CARGO_INCREMENTAL",
                    "CARGO_PROFILE_RELEASE_CODEGEN_UNITS",
                    "LANG",
                    "LC_ALL",
                    "SOURCE_DATE_EPOCH",
                    "TZ",
                )
            },
        }
        environment_digest = canonical_digest(environment)
        environment_record = {
            "environment": environment,
            "sha256": environment_digest,
            "type": "reproducible_build_environment",
        }
        stage("write-environment-digest")
        write_json(ENVIRONMENT_FILE, environment_record)
        print(json.dumps(environment_record, sort_keys=True), flush=True)

        stage("cargo-fetch-locked")
        run(
            ["cargo", "fetch", "--locked"],
            cwd=SOURCE_A,
            env=common_env,
            timeout=FETCH_TIMEOUT,
        )
        stage("record-build-input-closure")
        closure = build_input_closure(SOURCE_A, common_env)
        closure_b = build_input_closure(SOURCE_B, common_env)
        if closure["sha256"] != closure_b["sha256"]:
            raise RuntimeError("source copies have different build-input closures")
        write_json(CLOSURE_FILE, closure)
        build_env_a = common_env | {
            "CARGO_NET_OFFLINE": "true",
            "CARGO_TARGET_DIR": str(TARGET_A),
        }
        build_env_b = common_env | {
            "CARGO_NET_OFFLINE": "true",
            "CARGO_TARGET_DIR": str(TARGET_B),
        }
        command = ["cargo", "build", "--locked", "--offline", "--release"]
        stage("cargo-build-a-offline")
        run(command, cwd=SOURCE_A, env=build_env_a, timeout=BUILD_TIMEOUT)
        stage("cargo-build-b-offline")
        run(command, cwd=SOURCE_B, env=build_env_b, timeout=BUILD_TIMEOUT)

        binary_a = TARGET_A / "release" / "kit"
        binary_b = TARGET_B / "release" / "kit"
        stage("digest-binary-a")
        digest_a = sha256(binary_a)
        stage("digest-binary-b")
        digest_b = sha256(binary_b)
        stage("compare-binary-bytes")
        if not filecmp.cmp(binary_a, binary_b, shallow=False):
            raise RuntimeError(
                f"release binaries differ: source_a={digest_a} source_b={digest_b}"
            )
        artifact_record = {
            "binaries": {
                "source_a_sha256": digest_a,
                "source_b_sha256": digest_b,
            },
            "byte_identical": True,
            "build_input_closure_sha256": closure["sha256"],
            "environment_sha256": environment_digest,
            "schema_version": 1,
            "type": "reproducible_build_artifact",
        }
        stage("write-artifact-digests")
        write_json(ARTIFACT_FILE, artifact_record)
        print(json.dumps(artifact_record, sort_keys=True), flush=True)
        return 0
    finally:
        stage("cleanup-transient-work")
        clean_work()


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (KeyError, OSError, RuntimeError) as error:
        print(
            json.dumps({"error": str(error), "type": "reproducible_build_error"}),
            file=sys.stderr,
        )
        sys.exit(1)
