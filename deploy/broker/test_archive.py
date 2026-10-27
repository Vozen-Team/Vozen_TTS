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


class ArchiveTests(unittest.TestCase):
    def validate(self, payload=None, *, wrong_size=False, missing=False):
        payload = layer() if payload is None else payload
        digest = hashlib.sha256(payload).hexdigest()
        name = "blobs/sha256/" + digest
        manifest = [{"RepoTags": ["vozen-rust:" + REVISION], "Config": "config.json", "Layers": [name], "LayerSources": {"sha256:" + digest: {"mediaType": "application/vnd.oci.image.layer.v1.tar", "size": len(payload) + int(wrong_size), "digest": "sha256:" + digest}}}]
        files = {"manifest.json": json.dumps(manifest).encode(), "config.json": json.dumps({"config": {"Labels": {validator.REVISION_LABEL: REVISION}}}).encode()}
        if not missing:
            files[name] = payload
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
