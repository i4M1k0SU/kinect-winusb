"""Validate release versions and package only the intended distribution files."""

import argparse
import hashlib
import os
from pathlib import Path
import re
import shutil
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[2]
# Rust target -> folder in the archives and the binaries it carries. Only 64-bit
# Windows is supported: the x86 build of the proxy DLL serves 32-bit
# applications under WOW64, and the tool is 64-bit only (next to the x64 DLL,
# which its nuitest loads).
TARGETS = {
    "x86_64-pc-windows-msvc": ("x64", ("kinect10.dll", "kinect-winusb.exe")),
    "i686-pc-windows-msvc": ("x86", ("kinect10.dll",)),
}
# The debug information of each binary, shipped in a separate archive.
SYMBOLS = {"kinect10.dll": "kinect10.pdb", "kinect-winusb.exe": "kinect_winusb.pdb"}
DOCUMENTS = ("README.md", "README.ja.md", "LICENSE")


def validate_tag(version, tag):
    number = r"(?:0|[1-9][0-9]*)"
    identifier = r"[0-9A-Za-z-]+"
    pattern = rf"{number}\.{number}\.{number}(?:-{identifier}(?:\.{identifier})*)?(?:\+{identifier}(?:\.{identifier})*)?"
    if not re.fullmatch(pattern, version):
        raise ValueError(f"Invalid SemVer: {version}")
    prerelease = version.split("+", 1)[0].partition("-")[2]
    if any(part.isdigit() and len(part) > 1 and part.startswith("0") for part in prerelease.split(".")):
        raise ValueError(f"Invalid prerelease: {version}")
    if tag and tag != f"v{version}":
        raise ValueError(f"Tag {tag!r} must be v + Cargo version {version!r}")
    return bool(prerelease)


def version_info(root=ROOT):
    with (root / "Cargo.toml").open("rb") as file:
        manifest = tomllib.load(file)
    version = manifest["workspace"]["package"]["version"]
    validate_tag(version, "")
    # A tagged release must also build successfully with --locked.
    with (root / "Cargo.lock").open("rb") as file:
        locked = {p["name"]: p["version"] for p in tomllib.load(file)["package"] if "source" not in p}
    for member in manifest["workspace"]["members"]:
        with (root / member / "Cargo.toml").open("rb") as file:
            package = tomllib.load(file)["package"]
        if package["version"] != {"workspace": True} or locked.get(package["name"]) != version:
            raise ValueError(f"{member}: workspace / lockfile version must be {version}")
    return version


def archive_names(version):
    return (f"kinect-winusb-{version}.zip", f"kinect-winusb-{version}-symbols.zip")


def write_zip(archive, files):
    folder = archive.name.removesuffix(".zip")
    with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as bundle:
        for source, destination in files:
            bundle.write(source, f"{folder}/{destination}")


def package(root, output):
    version = version_info(root)
    binaries, symbols = [], []
    for target, (folder, names) in TARGETS.items():
        directory = root / "target" / target / "release"
        binaries += [(directory / name, f"{folder}/{name}") for name in names]
        symbols += [(directory / SYMBOLS[name], f"{folder}/{SYMBOLS[name]}") for name in names]
    binaries += [(root / name, name) for name in DOCUMENTS]
    for source, _ in binaries + symbols:
        if not source.is_file():
            raise FileNotFoundError(source)
    output.mkdir(parents=True, exist_ok=True)
    archives = [output / name for name in archive_names(version)]
    write_zip(archives[0], binaries)
    write_zip(archives[1], symbols)
    return archives


def collect(source, output, version):
    # Require every archive before publishing; avoid silently incomplete releases.
    archives = []
    for name in archive_names(version):
        matches = list(source.rglob(name))
        if len(matches) != 1 or not matches[0].is_file():
            raise ValueError(f"Expected exactly one artifact: {name}")
        archives.append(matches[0])
    output.mkdir(parents=True, exist_ok=True)
    checksums = []
    for archive in sorted(archives):
        destination = output / archive.name
        shutil.copyfile(archive, destination)
        with destination.open("rb") as file:
            digest = hashlib.file_digest(file, "sha256").hexdigest()
        checksums.append(f"{digest}  {destination.name}\n")
    (output / "SHA256SUMS").write_text("".join(checksums), encoding="utf-8")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("version")
    pack = commands.add_parser("package")
    pack.add_argument("--output", type=Path, default=Path("dist"))
    gather = commands.add_parser("collect")
    gather.add_argument("--source", type=Path, default=Path("artifacts"))
    gather.add_argument("--output", type=Path, default=Path("dist"))
    args = parser.parse_args()
    if args.command == "version":
        version = version_info()
        prerelease = validate_tag(version, os.environ.get("RELEASE_TAG", ""))
        result = f"version={version}\nprerelease={str(prerelease).lower()}\n"
        print(result, end="")
        if os.environ.get("GITHUB_OUTPUT"):
            with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as file:
                file.write(result)
    elif args.command == "package":
        for archive in package(ROOT, args.output):
            print(archive)
    else:
        collect(args.source, args.output, version_info())


if __name__ == "__main__":
    main()
