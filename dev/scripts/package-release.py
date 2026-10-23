"""Package a staged build without modifying an installed/running application."""
import argparse
import hashlib
from pathlib import Path
import shutil
import tomllib
import zipfile


def main():
    root = Path(__file__).resolve().parents[2]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app-dir", required=True, type=Path)
    args = parser.parse_args()
    app = args.app_dir.resolve()
    version = tomllib.loads((root / "dev/core/Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]
    name = f"OpenLEAudio-{version}-win-x64"
    destination = root / "dev/artifacts" / name
    archive = root / f"{name}.zip"
    if destination.exists() or archive.exists():
        raise FileExistsError("Package output already exists; choose or archive the previous output first.")
    for required in ["OpenLEAudio4.exe", "OpenLEAudio4.dll", "OpenLEAudio4.Client.exe",
                     "OpenLEAudio4.pri", "OpenLEAudio4.deps.json", "OpenLEAudio4.runtimeconfig.json"]:
        if not (app / required).is_file():
            raise FileNotFoundError(f"Incomplete application stage: {required}")
    expected_core = root / "dev/core/target/release/OpenLEAudio_Client.exe"
    if hashlib.sha256((app / "OpenLEAudio4.Client.exe").read_bytes()).digest() != hashlib.sha256(expected_core.read_bytes()).digest():
        raise ValueError("Staged core does not match the current release build.")

    destination.mkdir(parents=True)
    shutil.copytree(app, destination / "OpenLEAudio4",
                    ignore=shutil.ignore_patterns("*.pdb", "*.lib", "*.exp", "logs"))
    release = root / "release"
    # Explicit allowlist keeps rollback JSON, cached installers and private state out.
    files = list(release.glob("*.bat")) + [release / "README.md"]
    files += list((release / "scripts").glob("*.ps1"))
    files += list((release / "driver").glob("*.inf")) + list((release / "driver").glob("*.cat"))
    files += [release / "dependencies/README.txt", release / "runtime-data/README.txt"]
    for source in files:
        target = destination / source.relative_to(release)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)

    with zipfile.ZipFile(archive, "x", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as output:
        for source in sorted(destination.rglob("*")):
            if source.is_file():
                output.write(source, source.relative_to(destination).as_posix())
    with zipfile.ZipFile(archive) as packaged:
        assert packaged.testzip() is None, "ZIP integrity check failed"
        for source in destination.rglob("*"):
            if source.is_file():
                assert packaged.read(source.relative_to(destination).as_posix()) == source.read_bytes(), source
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_suffix(".zip.sha256").write_text(f"{digest}  {archive.name}\n", encoding="ascii")
    print(f"Package: {destination}")
    print(f"Archive: {archive} ({archive.stat().st_size:,} bytes)")
    print(f"SHA-256: {digest}")
    print("PASS: all ZIP entries match the staged package; release core hash matches")


if __name__ == "__main__":
    main()
