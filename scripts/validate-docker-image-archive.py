#!/usr/bin/env python3
"""Validate a gzipped Docker image archive before passing it to docker load."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import tarfile


MAX_ARCHIVE_BYTES = 2 * 1024 * 1024 * 1024
MAX_UNPACKED_BYTES = 8 * 1024 * 1024 * 1024
MAX_MEMBERS = 10_000
MAX_LAYER_MEMBERS = 100_000
MAX_PATH_BYTES = 4096
MAX_PATH_DEPTH = 128
REVISION_LABEL = "org.opencontainers.image.revision"
RAW_LAYER = "application/vnd.oci.image.layer.v1.tar"
OCI_MANIFEST = "application/vnd.oci.image.manifest.v1+json"
OCI_CONFIG = "application/vnd.oci.image.config.v1+json"


def safe_member_path(value: str) -> pathlib.PurePosixPath:
    if (not isinstance(value, str) or len(value.encode("utf-8")) > MAX_PATH_BYTES
            or "\x00" in value or len(value.split("/")) > MAX_PATH_DEPTH):
        raise ValueError("production image artifact path exceeds safety limits")
    path = pathlib.PurePosixPath(value)
    if (not value or path.is_absolute() or ".." in path.parts
            or value.rstrip("/") != path.as_posix()):
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
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("duplicate production image JSON key")
            result[key] = value
        return result
    return json.load(stream, object_pairs_hook=unique)


def descriptor_name(descriptor, media_type, members, digests):
    allowed = {"mediaType", "digest", "size"}
    if media_type == OCI_MANIFEST:
        allowed.add("annotations")
    if (not isinstance(descriptor, dict) or descriptor.get("mediaType") != media_type
            or set(descriptor) - allowed):
        raise ValueError("production image OCI descriptor format mismatch")
    digest = descriptor.get("digest")
    if (not isinstance(digest, str) or not digest.startswith("sha256:")
            or len(digest) != 71 or any(c not in "0123456789abcdef" for c in digest[7:])):
        raise ValueError("production image OCI descriptor digest is invalid")
    name = "blobs/sha256/" + digest[7:]
    member = members.get(name)
    if (member is None or not member.isfile() or type(descriptor.get("size")) is not int
            or descriptor["size"] != member.size or digests.get(name) != digest[7:]
            or descriptor.get("urls") or descriptor.get("data")):
        raise ValueError("production image OCI descriptor size or digest mismatch")
    return name


def validate_oci_graph(image, members, digests, config_name, layers, expected_tag):
    if "oci-layout" not in members and "index.json" not in members:
        return
    layout = read_json_member(image, "oci-layout")
    index = read_json_member(image, "index.json")
    if (layout != {"imageLayoutVersion": "1.0.0"} or not isinstance(index, dict)
            or index.get("schemaVersion") != 2 or not isinstance(index.get("manifests"), list)
            or set(index) - {"schemaVersion", "mediaType", "manifests"}
            or index.get("mediaType", "application/vnd.oci.image.index.v1+json") != "application/vnd.oci.image.index.v1+json"
            or len(index["manifests"]) != 1):
        raise ValueError("production image OCI layout must contain exactly one image")
    descriptor = index["manifests"][0]
    annotations = descriptor.get("annotations", {}) if isinstance(descriptor, dict) else None
    expected_annotations = {"org.opencontainers.image.ref.name": expected_tag.split(":", 1)[1],
                            "io.containerd.image.name": "docker.io/library/" + expected_tag}
    if annotations != expected_annotations:
        raise ValueError("production image OCI tag differs from Docker manifest")
    manifest_name = descriptor_name(descriptor, OCI_MANIFEST, members, digests)
    manifest = read_json_member(image, manifest_name)
    if (not isinstance(manifest, dict) or manifest.get("schemaVersion") != 2
            or set(manifest) != {"schemaVersion", "mediaType", "config", "layers"}
            or manifest.get("mediaType") != OCI_MANIFEST
            or descriptor_name(manifest.get("config"), OCI_CONFIG, members, digests) != config_name
            or not isinstance(manifest.get("layers"), list)
            or [descriptor_name(item, RAW_LAYER, members, digests) for item in manifest["layers"]] != layers
            or manifest.get("subject") is not None):
        raise ValueError("production image OCI graph differs from Docker manifest")
    approved = {"manifest.json", "oci-layout", "index.json", manifest_name, config_name, *layers}
    extras = [name for name, member in members.items() if member.isfile() and name not in approved]
    if extras:
        validate_docker_compatibility(image, members, digests, extras, layers, expected_tag)


def validate_docker_compatibility(image, members, digests, extras, layers, expected_tag):
    # OCI ImportIndex ingests these unreferenced v1 compatibility records as CAS
    # content, but only the approved OCI graph can be unpacked. Their bytes are
    # already included in expanded; do not admit another manifest/index or layer.
    legacy_ids = set()
    legacy_names = [name for name in extras if name != "repositories"]
    if len(legacy_names) > len(layers) or "repositories" not in extras:
        raise ValueError("production image artifact contains unapproved OCI content")
    for name in legacy_names:
        if (not name.startswith("blobs/sha256/") or len(name) != len("blobs/sha256/") + 64
                or name.rsplit("/", 1)[1] != digests[name]):
            raise ValueError("production image artifact contains unapproved OCI content")
        value = read_json_member(image, name)
        required = {"id", "created", "container_config", "os"}
        if (not isinstance(value, dict) or not required.issubset(value)
                or set(value) - required - {"parent", "config", "architecture"}
                or value["os"] != "linux" or value.get("architecture", "amd64") != "amd64"
                or not isinstance(value["created"], str) or len(value["created"]) > 128
                or not isinstance(value["container_config"], dict)
                or not isinstance(value.get("config", {}), dict)):
            raise ValueError("production image artifact contains unapproved OCI content")
        for field in ("id", "parent"):
            identifier = value.get(field)
            if field == "parent" and identifier is None:
                continue
            if (not isinstance(identifier, str) or len(identifier) != 64
                    or any(c not in "0123456789abcdef" for c in identifier)):
                raise ValueError("production image legacy identifier is invalid")
        if value["id"] in legacy_ids:
            raise ValueError("production image legacy identifier is duplicated")
        legacy_ids.add(value["id"])
    repositories = read_json_member(image, "repositories")
    revision = expected_tag.split(":", 1)[1]
    if (not isinstance(repositories, dict) or set(repositories) != {"vozen-rust"}
            or not isinstance(repositories["vozen-rust"], dict)
            or set(repositories["vozen-rust"]) != {revision}
            or repositories["vozen-rust"][revision] != layers[-1].rsplit("/", 1)[1]):
        raise ValueError("production image legacy repository tag mismatch")


def validate_archive(archive: pathlib.Path, revision: str) -> int:
    if archive.stat().st_size > MAX_ARCHIVE_BYTES:
        raise ValueError("production image artifact exceeds the 2 GiB safety limit")

    expected_tag = f"vozen-rust:{revision}"
    with tarfile.open(archive, mode="r:gz") as image:
        members, expanded, digests = [], 0, {}
        for member in image:
            if len(members) >= MAX_MEMBERS:
                raise ValueError("production image artifact contains too many entries")
            expanded += member.size
            if member.size < 0 or expanded > MAX_UNPACKED_BYTES:
                raise ValueError("production image artifact expands beyond the 8 GiB safety limit")
            members.append(member)
            if member.isfile():
                digest = hashlib.sha256()
                with image.extractfile(member) as stream:
                    for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                        digest.update(chunk)
                digests[member.name] = digest.hexdigest()
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
        validate_oci_graph(image, {member.name: member for member in members}, digests, config_name, layers, expected_tag)
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
                    or source.get("mediaType") != RAW_LAYER
                    or source.get("size") != member.size
                    or source.get("digest") != "sha256:" + parts[2]
                    or digests[member.name] != parts[2]):
                raise ValueError("production image artifact layer size or format mismatch")
            # The reservation covers raw tar bytes, so compressed layers must fail closed.
            try:
                with image.extractfile(member) as stream, tarfile.open(fileobj=stream, mode="r:") as layer:
                    directories = set()
                    for entry in layer:
                        if entry.size < 0:
                            raise ValueError("production image filesystem has an invalid file size")
                        path = safe_member_path(entry.name)
                        entries += 1
                        # Apply creates omitted parents, independently in each snapshot layer.
                        parents = path.parents if not entry.isdir() else (path, *path.parents)
                        for parent in parents:
                            if parent == pathlib.PurePosixPath(".") or parent in directories:
                                continue
                            directories.add(parent)
                            if parent != path:
                                entries += 1
                            snapshot_bytes += 4096
                        if not entry.isdir():
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
