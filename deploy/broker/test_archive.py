"""Bound raw layer allocation before a signed image reaches Docker."""
import gzip
import hashlib
import importlib.util
import io
import json
import pathlib
import tarfile
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("validator", pathlib.Path(__file__).resolve().parents[2] / "scripts/validate-docker-image-archive.py")
validator = importlib.util.module_from_spec(spec)
spec.loader.exec_module(validator)
REVISION = "a" * 40


def layer(files=1):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w") as archive:
        for index in range(files):
            member = tarfile.TarInfo(f"code/{index}")
            member.size = 1
            archive.addfile(member, io.BytesIO(b"x"))
    return output.getvalue()


def path_layer(paths):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w") as archive:
        for path in paths:
            member = tarfile.TarInfo(path)
            member.size = 1
            archive.addfile(member, io.BytesIO(b"x"))
    return output.getvalue()


def blob(files, payload, media_type):
    digest = hashlib.sha256(payload).hexdigest()
    files["blobs/sha256/" + digest] = payload
    return {"mediaType": media_type, "size": len(payload), "digest": "sha256:" + digest}


def docker_compatibility(files, image):
    parent = None
    for index, _ in enumerate(image["layers"]):
        identifier = hashlib.sha256(("legacy" + str(index)).encode()).hexdigest()
        value = {"id": identifier, "created": "2026-10-28T00:00:00Z",
                 "container_config": {}, "os": "linux"}
        if parent:
            value["parent"] = parent
        if index == len(image["layers"]) - 1:
            value.update(config={}, architecture="amd64")
        blob(files, json.dumps(value).encode(), validator.OCI_CONFIG)
        parent = identifier
    files["repositories"] = json.dumps({"vozen-rust": {REVISION: image["layers"][-1]["digest"][7:]}}).encode()


