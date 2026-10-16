#!/usr/bin/env python3
"""Package native builds and check the executables extracted from the archive."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import zipfile


def check_binaries(directory, suffix, version):
    player = directory / f"plavno{suffix}"
    result = subprocess.run(
        [str(player), "--version"], check=True, capture_output=True, text=True, timeout=30
    )
    if result.stdout.strip() != f"plavno {version}":
        raise RuntimeError(f"Unexpected player version: {result.stdout.strip()}")
    subprocess.run([str(player), "--help"], check=True, capture_output=True, timeout=30)
    for name in ("plavno-spotify",):
        # Missing profile/method must fail locally, before contacting any service.
        result = subprocess.run(
            [str(directory / f"{name}{suffix}")],
            input="{}\n",
            check=True,
            capture_output=True,
            text=True,
            timeout=30,
        )
        response = json.loads(result.stdout)
        if not isinstance(response.get("error"), str) or not response["error"]:
            raise RuntimeError(f"{name} did not return a protocol error")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument(
        "--platform",
        required=True,
        choices=(
            "linux-x86_64-gnu",
            "linux-aarch64-gnu",
            "macos-aarch64",
            "macos-x86_64",
            "windows-x86_64-msvc",
        ),
    )
    parser.add_argument("--binary-dir", required=True, type=Path)
    args = parser.parse_args()
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", args.version):
        parser.error("invalid release version")

    root = Path(__file__).resolve().parent.parent
    output = root / "dist"
    output.mkdir(exist_ok=True)
    windows = args.platform.startswith("windows-")
    suffix = ".exe" if windows else ""
    extension = ".zip" if windows else ".tar.gz"
    archive = output / f"plavno-{args.version}-{args.platform}{extension}"
    with tempfile.TemporaryDirectory(prefix="plavno-release-") as temporary:
        stage = Path(temporary) / "package"
        stage.mkdir()
        for name in ("plavno", "plavno-spotify"):
            executable = stage / f"{name}{suffix}"
            shutil.copy2(args.binary_dir / executable.name, executable)
            executable.chmod(0o755)
        for name in ("README.md", "LICENSE"):
            shutil.copy2(root / name, stage / name)

        if windows:
            with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as package:
                for path in sorted(stage.iterdir()):
                    package.write(path, path.name)
        else:
            with tarfile.open(archive, "w:gz") as package:
                for path in sorted(stage.iterdir()):
                    package.add(path, arcname=path.name)

        extracted = Path(temporary) / "extracted"
        extracted.mkdir()
        expected = {path.name for path in stage.iterdir()}
        if windows:
            with zipfile.ZipFile(archive) as package:
                if set(package.namelist()) != expected:
                    raise RuntimeError("Unexpected archive contents")
                package.extractall(extracted)
        else:
            with tarfile.open(archive) as package:
                if set(package.getnames()) != expected:
                    raise RuntimeError("Unexpected archive contents")
                for name in expected:
                    with package.extractfile(name) as source:
                        (extracted / name).write_bytes(source.read())
                    (extracted / name).chmod((stage / name).stat().st_mode)
        check_binaries(extracted, suffix, args.version)

    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_name(archive.name + ".sha256").write_text(
        f"{digest}  {archive.name}\n", encoding="ascii"
    )
    print(f"Verified {archive.name} ({archive.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
