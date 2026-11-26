#!/usr/bin/env python3
"""Hash release archives and sign the exact manifest bytes when enabled."""
import argparse
import base64
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import tarfile
import tempfile
import zipfile

ROOT = Path(__file__).resolve().parents[2]
HEADLESS = ("ddg", "ddg-agents-host", "ddg-terminal-server")
DESKTOP = (*HEADLESS, "ddg-agents-native")
MAC_CONTENTS = "DDG Agents.app/Contents/"
PLATFORMS = {
    "darwin-arm64": ("app.zip", MAC_CONTENTS + "MacOS/", "", DESKTOP),
    "linux-x86_64": ("tar.gz", "", "", HEADLESS),
    "linux-aarch64": ("tar.gz", "", "", HEADLESS),
    "windows-x86_64": ("zip", "", ".exe", DESKTOP),
}
NUMBER = r"(?:0|[1-9][0-9]*)"
PRE = rf"(?:{NUMBER}|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
TAG = re.compile(rf"v?({NUMBER}\.{NUMBER}\.{NUMBER})(-{PRE}(?:\.{PRE})*)?(\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?")


def sha256(stream):
    digest = hashlib.sha256()
    for chunk in iter(lambda: stream.read(1024 * 1024), b""):
        digest.update(chunk)
    return digest.hexdigest()


def inspect_archive(path, platform, prefix, extension, binaries):
    with path.open("rb") as stream:
        digest = sha256(stream)
        size = stream.tell()
        stream.seek(0)
        is_tar = path.name.endswith(".tar.gz")
        archive = tarfile.open(fileobj=stream, mode="r:gz") if is_tar else zipfile.ZipFile(stream)
        with archive:
            members = {}
            for member in archive.getmembers() if is_tar else archive.infolist():
                name = member.name if is_tar else member.filename
                name = name.rstrip("/")
                if name in members or any(part == "__MACOSX" or part.startswith("._") for part in name.split("/")):
                    raise ValueError(f"{path.name}: duplicate or AppleDouble member {name!r}")
                members[name] = member

            def regular_file(name):
                member = members.get(name)
                if is_tar:
                    valid = member is not None and member.isfile()
                else:
                    valid = (member is not None and not member.is_dir()
                             and stat.S_IFMT(member.external_attr >> 16) in (0, stat.S_IFREG))
                if not valid:
                    raise ValueError(f"{path.name}: missing regular file {name!r}")
                return archive.extractfile(member) if is_tar else archive.open(member)

            notices = (MAC_CONTENTS + "Resources/" if prefix else "") + "THIRD_PARTY_NOTICES.txt"
            with regular_file(notices):
                pass
            hashes = {}
            for bin_name in binaries:
                name = prefix + bin_name + extension
                with regular_file(name) as binary:
                    hashes[bin_name] = {"path": name, "sha256": sha256(binary)}
    return {"platform": platform, "name": path.name, "sha256": digest, "size": size, "binaries": hashes}


def detached_signature(manifest, key, key_id):
    # Rust verifies the raw manifest bytes and reads the raw 64-byte signature from a JSON envelope.
    with tempfile.TemporaryDirectory(prefix="ddg-sign-") as directory:
        manifest_path = Path(directory) / "manifest.json"
        key_path = Path(directory) / "release.pem"
        manifest_path.write_bytes(manifest)
        key_path.write_bytes(key)
        public_key = subprocess.check_output([
            "openssl", "pkey", "-in", str(key_path), "-pubout", "-outform", "DER",
        ], stdin=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if len(public_key) != 44 or not public_key.startswith(bytes.fromhex("302a300506032b6570032100")):
            raise ValueError("expected an Ed25519 PEM private key")
        signature = subprocess.check_output([
            "openssl", "pkeyutl", "-sign", "-rawin", "-inkey", str(key_path), "-in", str(manifest_path),
        ], stdin=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if len(signature) != 64:
        raise ValueError("expected a 64-byte Ed25519 signature")
    return json_bytes({"algorithm": "ed25519", "key_id": key_id, "value": base64.b64encode(signature).decode("ascii")})


def json_bytes(value):
    return (json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode("utf-8")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifacts-dir", type=Path, required=True, help="read archives and write metadata here")
    parser.add_argument("--tag", required=True)
    parser.add_argument("--require-all", action="store_true", help="require every release platform")
    parser.add_argument("--holder-restart-required", action="store_true")
    parser.add_argument("--release-notes-summary", default="")
    parser.add_argument("--ed25519-key-file", type=Path, help="PEM private key for Phase 1 dry runs only")
    parser.add_argument("--ed25519-key-id", default=os.environ.get("RELEASE_ED25519_KEY_ID"))
    args = parser.parse_args()
    match = TAG.fullmatch(args.tag)
    if not match or any(int(n) >= 2**64 for n in match[1].split(".")):
        parser.error("--tag must be a SemVer release tag")

    source = (ROOT / "crates/ddg-agents-update/src/integrity.rs").read_text(encoding="utf-8")
    # Only the explicit Phase 1 initializer opts out; unknown syntax fails closed.
    signing_required = not re.search(
        r"^pub const RELEASE_PUBKEYS:\s*&\[\(&str,\s*\[u8;\s*32\]\)\]\s*=\s*&\[\s*\]\s*;", source, re.M,
    )
    key = os.environ.get("RELEASE_ED25519_PRIVATE_KEY", "").strip()
    if signing_required and (not key or args.ed25519_key_file or not (args.ed25519_key_id or "").strip()):
        parser.error("Phase 2 requires RELEASE_ED25519_PRIVATE_KEY (PEM) and RELEASE_ED25519_KEY_ID")

    version = args.tag.removeprefix("v")
    outputs = args.artifacts_dir
    artifacts = []
    for platform, (format_, prefix, extension, binaries) in PLATFORMS.items():
        path = outputs / f"ddg-agents-{version}-{platform}.{format_}"
        if args.require_all or path.is_file():
            artifacts.append(inspect_archive(path, platform, prefix, extension, binaries))
    if not artifacts:
        parser.error(f"no platform archives found for {args.tag}")

    manifest = json_bytes({
        "schema_version": 1,
        "version": version,
        "tag": args.tag,
        "channel": "prerelease" if match[2] else "stable",
        "published_at": datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z"),
        "protocol_version": 1,
        "holder_protocol_version": 1,
        "holder_restart_required": args.holder_restart_required,
        "min_compatible_schema": 1,
        "release_notes_summary": args.release_notes_summary,
        "artifacts": artifacts,
    })
    signature = None
    if signing_required or args.ed25519_key_file:
        signature = detached_signature(
            manifest, key.encode("utf-8") + b"\n" if signing_required else args.ed25519_key_file.read_bytes(),
            args.ed25519_key_id or "dry-run",
        )
    sig_path = outputs / "manifest.json.sig"
    sig_path.unlink(missing_ok=True)
    (outputs / "manifest.json").write_bytes(manifest)
    (outputs / "SHA256SUMS").write_text("".join(f"{a['sha256']}  {a['name']}\n" for a in artifacts), encoding="ascii")
    if signature is not None:
        sig_path.write_bytes(signature)


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError:
        raise SystemExit("OpenSSL could not sign the manifest (expected an Ed25519 PEM private key)") from None
    except (OSError, ValueError, tarfile.TarError, zipfile.BadZipFile) as error:
        raise SystemExit(str(error)) from None
