"""Validate the published SSH release from GitHub API data and downloaded metadata."""

import argparse
import json
from pathlib import Path


def verify(repository: str, tag: str, directory: Path) -> None:
    release = json.loads((directory / "release.json").read_text())
    latest = json.loads((directory / "latest-release.json").read_text())
    manifest = json.loads((directory / "latest.json").read_text())
    if release["tag_name"] != tag or release["draft"] or release["prerelease"]:
        raise ValueError("Release is not published as the requested stable SSH build")
    if latest["tag_name"] != tag:
        raise ValueError("Latest release does not point to the new SSH build")

    prefix = f"CC-Switch-{tag}"
    expected = {
        f"{prefix}-macOS.dmg",
        f"{prefix}-macOS.zip",
        f"{prefix}-macOS.tar.gz",
        f"{prefix}-macOS.tar.gz.sig",
        f"{prefix}-Windows.msi",
        f"{prefix}-Windows.msi.sig",
        f"{prefix}-Windows-Portable.zip",
        f"{prefix}-Linux-x86_64.AppImage",
        f"{prefix}-Linux-x86_64.AppImage.sig",
        f"{prefix}-Linux-x86_64.deb",
        f"{prefix}-Linux-x86_64.deb.sig",
        f"{prefix}-Linux-x86_64.rpm",
        f"{prefix}-Linux-x86_64.rpm.sig",
        "latest.json",
    }
    assets = {asset["name"]: asset for asset in release["assets"]}
    for name in expected:
        asset = assets.get(name)
        if not asset or asset["state"] != "uploaded" or asset["size"] <= 0:
            raise ValueError(f"Missing or incomplete release asset: {name}")

    targets = {
        "darwin-aarch64": f"{prefix}-macOS.tar.gz",
        "darwin-x86_64": f"{prefix}-macOS.tar.gz",
        "windows-x86_64": f"{prefix}-Windows.msi",
        "linux-x86_64": f"{prefix}-Linux-x86_64.AppImage",
        "linux-x86_64-deb": f"{prefix}-Linux-x86_64.deb",
        "linux-x86_64-rpm": f"{prefix}-Linux-x86_64.rpm",
    }
    if manifest["version"] != tag.removeprefix("v"):
        raise ValueError("Updater version differs from the published tag")
    if set(manifest["platforms"]) != set(targets):
        raise ValueError("Updater platform list is incomplete")
    for platform, name in targets.items():
        entry = manifest["platforms"][platform]
        url = f"https://github.com/{repository}/releases/download/{tag}/{name}"
        signature = (directory / f"{name}.sig").read_text().strip()
        if entry["url"] != url or assets[name]["browser_download_url"] != url:
            raise ValueError(f"Updater URL differs from the release asset: {platform}")
        if not signature or entry["signature"].strip() != signature:
            raise ValueError(f"Updater signature differs from the published signature: {platform}")
    notes = Path(f"docs/release-notes/{tag}-zh.md")
    if notes.is_file() and release["body"].strip() != notes.read_text().strip():
        raise ValueError("Published release notes differ from the committed notes")
    print(
        f"Verified {tag}: {len(expected)} assets, {len(targets)} updater platforms, latest release"
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--directory", type=Path, required=True)
    args = parser.parse_args()
    verify(args.repository, args.tag, args.directory)
