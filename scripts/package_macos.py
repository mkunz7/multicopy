"""Package the two Cargo macOS executables as a Universal 2 app ZIP."""
import plistlib
import stat
import struct
import tomllib
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


def add_file(archive: ZipFile, name: str, data: bytes, permissions: int) -> None:
    entry = ZipInfo("MultiCopy.app/Contents/" + name)
    entry.create_system = 3
    entry.external_attr = (stat.S_IFREG | permissions) << 16
    archive.writestr(entry, data, compress_type=ZIP_DEFLATED, compresslevel=9)


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
    with ZipFile(output, "w") as archive:
        add_file(archive, "Info.plist", info, 0o644)
        add_file(archive, "MacOS/MultiCopy", binary, 0o755)
    print(output)


if __name__ == "__main__":
    main()