class ArchiveTests(unittest.TestCase):
    def validate(self, payload=None, *, wrong_size=False, missing=False, oci=False, mutate=None, index_mutate=None, payloads=None):
        payload = layer() if payload is None else payload
        digest = hashlib.sha256(payload).hexdigest()
        name = "blobs/sha256/" + digest
        manifest = [{"RepoTags": ["vozen-rust:" + REVISION], "Config": "config.json", "Layers": [name], "LayerSources": {"sha256:" + digest: {"mediaType": "application/vnd.oci.image.layer.v1.tar", "size": len(payload) + int(wrong_size), "digest": "sha256:" + digest}}}]
        files = {"manifest.json": json.dumps(manifest).encode(), "config.json": json.dumps({"config": {"Labels": {validator.REVISION_LABEL: REVISION}}}).encode()}
        if not missing:
            files[name] = payload
        if payloads:
            files.pop(name, None)
            sources = [blob(files, item, validator.RAW_LAYER) for item in payloads]
            manifest[0]["Layers"] = ["blobs/sha256/" + source["digest"][7:] for source in sources]
            manifest[0]["LayerSources"] = {source["digest"]: source for source in sources}
            files["config.json"] = json.dumps({"architecture": "amd64", "os": "linux",
                "config": {"Labels": {validator.REVISION_LABEL: REVISION}},
                "rootfs": {"type": "layers", "diff_ids": [source["digest"] for source in sources]}}).encode()
            files["manifest.json"] = json.dumps(manifest).encode()
        if oci:
            config = blob(files, files.pop("config.json"), validator.OCI_CONFIG)
            manifest[0]["Config"] = "blobs/sha256/" + config["digest"][7:]
            image = {"schemaVersion": 2, "mediaType": validator.OCI_MANIFEST,
                     "config": config, "layers": [manifest[0]["LayerSources"]["sha256:" + name.split("/")[-1]]
                                                 for name in manifest[0]["Layers"]]}
            if mutate:
                mutate(files, image)
            descriptor = blob(files, json.dumps(image).encode(), validator.OCI_MANIFEST)
            descriptor["annotations"] = {"org.opencontainers.image.ref.name": REVISION,
                                         "io.containerd.image.name": "docker.io/library/vozen-rust:" + REVISION}
            files["oci-layout"] = b'{"imageLayoutVersion":"1.0.0"}'
            index = {"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": [descriptor]}
            if index_mutate:
                index_mutate(files, index)
            files["index.json"] = json.dumps(index).encode()
            files["manifest.json"] = json.dumps(manifest).encode()
        elif mutate:
            mutate(files, manifest)
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "image.tar.gz"
            with tarfile.open(path, "w:gz") as archive:
                for filename, data in files.items():
                    member = tarfile.TarInfo(filename)
                    member.size = len(data)
                    archive.addfile(member, io.BytesIO(data))
            return validator.validate_archive(path, REVISION)

    def test_valid_raw_layers_reserve_at_least_their_content(self):
        payload = layer()
        self.assertGreaterEqual(self.validate(payload), len(payload))

    def test_matching_single_raw_oci_graph_is_accepted(self):
        self.assertGreaterEqual(self.validate(oci=True), len(layer()))

    def test_current_ci_twelve_raw_layer_oci_export_shape_is_accepted(self):
        # Public 41f export metadata: single OCI manifest, SHA ref-name, normalized
        # containerd name, Linux/amd64 config, twelve ordered raw OCI layers.
        payloads = [path_layer(["shared/layer" + str(i) + "/file"]) for i in range(12)]
        self.assertGreaterEqual(self.validate(oci=True, payloads=payloads, mutate=docker_compatibility), sum(map(len, payloads)))

    def test_legacy_compatibility_cannot_admit_unreferenced_artifacts_or_other_tags(self):
        def extra_graph(files, image):
            docker_compatibility(files, image)
            blob(files, b'{"schemaVersion":2,"manifests":[]}', "application/vnd.oci.image.index.v1+json")
        def extra_compressed_blob(files, image):
            docker_compatibility(files, image)
            blob(files, gzip.compress(layer()), validator.RAW_LAYER + "+gzip")
        def foreign_repository(files, image):
            docker_compatibility(files, image)
            files["repositories"] = b'{"other":{"latest":"' + b"a" * 64 + b'"}}'
        def wrong_repository_target(files, image):
            docker_compatibility(files, image)
            files["repositories"] = json.dumps({"vozen-rust": {REVISION: "b" * 64}}).encode()
        def wrong_blob_hash(files, image):
            docker_compatibility(files, image)
            name = next(name for name, data in files.items() if b'"container_config"' in data)
            files[name] += b" "
        for change in (extra_graph, extra_compressed_blob, foreign_repository, wrong_repository_target, wrong_blob_hash):
            with self.subTest(change=change.__name__), self.assertRaises(ValueError):
                self.validate(oci=True, mutate=change)

    def test_same_implicit_parents_are_charged_in_each_layer(self):
        parents = "/".join("d" + str(i) for i in range(32))
        payloads = [path_layer([parents + "/f" + str(i)]) for i in range(2)]
        self.assertEqual(self.validate(oci=True, payloads=payloads), 2 * (32 * 4096 + 8192))

    def test_oci_extra_image_nested_index_and_wrong_tag_are_rejected(self):
        def extra_image(files, index):
            index["manifests"].append(index["manifests"][0])
        def nested_index(files, index):
            index["manifests"][0] = blob(files, json.dumps(index).encode(), "application/vnd.oci.image.index.v1+json")
        def wrong_tag(files, index):
            index["manifests"][0]["annotations"]["io.containerd.image.name"] = "docker.io/library/other:latest"
        for change in (extra_image, nested_index, wrong_tag):
            with self.subTest(change=change.__name__), self.assertRaises(ValueError):
                self.validate(oci=True, index_mutate=change)

    def test_authoritative_oci_cannot_reference_another_compressed_layer(self):
        def diverge(files, image):
            image["layers"] = [blob(files, gzip.compress(layer()), validator.RAW_LAYER + "+gzip")]
        with self.assertRaisesRegex(ValueError, "descriptor format"):
            self.validate(oci=True, mutate=diverge)

    def test_authoritative_oci_cannot_reference_another_raw_layer(self):
        def diverge(files, image):
            image["layers"] = [blob(files, layer(2), validator.RAW_LAYER)]
        with self.assertRaisesRegex(ValueError, "graph differs"):
            self.validate(oci=True, mutate=diverge)

    def test_authoritative_oci_cannot_reference_another_config(self):
        def diverge(files, image):
            image["config"] = blob(files, b"{}", validator.OCI_CONFIG)
        with self.assertRaisesRegex(ValueError, "graph differs"):
            self.validate(oci=True, mutate=diverge)

    def test_oci_payload_digest_and_descriptor_size_are_verified(self):
        def tamper(files, image):
            image["layers"][0]["size"] += 1
        with self.assertRaisesRegex(ValueError, "size or digest"):
            self.validate(oci=True, mutate=tamper)
        def tamper_bytes(files, image):
            name = "blobs/sha256/" + image["layers"][0]["digest"][7:]
            files[name] = b"!" + files[name][1:]
        with self.assertRaisesRegex(ValueError, "size or digest"):
            self.validate(oci=True, mutate=tamper_bytes)

    def test_unreferenced_oci_content_is_rejected(self):
        def extra(files, image):
            blob(files, gzip.compress(layer()), validator.RAW_LAYER + "+gzip")
        with self.assertRaisesRegex(ValueError, "unapproved OCI content"):
            self.validate(oci=True, mutate=extra)

    def test_omitted_parents_are_charged_once_per_layer(self):
        parents = "/".join("d" + str(i) for i in range(32))
        payload = path_layer([parents + "/f" + str(i) for i in range(64)])
        self.assertEqual(self.validate(payload), 32 * 4096 + 64 * 8192)

    def test_implicit_parents_count_towards_entry_limit(self):
        with patch.object(validator, "MAX_LAYER_MEMBERS", 3):
            with self.assertRaisesRegex(ValueError, "filesystem exceeds"):
                self.validate(path_layer(["a/b/c/file"]))

    def test_deep_distinct_implicit_parent_reproducer_is_rejected(self):
        payload = path_layer(["/".join(["d" + str(i)] * 400) + "/file" for i in range(3000)])
        with self.assertRaisesRegex(ValueError, "path exceeds"):
            self.validate(payload)

    def test_noncanonical_or_unbounded_paths_fail_closed(self):
        for path in ("a/../file", "a//file", "./file", "/file", "x" * 4097):
            with self.subTest(path=path[:20]), self.assertRaises(ValueError):
                self.validate(path_layer([path]))

    def test_small_files_include_block_and_metadata_overhead(self):
        self.assertGreaterEqual(self.validate(layer(16)), 16 * 4096)

    def test_compressed_layer_cannot_hide_behind_a_raw_media_type(self):
        with self.assertRaisesRegex(ValueError, "uncompressed tar"):
            self.validate(gzip.compress(layer()))

    def test_layer_metadata_size_must_match(self):
        with self.assertRaisesRegex(ValueError, "size or format"):
            self.validate(wrong_size=True)

    def test_missing_layers_are_rejected(self):
        with self.assertRaisesRegex(ValueError, "layer is missing"):
            self.validate(missing=True)

    def test_huge_declared_inner_file_is_rejected_before_unpacking(self):
        member = tarfile.TarInfo("code/oversized")
        member.size = validator.MAX_UNPACKED_BYTES + 1
        with self.assertRaisesRegex(ValueError, "filesystem exceeds"):
            self.validate(member.tobuf() + bytes(1024))

    def test_inner_and_outer_entry_counts_are_bounded(self):
        with patch.object(validator, "MAX_LAYER_MEMBERS", 1):
            with self.assertRaisesRegex(ValueError, "filesystem exceeds"):
                self.validate(layer(2))
        with patch.object(validator, "MAX_MEMBERS", 1):
            with self.assertRaisesRegex(ValueError, "too many entries"):
                self.validate()


if __name__ == "__main__":
    unittest.main()
