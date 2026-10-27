#!/usr/bin/env python3
"""Validate a gzipped Docker image archive before passing it to docker load."""

from __future__ import annotations

import argparse
import json
import pathlib
import tarfile


MAX_ARCHIVE_BYTES = 2 * 1024 * 1024 * 1024
MAX_UNPACKED_BYTES = 8 * 1024 * 1024 * 1024
MAX_MEMBERS = 10_000
MAX_LAYER_MEMBERS = 100_000
REVISION_LABEL = "org.opencontainers.image.revision"


def safe_member_path(value: str) -> pathlib.PurePosixPath:
    path = pathlib.PurePosixPath(value)
    if not value or path.is_absolute() or ".." in path.parts:
        raise ValueError(f"unsafe production image artifact entry: {value}")
    return path


def read_json_member(image: tarfile.TarFile, name: str) -> object:
    safe_member_path(name)
    member = image.getmember(name)
    if not member.isfile() or member.size > 1024 * 1024:
        raise ValueError(f"production image artifact member is not a regular file: {name}")
    stream = image.extractfile(member)
    if stream is None:
        raise ValueError(f"unable to read production image artifact member: {name}")
    return json.load(stream)


def validate_archive(archive: pathlib.Path, revision: str) -> int:
    if archive.stat().st_size > MAX_ARCHIVE_BYTES:
        raise ValueError("production image artifact exceeds the 2 GiB safety limit")

    expected_tag = f"vozen-rust:{revision}"
    with tarfile.open(archive, mode="r:gz") as image:
        members, expanded = [], 0
        for member in image:
            if len(members) >= MAX_MEMBERS:
                raise ValueError("production image artifact contains too many entries")
            expanded += member.size
            if member.size < 0 or expanded > MAX_UNPACKED_BYTES:
                raise ValueError("production image artifact expands beyond the 8 GiB safety limit")
            members.append(member)
        if len({member.name for member in members}) != len(members):
            raise ValueError("production image artifact contains duplicate entries")
        for member in members:
            safe_member_path(member.name)
            if not (member.isfile() or member.isdir()):
                raise ValueError(f"unsafe production image artifact entry type: {member.name}")

        manifest = read_json_member(image, "manifest.json")
        if not isinstance(manifest, list) or len(manifest) != 1:
            raise ValueError("production image artifact must contain exactly one image")
        image_manifest = manifest[0]
        if not isinstance(image_manifest, dict) or image_manifest.get("RepoTags") != [expected_tag]:
            raise ValueError("production image artifact must contain exactly the expected tag")

        config_name = image_manifest.get("Config")
        if not isinstance(config_name, str):
            raise ValueError("production image artifact is missing its config")
        config = read_json_member(image, config_name)
        if not isinstance(config, dict):
            raise ValueError("production image artifact config is invalid")
        labels = config.get("config", {}).get("Labels", {}) or {}
        if labels.get(REVISION_LABEL) != revision:
            raise ValueError("production image artifact revision label mismatch")

        layers = image_manifest.get("Layers")
        sources = image_manifest.get("LayerSources")
        if (not isinstance(layers, list) or not layers
                or not all(isinstance(name, str) for name in layers)
                or len(set(layers)) != len(layers) or not isinstance(sources, dict)):
            raise ValueError("production image artifact layer metadata is invalid")
        layer_names = set(layers)
        snapshot_bytes = 0
        entries = 0
        for member in members:
            if member.name not in layer_names:
                continue
            parts = pathlib.PurePosixPath(member.name).parts
            if (len(parts) != 3 or parts[:2] != ("blobs", "sha256")
                    or len(parts[2]) != 64
                    or any(character not in "0123456789abcdef" for character in parts[2])):
                raise ValueError("production image artifact layer path is invalid")
            source = sources.get("sha256:" + parts[2])
            if (not member.isfile() or not isinstance(source, dict)
                    or source.get("mediaType") != "application/vnd.oci.image.layer.v1.tar"
                    or source.get("size") != member.size
                    or source.get("digest") != "sha256:" + parts[2]):
                raise ValueError("production image artifact layer size or format mismatch")
            # The reservation covers raw tar bytes, so compressed layers must fail closed.
            try:
                with image.extractfile(member) as stream, tarfile.open(fileobj=stream, mode="r:") as layer:
                    for entry in layer:
                        if entry.size < 0:
                            raise ValueError("production image filesystem has an invalid file size")
                        entries += 1
                        snapshot_bytes += 4096 + ((entry.size + 4095) // 4096) * 4096
                        if entries > MAX_LAYER_MEMBERS or snapshot_bytes > MAX_UNPACKED_BYTES:
                            raise ValueError("production image filesystem exceeds safety limits")
            except tarfile.TarError as error:
                raise ValueError("production image layer must be an uncompressed tar") from error
        if not layer_names.issubset({member.name for member in members}):
            raise ValueError("production image artifact layer is missing")
        return max(expanded, snapshot_bytes)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("archive", type=pathlib.Path)
    parser.add_argument("revision")
    args = parser.parse_args()
    validate_archive(args.archive, args.revision)


if __name__ == "__main__":
    main()
