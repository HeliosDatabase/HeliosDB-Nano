#!/usr/bin/env python3
"""Nonpublishing source29 native binary gate. Standard library only; never repairs source."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import struct
import sys
import tarfile
import traceback
import zipfile


ROOT = Path(__file__).resolve().parents[2]
EXPECTED = ROOT / ".github/binary-preflight-source28.json"
OUTPUT = ROOT / ".preflight-out"


def digest(data):
    return hashlib.sha256(data).hexdigest()


def file_hash(path):
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for part in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(part)
    return result.hexdigest()


def canonical(value):
    return digest(json.dumps(value, sort_keys=True, separators=(",", ":")).encode())


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def run(args, **kwargs):
    return subprocess.run(args, cwd=ROOT, check=True, capture_output=True, **kwargs).stdout


def git(*args):
    return run(["git", *args]).decode("utf-8").strip()


def model():
    wanted = os.environ.get("PREFLIGHT_EXPECTED_SHA256", "")
    helper = os.environ.get("PREFLIGHT_HELPER_SHA256", "")
    if not wanted or file_hash(EXPECTED) != wanted:
        raise ValueError("expected-source manifest does not match workflow pin")
    if not helper or file_hash(Path(__file__)) != helper:
        raise ValueError("preflight helper does not match workflow pin")
    value = json.loads(EXPECTED.read_text(encoding="utf-8"))
    if value["schema"] != 1 or value["version"] != "4.42.0":
        raise ValueError("wrong source29 manifest schema/version")
    return value


def source_check(expected, destination):
    if git("config", "--get", "core.autocrlf").lower() != "false":
        raise ValueError("core.autocrlf must be false before checkout")
    tree = {}
    for record in run(["git", "ls-tree", "-rz", "--full-tree", "HEAD"]).split(b"\0"):
        if not record:
            continue
        metadata, name = record.split(b"\t", 1)
        mode, kind, oid = metadata.decode().split()
        if kind != "blob":
            raise ValueError("submodule/non-blob input is not allowed")
        tree[name.decode("utf-8")] = {"mode": mode, "oid": oid}
    extras = set(expected["allowed_extra_committed_paths"])
    if set(tree) != set(expected["entries"]) | extras:
        raise ValueError("committed tree has missing or unexpected paths")
    actual_physical = {}
    symlinks = {}
    for name, entry in expected["entries"].items():
        if tree[name] != {key: entry[key] for key in ("mode", "oid")}:
            raise ValueError("Git mode/blob differs from frozen source29: " + name)
        path = ROOT / name
        if entry["mode"] == "120000":
            target = expected["known_host_bound_symlinks"].get(name)
            if target is None or target != entry["target_text"]:
                raise ValueError("unlisted symlink exception: " + name)
            if path.is_symlink():
                if os.readlink(path) != target:
                    raise ValueError("symlink target changed: " + name)
                representation = "symlink"
            elif os.name == "nt" and path.is_file() and path.read_bytes() == target.encode():
                representation = "Windows Git symlink-text checkout"
            else:
                raise ValueError("missing/unexpected symlink representation: " + name)
            symlinks[name] = {"mode": "120000", "target_text": target, "representation": representation}
        else:
            if path.is_symlink() or not path.is_file() or file_hash(path) != entry["sha256"]:
                raise ValueError("regular source/config/asset bytes changed: " + name)
            actual_physical[name] = entry["sha256"]
    for name in extras:
        path = ROOT / name
        if tree[name]["mode"] != "100644" or path.is_symlink() or not path.is_file():
            raise ValueError("preflight additions must be regular 100644 files: " + name)
    if git("diff", "--name-only", "HEAD", "--"):
        raise ValueError("tracked checkout differs from HEAD")
    for name in run(["git", "ls-files", "--others", "--exclude-standard", "-z"]).split(b"\0"):
        if name and not name.decode().startswith((".preflight-out/", ".preflight-download/", ".preflight-summary/", "target/")):
            raise ValueError("unexpected untracked source input: " + name.decode())
    head = git("rev-parse", "HEAD")
    if head != os.environ["GITHUB_SHA"] or git("rev-parse", "--show-object-format") != "sha1":
        raise ValueError("checkout must be the exact event SHA in this SHA1 repository")
    result = {
        "success": True,
        "head": head,
        "tree_oid": git("rev-parse", "HEAD^{tree}"),
        "expected_manifest_sha256": file_hash(EXPECTED),
        "helper_sha256": file_hash(Path(__file__)),
        "git_entries": tree,
        "git_entries_digest": canonical(tree),
        "regular_file_count": len(actual_physical),
        "physical_sha256_digest": canonical(actual_physical),
        "source_input_count": len(expected["entries"]),
        "known_symlinks": symlinks,
    }
    write_json(destination, result)
    return result


def binary_name(target):
    return "heliosdb-nano.exe" if "windows" in target else "heliosdb-nano"


def archive_name(target, expected):
    return "heliosdb-nano-v4.42.0-" + target + "." + expected["targets"][target]["archive"]


def pack(binary, target, expected, destination):
    files = {binary_name(target): binary, "LICENSE": ROOT / "LICENSE", "README.md": ROOT / "README.md"}
    path = destination / archive_name(target, expected)
    if path.suffix == ".zip":
        with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
            for name, source in files.items():
                archive.write(source, name)
    else:
        with tarfile.open(path, "w:gz") as archive:
            for name, source in files.items():
                archive.add(source, arcname=name, recursive=False)
    return path



def macho_info(data):
    if len(data) < 32 or data[:4] != b"\xcf\xfa\xed\xfe":
        raise ValueError("expected a thin little-endian Mach-O64 executable")
    _, cpu, _, kind, count, command_bytes, _, _ = struct.unpack_from("<8I", data)
    if cpu != 0x100000C or kind != 2 or command_bytes > len(data) - 32:
        raise ValueError("expected bounded ARM64 Mach-O executable commands")
    found = []
    offset = 32
    for _ in range(count):
        if offset + 8 > 32 + command_bytes:
            raise ValueError("truncated Mach-O load command")
        command, size = struct.unpack_from("<2I", data, offset)
        if size < 8 or offset + size > 32 + command_bytes:
            raise ValueError("invalid Mach-O load command size")
        if command == 0x32:
            if size < 24:
                raise ValueError("truncated LC_BUILD_VERSION")
            _, _, system, minimum, sdk, tools = struct.unpack_from("<6I", data, offset)
            if system != 1 or size < 24 + tools * 8:
                raise ValueError("not a macOS LC_BUILD_VERSION")
            decode = lambda value: [value >> 16, (value >> 8) & 255, value & 255]
            found.append({"cpu": "arm64", "command": "LC_BUILD_VERSION", "minimum": decode(minimum), "sdk": decode(sdk)})
        offset += size
    if offset != 32 + command_bytes or len(found) != 1:
        raise ValueError("need exactly one complete macOS build-version command")
    return found[0]


def check_macos_receipt(receipt, data, target_model):
    info = macho_info(data)
    proof = receipt["macos_deployment"]
    if proof["environment"] != target_model["deployment_target"] or proof["macho"] != info:
        raise ValueError("macOS deployment environment/actual Mach-O mismatch")
    if tuple(info["minimum"]) > tuple(target_model["maximum_minos"]):
        raise ValueError("macOS minimum requirement exceeds verified released binary")
    match = re.search(r"\bminos\s+([0-9.]+)", proof["vtool_stdout"])
    version = [int(part) for part in match[1].split(".")] if match else []
    version += [0] * (3 - len(version))
    if version != info["minimum"]:
        raise ValueError("vtool and actual Mach-O disagree")
    return info


def build(target):
    expected = model()
    if target not in expected["targets"]:
        raise ValueError("unexpected matrix target")
    directory = OUTPUT / target
    directory.mkdir(parents=True, exist_ok=True)
    receipt = {"schema": 1, "success": False, "target": target, "macos_deployment": None}
    try:
        before = source_check(expected, directory / "source-before.json")
        import tomllib
        manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
        if manifest["package"]["version"] != expected["version"] or manifest["features"]["default"] != expected["default_features"]:
            raise ValueError("package version/default feature set differs from frozen manifest")
        flag_names = [
            key for key in os.environ if (
                key in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS", "CARGO_BUILD_TARGET", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER")
                or key.startswith("CARGO_PROFILE_RELEASE_")
                or (key.startswith("CARGO_TARGET_") and key.endswith("_RUSTFLAGS"))
            )
        ]
        overrides = {key: os.environ[key] for key in flag_names if os.environ[key]}
        if overrides:
            raise ValueError("unexpected Rust build override names: " + ",".join(sorted(overrides)))
        compiler = run(["rustc", "-vV"]).decode()
        cargo = run(["cargo", "-vV"]).decode()
        if ("host: " + target) not in compiler.splitlines():
            raise ValueError("target must execute on its native host: " + compiler)
        compiler_path = Path(run(["rustup", "which", "rustc"]).decode().strip())
        cargo_path = Path(run(["rustup", "which", "cargo"]).decode().strip())
        command = ["cargo", "build", "--release", "--locked", "--target", target]
        receipt.update({
            "head": before["head"], "tree_oid": before["tree_oid"],
            "expected_manifest_sha256": file_hash(EXPECTED), "helper_sha256": file_hash(Path(__file__)),
            "default_features": expected["default_features"], "package_version": expected["version"],
            "build_command": command, "build_overrides": overrides,
            "compiler": compiler, "compiler_sha256": file_hash(compiler_path),
            "cargo": cargo, "cargo_sha256": file_hash(cargo_path),
            "host": platform.uname()._asdict(),
            "github": {key: os.environ.get(key) for key in ("GITHUB_SHA", "GITHUB_REPOSITORY", "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_REF", "GITHUB_WORKFLOW_REF", "GITHUB_WORKFLOW_SHA")},
        })
        with (directory / "build.log").open("wb") as log:
            completed = subprocess.run(command, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT)
        receipt["build_exit"] = completed.returncode
        if completed.returncode:
            raise ValueError("native release build failed; see build.log")
        binary = ROOT / "target" / target / "release" / binary_name(target)
        if binary.is_symlink() or not binary.is_file():
            raise ValueError("missing regular native binary")
        completed = subprocess.run([str(binary), "--version"], cwd=ROOT, capture_output=True, text=True, timeout=30)
        receipt["native_version"] = {"exit": completed.returncode, "stdout": completed.stdout, "stderr": completed.stderr}
        if completed.returncode or not re.fullmatch(r"heliosdb-nano 4\.42\.0\s*", completed.stdout):
            raise ValueError("native binary version/loader check failed")
        if target == "aarch64-apple-darwin":
            receipt["macos_deployment"] = {
                "environment": os.environ.get("MACOSX_DEPLOYMENT_TARGET"),
                "macho": macho_info(binary.read_bytes()),
                "vtool_stdout": run(["xcrun", "vtool", "-show-build", str(binary)], timeout=30).decode(),
            }
            check_macos_receipt(receipt, binary.read_bytes(), expected["targets"][target])
        after = source_check(expected, directory / "source-after.json")
        if before != after:
            raise ValueError("source identity changed during build")
        archive = pack(binary, target, expected, directory)
        receipt.update({"binary_name": binary.name, "binary_sha256": file_hash(binary), "binary_bytes": binary.stat().st_size,
                        "archive": archive.name, "archive_sha256": file_hash(archive), "archive_bytes": archive.stat().st_size,
                        "source_before_sha256": file_hash(directory / "source-before.json"), "source_after_sha256": file_hash(directory / "source-after.json"),
                        "source_digest": before["physical_sha256_digest"], "git_entries_digest": before["git_entries_digest"], "success": True})
    except Exception as error:
        receipt["error"] = str(error)
        traceback.print_exc()
    finally:
        write_json(directory / "receipt.json", receipt)
    if not receipt["success"]:
        raise SystemExit(1)
    print("NATIVE_BINARY_PREFLIGHT_PASS", target, receipt["binary_sha256"])


def member_hashes(path):
    result = {}
    if path.suffix == ".zip":
        with zipfile.ZipFile(path) as archive:
            for member in archive.infolist():
                if member.is_dir() or member.filename in result:
                    raise ValueError("duplicate/directory archive member")
                result[member.filename] = digest(archive.read(member))
    else:
        with tarfile.open(path, "r:gz") as archive:
            for member in archive.getmembers():
                if not member.isfile() or member.name in result:
                    raise ValueError("nonregular/duplicate archive member")
                result[member.name] = digest(archive.extractfile(member).read())
    return result


def aggregate(root, destination, matrix_result):
    expected = model()
    if matrix_result != "success":
        raise ValueError("all four native matrix jobs must succeed: " + matrix_result)
    source = source_check(expected, destination / "aggregate-source.json")
    wanted = {"binary-preflight-" + target for target in expected["targets"]}
    if {path.name for path in root.iterdir()} != wanted:
        raise ValueError("expected exactly four target artifact directories")
    summaries = []
    for target in expected["targets"]:
        directory = root / ("binary-preflight-" + target)
        receipt = json.loads((directory / "receipt.json").read_text(encoding="utf-8"))
        if receipt.get("success") is not True or receipt["target"] != target or receipt["schema"] != 1:
            raise ValueError("incomplete target receipt: " + target)
        for key in ("head", "tree_oid", "expected_manifest_sha256", "helper_sha256", "git_entries_digest"):
            if receipt[key] != source[key]:
                raise ValueError("mixed source/helper identities: " + target + "/" + key)
        if receipt["source_digest"] != source["physical_sha256_digest"]:
            raise ValueError("physical source digest mismatch")
        if receipt["default_features"] != expected["default_features"] or receipt["build_command"] != ["cargo", "build", "--release", "--locked", "--target", target] or receipt["build_overrides"]:
            raise ValueError("feature/build flag drift")
        if receipt["package_version"] != expected["version"] or not receipt["cargo"].startswith("cargo "):
            raise ValueError("missing package/compiler receipt")
        for key in ("compiler_sha256", "cargo_sha256", "binary_sha256"):
            if not re.fullmatch(r"[a-f0-9]{64}", receipt[key]):
                raise ValueError("invalid compiler/binary identity: " + key)
        if receipt["github"]["GITHUB_SHA"] != source["head"] or receipt["github"]["GITHUB_RUN_ID"] != os.environ["GITHUB_RUN_ID"]:
            raise ValueError("receipt belongs to another source/run")
        for phase in ("before", "after"):
            report = directory / ("source-" + phase + ".json")
            if file_hash(report) != receipt["source_" + phase + "_sha256"]:
                raise ValueError("source report digest mismatch")
            observed = json.loads(report.read_text(encoding="utf-8"))
            if observed.get("success") is not True or observed["git_entries"] != source["git_entries"]:
                raise ValueError("incomplete source report or different Git tree")
            for key in ("head", "tree_oid", "expected_manifest_sha256", "helper_sha256", "git_entries_digest", "physical_sha256_digest", "regular_file_count", "source_input_count"):
                if observed[key] != source[key]:
                    raise ValueError("source report identity drift")
        archive = directory / archive_name(target, expected)
        allowed_files = {"receipt.json", "source-before.json", "source-after.json", "build.log", archive.name}
        if {path.name for path in directory.iterdir()} != allowed_files or any(path.is_symlink() or not path.is_file() for path in directory.iterdir()):
            raise ValueError("unexpected or incomplete target artifact contents")
        if receipt["archive"] != archive.name or file_hash(archive) != receipt["archive_sha256"] or archive.stat().st_size != receipt["archive_bytes"]:
            raise ValueError("archive identity mismatch")
        names = {binary_name(target), "LICENSE", "README.md"}
        members = member_hashes(archive)
        if set(members) != names or members[binary_name(target)] != receipt["binary_sha256"]:
            raise ValueError("binary/flat archive mismatch")
        if target == "aarch64-apple-darwin":
            with tarfile.open(archive, "r:gz") as packed:
                check_macos_receipt(receipt, packed.extractfile(binary_name(target)).read(), expected["targets"][target])
        elif receipt.get("macos_deployment") is not None:
            raise ValueError("macOS deployment setting leaked into another target")
        for name in ("LICENSE", "README.md"):
            if members[name] != expected["entries"][name]["sha256"]:
                raise ValueError("archive source asset mismatch")
        if receipt["build_exit"] or receipt["native_version"]["exit"] or not re.fullmatch(r"heliosdb-nano 4\.42\.0\s*", receipt["native_version"]["stdout"]):
            raise ValueError("native execution did not pass")
        if ("host: " + target) not in receipt["compiler"].splitlines():
            raise ValueError("nonnative compiler host")
        destination.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(archive, destination / archive.name)
        summaries.append({"target": target, "archive": archive.name, "sha256": receipt["archive_sha256"], "binary_sha256": receipt["binary_sha256"], "receipt_sha256": file_hash(directory / "receipt.json")})
    checksums = "".join(item["sha256"] + "  " + item["archive"] + "\n" for item in sorted(summaries, key=lambda item: item["archive"]))
    (destination / "SHA256SUMS").write_text(checksums, encoding="ascii")
    write_json(destination / "summary.json", {"success": True, "target_count": 4, "head": source["head"], "expected_manifest_sha256": source["expected_manifest_sha256"], "targets": summaries})
    print("ALL_FOUR_NATIVE_BINARIES_PASS", source["head"])


def main():
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("verify")
    build_parser = commands.add_parser("build")
    build_parser.add_argument("--target", required=True)
    aggregate_parser = commands.add_parser("aggregate")
    aggregate_parser.add_argument("--artifacts", type=Path, required=True)
    aggregate_parser.add_argument("--output", type=Path, required=True)
    aggregate_parser.add_argument("--matrix-result", required=True)
    arguments = parser.parse_args()
    if arguments.command == "build":
        build(arguments.target)
    elif arguments.command == "verify":
        result = source_check(model(), OUTPUT / "verify-source.json")
        print("EXACT_SOURCE28_INPUTS_VERIFIED", result["head"])
    else:
        aggregate(arguments.artifacts, arguments.output, arguments.matrix_result)


if __name__ == "__main__":
    main()
