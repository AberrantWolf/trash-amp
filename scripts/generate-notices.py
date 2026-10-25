#!/usr/bin/env python3
"""Package runtime notices, including custom Slint and native library licenses."""
import hashlib
import html
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
TOOL_VERSION = "0.9.2"
TOOL_SHA256 = "ae72f0df0c399a1e96336f696fa55b1b28679fd725632eba8cf8e4568467cc3e"


def main():
    output = Path(sys.argv[1]).resolve()
    output.mkdir(parents=True, exist_ok=True)
    if os.uname().sysname != "Darwin" or os.uname().machine != "arm64":
        raise SystemExit("Notice generation currently supports Apple Silicon macOS.")
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--format-version", "1"], cwd=ROOT
    ))
    slint = next(p for p in metadata["packages"] if p["name"] == "slint")
    slint_text = (ROOT / "assets/licenses/SLINT-LICENSE.md").read_bytes()
    upstream_slint = Path(slint["manifest_path"]).parent / "LICENSES/LicenseRef-Slint-Royalty-free-2.0.md"
    if upstream_slint.read_bytes() != slint_text:
        raise SystemExit("Slint license changed; update the bundled license before packaging.")
    skia = next(p for p in metadata["packages"] if p["name"] == "skia-bindings")
    skia_manifest = tomllib.loads(Path(skia["manifest_path"]).read_text())
    if skia["version"] != "0.153.3" or skia_manifest["package"]["metadata"]["skia"] != "m153-0.101.2":
        raise SystemExit("Skia version changed; update assets/native-licenses before packaging.")

    with tempfile.TemporaryDirectory(prefix="trash-amp-notices-") as work:
        work = Path(work)
        archive = work / "cargo-about.tar.gz"
        url = f"https://github.com/EmbarkStudios/cargo-about/releases/download/{TOOL_VERSION}/cargo-about-{TOOL_VERSION}-aarch64-apple-darwin.tar.gz"
        with urllib.request.urlopen(url, timeout=60) as response:
            archive.write_bytes(response.read())
        if hashlib.sha256(archive.read_bytes()).hexdigest() != TOOL_SHA256:
            raise SystemExit("cargo-about archive checksum mismatch")
        with tarfile.open(archive) as tar:
            tar.extractall(work, filter="data")
        tool = work / f"cargo-about-{TOOL_VERSION}-aarch64-apple-darwin/cargo-about"
        notices = output / "THIRD_PARTY_LICENSES.html"
        subprocess.run([
            str(tool), "about", "generate", "--manifest-path", str(ROOT / "crates/amp-desktop/Cargo.toml"),
            "--config", str(ROOT / "about.toml"), "--locked", "--fail",
            "--output-file", str(notices), str(ROOT / "assets/licenses.hbs"),
        ], cwd=ROOT, check=True)
    # cargo-about recognizes the LicenseRef choice but cannot harvest its text.
    # Include the exact upstream license explicitly rather than substituting GPL.
    section = "<h2>Slint Royalty-free Desktop, Mobile, and Web Applications License</h2>"
    section += "<p>Applies to Slint and its i-slint runtime crates.</p><pre>"
    section += html.escape(slint_text.decode()) + "</pre>\n"
    notices.write_text(notices.read_text().replace("</html>", section + "</html>"))
    shutil.copy2(ROOT / "LICENSE", output / "LICENSE")
    shutil.copy2(ROOT / "assets/licenses/SLINT-LICENSE.md", output / "SLINT-LICENSE.md")
    shutil.copytree(ROOT / "assets/native-licenses", output / "native-licenses", dirs_exist_ok=True)
    # Preserve native cryptography notices, including vendored third-party code.
    for package in metadata["packages"]:
        if package["name"] not in {"aws-lc-sys", "ring"}:
            continue
        source = Path(package["manifest_path"]).parent
        for path in source.rglob("*"):
            if path.is_file() and path.name.upper().startswith(("LICENSE", "COPYING", "NOTICE")):
                dest = output / "native-licenses" / package["name"] / path.relative_to(source)
                dest.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(path, dest)
    print(output)


if __name__ == "__main__":
    main()
