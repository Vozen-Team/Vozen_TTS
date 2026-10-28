"""Credential-free security/rollback tests; never contact Docker or GitHub."""
import base64
import copy
import importlib.util
import json
import os
import pathlib
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import padding, rsa

spec = importlib.util.spec_from_file_location("broker", pathlib.Path(__file__).with_name("broker.py"))
broker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(broker)


def encoded(value):
    if isinstance(value, dict):
        value = json.dumps(value).encode()
    return base64.urlsafe_b64encode(value).decode().rstrip("=")


class CapacityTests(unittest.TestCase):
    def check(self, separate, root_free, image_free, reservations):
        def filesystem(path, **kwargs):
            return SimpleNamespace(st_dev=1 if path == broker.IMAGE_SNAPSHOTS or not separate else 2)
        def free(path):
            return SimpleNamespace(free=root_free if path == broker.IMAGE_SNAPSHOTS or not separate else image_free)
        with patch.object(pathlib.Path, "stat", autospec=True, side_effect=filesystem), patch.object(broker.shutil, "disk_usage", side_effect=free):
            broker.require_disk_headroom(reservations)

    def test_primary_filesystem_reserves_content_and_snapshots_together(self):
        reservations = [(broker.IMAGE_CONTENT, 3 * 1024**3), (broker.IMAGE_SNAPSHOTS, 3 * 1024**3)]
        self.check(False, 8 * 1024**3, 8 * 1024**3, reservations)
        with self.assertRaisesRegex(broker.Refusal, "disk headroom"):
            self.check(False, 7 * 1024**3, 7 * 1024**3, reservations)

    def test_primary_budget_has_exact_combined_margin_and_guard_boundary(self):
        sizes = (2 * 1024**3 + 123, 3 * 1024**3 + 456)
        total = sum(sizes)
        required = total + (total + 9) // 10 + 1024**3
        reservations = list(zip((broker.IMAGE_CONTENT, broker.IMAGE_SNAPSHOTS), sizes))
        for free in (max(sizes) + 1024**3, total, required):
            with self.subTest(free=free), self.assertRaisesRegex(broker.Refusal, "disk headroom"):
                self.check(False, free, free, reservations)
        self.check(False, required + 1, required + 1, reservations)

    def test_initial_archive_budget_has_margin_and_guard(self):
        required = broker.MAX_ARCHIVE + (broker.MAX_ARCHIVE + 9) // 10 + 1024**3
        with self.assertRaisesRegex(broker.Refusal, "disk headroom"):
            self.check(False, required, required, [(broker.STAGING, broker.MAX_ARCHIVE)])
        self.check(False, required + 1, required + 1, [(broker.STAGING, broker.MAX_ARCHIVE)])

    def test_measured_archive_fits_primary_budget_without_worst_case_reservation(self):
        expanded = 2_982_846_464
        free_after_copy = 10_853_363_712 - 1_428_070_112
        reservations = [(broker.IMAGE_CONTENT, expanded), (broker.IMAGE_SNAPSHOTS, expanded)]
        self.check(False, free_after_copy, free_after_copy, reservations)
        with self.assertRaisesRegex(broker.Refusal, "disk headroom"):
            self.check(False, 7_636_004_045, 7_636_004_045, reservations)

    def test_separate_filesystems_keep_their_own_reservations(self):
        reservations = [(broker.IMAGE_CONTENT, 3 * 1024**3), (broker.IMAGE_SNAPSHOTS, 3 * 1024**3)]
        self.check(True, 5 * 1024**3, 5 * 1024**3, reservations)
        with self.assertRaisesRegex(broker.Refusal, "disk headroom"):
            self.check(True, 4 * 1024**3, 5 * 1024**3, reservations)
        with self.assertRaisesRegex(broker.Refusal, "disk headroom"):
            self.check(True, 5 * 1024**3, 4 * 1024**3, reservations)

    def test_staging_reservation_and_invalid_sizes_fail_closed(self):
        self.check(True, 0, 4 * 1024**3, [(broker.STAGING, broker.MAX_ARCHIVE)])
        with self.assertRaisesRegex(broker.Refusal, "disk headroom"):
            self.check(True, 0, 3 * 1024**3, [(broker.STAGING, broker.MAX_ARCHIVE)])
        for size in (None, True, 0, -1, 9 * 1024**3):
            with self.assertRaisesRegex(broker.Refusal, "invalid storage reservation"):
                self.check(True, 100 * 1024**3, 100 * 1024**3, [(broker.STAGING, size)])

    def test_primary_layout_checks_trusted_directories_without_secondary_mounts(self):
        self.assertEqual(broker.STAGING, broker.STATE)
        self.assertEqual(broker.IMAGE_CONTENT.as_posix(), "/var/lib/containerd/io.containerd.content.v1.content")
        self.assertEqual(broker.IMAGE_SNAPSHOTS.as_posix(), "/var/lib/containerd/io.containerd.snapshotter.v1.overlayfs")
        devices = {pathlib.Path("/"): 1, broker.STATE: 1,
                   broker.IMAGE_SNAPSHOTS: 1, broker.IMAGE_CONTENT: 1}
        def filesystem(path, **kwargs):
            return SimpleNamespace(st_dev=devices[path])
        with patch.object(broker, "trusted_path") as trust, patch.object(broker.os.path, "ismount", side_effect=AssertionError("mount requirement")), patch.object(pathlib.Path, "stat", autospec=True, side_effect=filesystem):
            broker.require_storage_layout()
            self.assertEqual([call.args[0] for call in trust.call_args_list],
                             [broker.STATE, broker.STAGING, broker.IMAGE_CONTENT, broker.IMAGE_SNAPSHOTS])
            for path in (broker.STATE, broker.IMAGE_CONTENT, broker.IMAGE_SNAPSHOTS):
                devices[path] = 2
                with self.assertRaisesRegex(broker.Refusal, "unexpected image preparation filesystem"):
                    broker.require_storage_layout()
                devices[path] = 1

    def test_primary_layout_rejects_untrusted_paths(self):
        with patch.object(pathlib.Path, "stat", return_value=SimpleNamespace(st_dev=1)), patch.object(broker, "trusted_path", side_effect=broker.Refusal("untrusted broker filesystem")):
            with self.assertRaisesRegex(broker.Refusal, "untrusted"):
                broker.require_storage_layout()


class TrustTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        public = cls.key.public_key().public_numbers()
        cls.jwks = {"keys": [{"kid": "fixture", "kty": "RSA", "use": "sig",
                               "e": encoded(public.e.to_bytes(3, "big")),
                               "n": encoded(public.n.to_bytes(256, "big"))}]}

    def setUp(self):
        self.request = {"sha": "a" * 40, "run_id": "123", "sha256": "b" * 64, "token": ""}
        self.claims = {"iss": broker.ISSUER, "aud": broker.request_fields(self.request),
                       "repository": broker.REPOSITORY, "repository_id": broker.REPOSITORY_ID,
                       "repository_owner_id": broker.OWNER_ID, "ref": "refs/heads/main",
                       "runner_environment": "github-hosted", "event_name": "workflow_dispatch",
                       "workflow_ref": broker.REPOSITORY + "/.github/workflows/deploy-bot.yml@refs/heads/main",
                       "sub": "repo:" + broker.REPOSITORY + ":ref:refs/heads/main",
                       "iat": 1000, "nbf": 1000, "exp": 1300}

    def token(self, claims=None, header=None):
        parts = [encoded(header or {"alg": "RS256", "kid": "fixture"}), encoded(claims or self.claims)]
        signature = self.key.sign(".".join(parts).encode(), padding.PKCS1v15(), hashes.SHA256())
        return ".".join(parts + [encoded(signature)])

    def verify(self):
        return broker.verify_oidc(self.request, self.jwks, 1100)

    def test_accepts_signed_bound_request(self):
        self.request["token"] = self.token()
        self.verify()

    def test_accepts_immutable_subject(self):
        self.claims["sub"] = "repo:Vozen-Team@343175426/Vozen_TTS@1308788653:ref:refs/heads/main"
        self.request["token"] = self.token()
        self.verify()

    def test_accepts_workflow_run_event(self):
        self.claims["event_name"] = "workflow_run"
        self.request["token"] = self.token()
        self.verify()

    def test_pins_transferred_repository_identity(self):
        self.assertEqual(broker.REPOSITORY, "Vozen-Team/Vozen_TTS")
        self.assertEqual(broker.REPOSITORY_ID, "1308788653")
        self.assertEqual(broker.OWNER_ID, "343175426")

    def test_rejects_previous_owner_and_subjects(self):
        for change in (
            {"repository": "Rexy40407/Vozen_TTS"},
            {"repository_owner_id": "285029577"},
            {"sub": "repo:Rexy40407/Vozen_TTS:ref:refs/heads/main"},
            {"sub": "repo:Rexy40407@285029577/Vozen_TTS@1308788653:ref:refs/heads/main"},
            {"workflow_ref": "Rexy40407/Vozen_TTS/.github/workflows/deploy-bot.yml@refs/heads/main"},
        ):
            with self.subTest(change=change), self.assertRaises(broker.Refusal):
                self.request["token"] = self.token({**self.claims, **change})
                self.verify()

    def test_rejects_untrusted_claims(self):
        for key in ("iss", "aud", "repository", "repository_id", "repository_owner_id", "ref",
                    "runner_environment", "workflow_ref", "sub", "event_name"):
            with self.subTest(key=key):
                claims = {**self.claims, key: "attacker"}
                self.request["token"] = self.token(claims)
                with self.assertRaises(broker.Refusal):
                    self.verify()

    def test_rejects_expired_future_and_malformed_times(self):
        for change in ({"exp": 1099}, {"iat": 1200}, {"nbf": 1200}, {"exp": 2000}, {"iat": True}):
            self.request["token"] = self.token({**self.claims, **change})
            with self.assertRaises(broker.Refusal):
                self.verify()

    def test_digest_change_breaks_authorization(self):
        self.request["token"] = self.token()
        self.request["sha256"] = "c" * 64
        with self.assertRaises(broker.Refusal):
            self.verify()

    def test_rejects_algorithm_confusion(self):
        for alg in ("none", "HS256", "RS512"):
            self.request["token"] = self.token(header={"alg": alg, "kid": "fixture"})
            with self.assertRaises(broker.Refusal):
                self.verify()

    def test_rejects_invalid_signature(self):
        self.request["token"] = self.token()[:-10] + "AAAAAAAAAA"
        with self.assertRaises(Exception):
            self.verify()

    def test_rejects_command_path_environment_and_extra_fields(self):
        for name in ("command", "path", "environment", "docker_flags"):
            with self.assertRaises(broker.Refusal):
                broker.request_fields({**self.request, name: "anything"})
        for value in ("../../root", "a;id", "A" * 40, 1):
            with self.assertRaises(broker.Refusal):
                broker.request_fields({**self.request, "sha": value})

    def test_rejects_duplicate_json_keys(self):
        with self.assertRaises(broker.Refusal):
            broker.strict_json('{"sha":"a","sha":"b"}')

    def test_ci_provenance(self):
        run = {"id": 123, "name": "CI", "event": "push", "status": "completed",
               "conclusion": "success", "head_repository": {"id": int(broker.REPOSITORY_ID)},
               "head_branch": "migration/vozen-rust", "head_sha": self.request["sha"]}
        head = {"sha": self.request["sha"]}
        broker.verify_run(self.request, run, head)
        for key in run:
            with self.subTest(key=key):
                changed = copy.deepcopy(run)
                changed[key] = {"id": 0} if key == "head_repository" else "invalid"
                with self.assertRaises(broker.Refusal):
                    broker.verify_run(self.request, changed, head)
        with self.assertRaises(broker.Refusal):
            broker.verify_run(self.request, run, {"sha": "c" * 40})


