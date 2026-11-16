#!/usr/bin/env python3
"""Package the CLI for one platform: the archive the install scripts
download.

usage: scripts/package-cli.py <platform> <binary> <out-dir> [--version <v>]

<platform> is one of linux-x64, linux-arm64, darwin-x64, darwin-arm64,
windows-x64. <binary> is the built `unharnessed` (or `unharnessed.exe`).
The version defaults to the workspace version in Cargo.toml.

Writes <out-dir>/unharnessed-<platform>.tar.gz (a .zip for Windows) and
<out-dir>/unharnessed-<platform>.<ext>.sha256. Inside, one folder:

  unharnessed/
    unharnessed[.exe]       the binary, and beside it what it looks for there:
    package.json            the version manifest
    unharnessed-runtime/    the kernel's Python package
    skills/                 the built-in skills
    README.md LICENSE

The same layout as the VS Code extension's bin/ (vscode/scripts/package.mjs);
of the runtime and the skills, the files git tracks are shipped.
"""
import argparse
import hashlib
import io
import json
import subprocess
import sys
import tarfile
import time
import tomllib
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PLATFORMS = ["linux-x64", "linux-arm64", "darwin-x64", "darwin-arm64", "windows-x64"]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("platform", choices=PLATFORMS)
    parser.add_argument("binary", type=Path)
    parser.add_argument("out", type=Path)
    parser.add_argument("--version")
    args = parser.parse_args()

    version = args.version or tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    windows = args.platform.startswith("windows")
    exe = "unharnessed.exe" if windows else "unharnessed"
    if not args.binary.is_file():
        sys.exit(f"no binary at {args.binary}")

    manifest = json.dumps({"name": "unharnessed", "version": version,
                           "piConfig": {"name": "unharnessed", "configDir": ".unharnessed"}}, indent=2) + "\n"
    tracked = subprocess.run(["git", "ls-files", "-z", "--", "unharnessed-runtime/pyproject.toml",
                              "unharnessed-runtime/src", "skills"],
                             cwd=ROOT, check=True, capture_output=True, text=True).stdout.split("\0")
    files = [(ROOT / path, path) for path in tracked if path]
    files += [(ROOT / "README.md", "README.md"), (ROOT / "LICENSE", "LICENSE")]
    if not any(name == "unharnessed-runtime/pyproject.toml" for _, name in files):
        sys.exit("unharnessed-runtime/pyproject.toml is not tracked")

    args.out.mkdir(parents=True, exist_ok=True)
    archive = args.out / f"unharnessed-{args.platform}.{'zip' if windows else 'tar.gz'}"
    if windows:
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as out:
            out.write(args.binary, f"unharnessed/{exe}")
            out.writestr("unharnessed/package.json", manifest)
            for source, name in files:
                out.write(source, f"unharnessed/{name}")
    else:
        with tarfile.open(archive, "w:gz") as out:
            def add(source, name, mode):
                info = out.gettarinfo(str(source), f"unharnessed/{name}")
                info.mode, info.uid, info.gid, info.uname, info.gname = mode, 0, 0, "", ""
                with open(source, "rb") as handle:
                    out.addfile(info, handle)
            add(args.binary, exe, 0o755)
            info = tarfile.TarInfo("unharnessed/package.json")
            data = manifest.encode()
            info.size, info.mode, info.mtime = len(data), 0o644, int(time.time())
            out.addfile(info, io.BytesIO(data))
            for source, name in files:
                add(source, name, 0o644)

    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    (args.out / f"{archive.name}.sha256").write_text(f"{digest}  {archive.name}\n")
    print(f"{archive} ({version}, {len(files) + 2} files, sha256 {digest})")


if __name__ == "__main__":
    main()
