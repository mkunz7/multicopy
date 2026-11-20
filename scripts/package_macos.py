"""Package the two Cargo macOS executables as a Universal 2 app ZIP."""
import plistlib
import subprocess
import stat
import struct
import tomllib
import tempfile
from pathlib import Path
from zipfile import ZIP_DEFLATED, ZipFile, ZipInfo

ROOT = Path(__file__).resolve().parents[1]
MACHO_MAGIC_64 = 0xFEEDFACF
FAT_MAGIC = 0xCAFEBABE
ARM64 = 0x0100000C
X86_64 = 0x01000007
SLICE_ALIGNMENT = 14


def universal_binary(arm64: bytes, x86_64: bytes) -> bytes:
    entries = []
    alignment = 1 << SLICE_ALIGNMENT
    offset = 8 + 20 * 2
    for blob, expected_cpu in [(arm64, ARM64), (x86_64, X86_64)]:
        magic, cpu, subtype, filetype = struct.unpack_from("<IIII", blob)
        if (magic, cpu, filetype) != (MACHO_MAGIC_64, expected_cpu, 2):
            raise ValueError("Expected a thin macOS executable for each architecture")
        offset = (offset + alignment - 1) & -alignment
        entries.append((cpu, subtype, offset, len(blob), SLICE_ALIGNMENT))
        offset += len(blob)

    result = bytearray(struct.pack(">II", FAT_MAGIC, len(entries)))
    for entry in entries:
        result.extend(struct.pack(">IIIII", *entry))
    for blob, entry in zip((arm64, x86_64), entries, strict=True):
        result.extend(bytes(entry[2] - len(result)))
        result.extend(blob)
    return bytes(result)


def add_file(archive: ZipFile, app: Path, path: Path) -> None:
    entry = ZipInfo("MultiCopy.app/" + path.relative_to(app).as_posix())
    entry.create_system = 3
    permissions = path.stat().st_mode & 0o777
    entry.external_attr = (stat.S_IFREG | permissions) << 16
    archive.writestr(entry, path.read_bytes(), compress_type=ZIP_DEFLATED, compresslevel=9)


def sign_app(app: Path) -> None:
    """Seal the finished bundle after creating its universal executable."""
    subprocess.run(
        ["/usr/bin/codesign", "--force", "--deep", "--sign", "-", "--timestamp=none", str(app)],
        check=True,
    )


def main() -> None:
    with (ROOT / "Cargo.toml").open("rb") as manifest:
        version = tomllib.load(manifest)["package"]["version"]
    target = ROOT / "target"
    arm64 = (target / "aarch64-apple-darwin/release/multicopy").read_bytes()
    x86_64 = (target / "x86_64-apple-darwin/release/multicopy").read_bytes()
    binary = universal_binary(arm64, x86_64)
    info = plistlib.dumps(
        {
            "CFBundleDisplayName": "MultiCopy",
            "CFBundleExecutable": "MultiCopy",
            "CFBundleIdentifier": "app.multicopy.desktop",
            "CFBundleInfoDictionaryVersion": "6.0",
            "CFBundleName": "MultiCopy",
            "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": version,
            "CFBundleVersion": version,
            "LSMinimumSystemVersion": "11.0",
            "NSHighResolutionCapable": True,
        }
    )

    output = ROOT / "dist" / f"MultiCopy-{version}-macOS-Universal.zip"
    output.parent.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="multicopy-package-") as temporary:
        app = Path(temporary) / "MultiCopy.app"
        contents = app / "Contents"
        executable = contents / "MacOS" / "MultiCopy"
        executable.parent.mkdir(parents=True)
        (contents / "Info.plist").write_bytes(info)
        (contents / "PkgInfo").write_bytes(b"APPL????")
        executable.write_bytes(binary)
        executable.chmod(0o755)
        sign_app(app)

        with ZipFile(output, "w") as archive:
            for path in sorted(path for path in app.rglob("*") if path.is_file()):
                add_file(archive, app, path)
    print(output)


if __name__ == "__main__":
    main()