class RecoveryTests(unittest.TestCase):
    def test_cancellation_is_a_recoverable_exception(self):
        with self.assertRaises(broker.Refusal):
            broker.cancelled(None, None)

    def test_backup_and_canary_before_supervisor_stop(self):
        source = pathlib.Path(broker.__file__).read_text()
        source = source[source.index("def deploy(request):"):]
        self.assertLess(source.index("canary(candidate)"), source.index("replace_runtime(candidate, previous)"))
        self.assertLess(source.index("backup()\n"), source.index("replace_runtime(candidate, previous)\n"))

    def test_failed_replacement_restores_previous_and_supervision(self):
        commands, images = [], []
        def compose(image):
            images.append(image)
            if image == "candidate":
                raise broker.Refusal("fixture failure")
        with patch.object(broker, "command", side_effect=lambda args, **kwargs: commands.append(args)), \
                patch.object(broker, "compose", side_effect=compose), \
                patch.object(broker, "wait_healthy") as health:
            with self.assertRaises(broker.Refusal):
                broker.replace_runtime("candidate", "previous")
        self.assertEqual(images, ["candidate", "previous"])
        self.assertIn(["/usr/bin/systemctl", "start", broker.UNIT], commands)
        health.assert_called_once()

    def test_supervision_restored_even_if_rollback_compose_fails(self):
        commands = []
        with patch.object(broker, "command", side_effect=lambda args, **kwargs: commands.append(args)), \
                patch.object(broker, "compose", side_effect=broker.Refusal("fixture failure")):
            with self.assertRaises(broker.Refusal):
                broker.replace_runtime("candidate", "previous")
        self.assertIn(["/usr/bin/systemctl", "start", broker.UNIT], commands)

    def test_clean_environment_has_no_caller_overrides(self):
        self.assertNotIn("PYTHONPATH", broker.CLEAN_ENV)
        self.assertNotIn("COMPOSE_FILE", broker.CLEAN_ENV)
        self.assertEqual(broker.CLEAN_ENV["DOCKER_HOST"], "unix:///var/run/docker.sock")

    def test_wrong_active_digest_attempts_image_recovery(self):
        images = []
        with patch.object(broker, "command"), patch.object(broker, "compose", side_effect=images.append), \
                patch.object(broker, "wait_healthy"), patch.object(broker, "live", return_value={"Image": "unexpected"}):
            with self.assertRaises(broker.Refusal):
                broker.replace_runtime("candidate", "previous")
        self.assertEqual(images, ["candidate", "previous"])


@unittest.skipUnless(os.name == "posix", "requires Linux no-follow dirfds")
class ArtifactTests(unittest.TestCase):
    def test_forced_publisher_command_scope(self):
        publisher_spec = importlib.util.spec_from_file_location("publisher", pathlib.Path(__file__).with_name("publish.py"))
        publisher = importlib.util.module_from_spec(publisher_spec)
        publisher_spec.loader.exec_module(publisher)
        self.assertEqual(publisher.parse("deploy"), ("deploy", None))
        self.assertEqual(publisher.parse("put " + "a" * 40), ("put", "a" * 40))
        for command in ("id", "bash", "sudo docker ps", "deploy;id", "put ../../root", "cleanup " + "A" * 40,
                        "put " + "a" * 40 + " extra", "scp -t /root", "internal-sftp"):
            with self.subTest(command=command), self.assertRaises(ValueError):
                publisher.parse(command)

    def test_regular_file_hash_and_symlink_rejection(self):
        import hashlib
        with tempfile.TemporaryDirectory() as root:
            directory = pathlib.Path(root) / "home/vozen/vozen-publish-incoming" / ("a" * 40)
            directory.mkdir(parents=True)
            source = directory / ("vozen-rust-" + "a" * 40 + ".tar.gz")
            content = b"fixture archive"
            source.write_bytes(content)
            request = {"sha": "a" * 40, "sha256": hashlib.sha256(content).hexdigest()}
            real_open = os.open
            def rooted(path, *args, **kwargs):
                return real_open(root if path == "/" else path, *args, **kwargs)
            with patch.object(broker.os, "open", side_effect=rooted):
                target = pathlib.Path(root) / "copy"
                broker.copy_artifact(request, target)
                self.assertEqual(target.read_bytes(), content)
                with self.assertRaises(broker.Refusal):
                    broker.copy_artifact({**request, "sha256": "0" * 64}, pathlib.Path(root) / "bad")
                source.unlink()
                source.symlink_to(target)
                with self.assertRaises(OSError):
                    broker.copy_artifact(request, pathlib.Path(root) / "symlink-copy")
                source.unlink()
                directory.rmdir()
                directory.symlink_to(pathlib.Path(root))
                with self.assertRaises(OSError):
                    broker.copy_artifact(request, pathlib.Path(root) / "parent-copy")


if __name__ == "__main__":
    unittest.main()
