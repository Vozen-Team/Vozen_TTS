"""Image-retention fixtures never contact Docker, GitHub, data or real backups."""
import copy
import importlib.util
import json
import pathlib
import types
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("retention_broker", pathlib.Path(__file__).with_name("broker.py"))
broker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(broker)


class ImageRetentionTests(unittest.TestCase):
    current = "sha256:" + "a" * 64
    rollback = "sha256:" + "b" * 64
    old = "sha256:" + "c" * 64
    extra = "sha256:" + "d" * 64

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = pathlib.Path(temporary.name)
        self.state = {"image": self.current, "rollback": self.rollback, "broker_verified": True}
        (self.root / "state.json").write_text(json.dumps(self.state))
        self.images = {}
        for index, image in enumerate([self.current, self.rollback, self.old, self.extra]):
            self.images[image] = {"Id": image, "RepoTags": ["vozen-rust:" + str(index) * 40],
                                  "Config": {"Labels": {"org.opencontainers.image.revision": str(index) * 40}}}
        self.images[self.rollback]["RepoTags"].append("vozen-rust:rollback")
        self.container = {"Id": "e" * 64, "Image": self.current,
                          "State": {"Health": {"Status": "healthy"}}}
        self.references = {"e" * 64: self.current}
        self.commands = []
        self.reference_reads = 0
        self.protect_old_later = False
        self.docker_failure = False
        state_patch = patch.object(broker, "STATE", self.root)
        live_patch = patch.object(broker, "live", side_effect=lambda: copy.deepcopy(self.container))
        docker_patch = patch.object(broker, "docker", side_effect=self.docker)
        for item in (state_patch, live_patch, docker_patch):
            item.start()
            self.addCleanup(item.stop)

    def docker(self, *arguments, **options):
        self.commands.append(arguments)
        if arguments == ("container", "ls", "--all", "--quiet", "--no-trunc"):
            self.reference_reads += 1
            if self.protect_old_later and self.reference_reads > 1:
                self.references["f" * 64] = self.old
            return "\n".join(self.references)
        if arguments[:2] == ("container", "inspect"):
            return json.dumps([{"Id": arguments[2], "Image": self.references[arguments[2]]}])
        if arguments == ("image", "ls", "--quiet", "--no-trunc", "vozen-rust"):
            return "\n".join(self.images)
        if arguments[:2] == ("image", "inspect"):
            image = self.rollback if arguments[2] == "vozen-rust:rollback" else arguments[2]
            return json.dumps([self.images[image]])
        if arguments[:2] == ("image", "rm"):
            self.assertEqual(len(arguments), 3)
            image = arguments[2]
            self.assertNotIn(image, (self.current, self.rollback))
            self.assertNotIn(image, self.references.values())
            if self.docker_failure:
                raise broker.Refusal("fixture Docker refusal")
            del self.images[image]
            return "Deleted: " + image
        self.fail("Unexpected Docker command: " + repr(arguments))

    def removals(self):
        return [item for item in self.commands if item[:2] == ("image", "rm")]

    def test_removes_only_exact_old_ids_and_retains_current_rollback(self):
        self.assertEqual(broker.cleanup_old_images(self.current, self.rollback), 2)
        self.assertEqual(set(self.images), {self.current, self.rollback})
        self.assertEqual(self.removals(), [("image", "rm", self.old), ("image", "rm", self.extra)])
        self.assertEqual(json.loads((self.root / "state.json").read_text()), self.state)
        self.assertFalse(any("prune" in item or "--force" in item or "volume" in item
                             for command in self.commands for item in command))

    def test_protects_running_and_stopped_container_references(self):
        self.references["f" * 64] = self.old
        broker.cleanup_old_images(self.current, self.rollback)
        self.assertIn(self.old, self.images)
        self.assertEqual(self.removals(), [("image", "rm", self.extra)])

    def test_rechecks_new_container_reference_before_removal(self):
        self.protect_old_later = True
        broker.cleanup_old_images(self.current, self.rollback)
        self.assertIn(self.old, self.images)
        self.assertEqual(self.removals(), [("image", "rm", self.extra)])

    def test_preserves_other_repository_aliases_and_untagged_images(self):
        self.images[self.old]["RepoTags"].append("unrelated:keep")
        self.images[self.extra]["RepoTags"] = []
        self.assertEqual(broker.cleanup_old_images(self.current, self.rollback), 0)
        self.assertEqual(self.removals(), [])

    def test_requires_vozen_revision_label(self):
        for image in (self.old, self.extra):
            self.images[image]["Config"]["Labels"] = {}
        self.assertEqual(broker.cleanup_old_images(self.current, self.rollback), 0)

    def test_state_mismatch_or_unhealthy_runtime_prevents_deletion(self):
        self.container["State"]["Health"]["Status"] = "unhealthy"
        with self.assertRaises(broker.Refusal):
            broker.cleanup_old_images(self.current, self.rollback)
        self.assertEqual(self.removals(), [])
        self.container["State"]["Health"]["Status"] = "healthy"
        (self.root / "state.json").write_text(json.dumps({**self.state, "rollback": self.old}))
        with self.assertRaises(broker.Refusal):
            broker.cleanup_old_images(self.current, self.rollback)
        self.assertEqual(self.removals(), [])

    def test_malformed_image_inventory_prevents_deletion(self):
        self.images["invalid"] = self.images.pop(self.old)
        with self.assertRaises(broker.Refusal):
            broker.cleanup_old_images(self.current, self.rollback)
        self.assertEqual(self.removals(), [])

    def test_candidate_identity_mismatch_prevents_deletion(self):
        self.images[self.old]["Id"] = self.current
        with self.assertRaises(broker.Refusal):
            broker.cleanup_old_images(self.current, self.rollback)
        self.assertEqual(self.removals(), [])

    def test_rollback_tag_mismatch_prevents_deletion(self):
        with patch.object(broker, "inspect_image", return_value={"Id": self.old}):
            with self.assertRaises(broker.Refusal):
                broker.cleanup_old_images(self.current, self.rollback)
        self.assertEqual(self.removals(), [])

    def test_changed_runtime_prevents_deletion(self):
        with patch.object(broker, "retention_guard", side_effect=["e" * 64, "f" * 64]):
            with self.assertRaises(broker.Refusal):
                broker.cleanup_old_images(self.current, self.rollback)
        self.assertEqual(self.removals(), [])

    def test_cleanup_is_idempotent(self):
        broker.cleanup_old_images(self.current, self.rollback)
        self.assertEqual(broker.cleanup_old_images(self.current, self.rollback), 0)

    def test_docker_refusal_warns_without_failing_successful_release(self):
        self.docker_failure = True
        with patch("builtins.print") as output:
            broker.cleanup_after_verified_deploy(self.current, self.rollback)
        self.assertTrue(any("Warning:" in str(call) for call in output.call_args_list))
        self.assertEqual(len(self.images), 4)

    def test_cleanup_never_opens_database_or_calls_backup_or_runtime_replacement(self):
        with patch.object(broker.sqlite3, "connect", side_effect=AssertionError("database touched")), \
                patch.object(broker, "backup", side_effect=AssertionError("backup touched")), \
                patch.object(broker, "replace_runtime", side_effect=AssertionError("runtime touched")):
            broker.cleanup_old_images(self.current, self.rollback)


class DeploymentRetentionHookTests(unittest.TestCase):
    def exercise(self, failure=False, retry=False):
        current, candidate, rollback = ["sha256:" + c * 64 for c in "abc"]
        request = {"sha": "a" * 40, "run_id": "200", "sha256": "b" * 64, "token": "fixture"}
        before = {"image": current, "rollback": rollback, "run_id": "100", "sha": "c" * 40}
        if retry:
            before.update(sha=request["sha"], run_id=request["run_id"],
                          archive_sha256=request["sha256"], broker_verified=True)
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            (root / "state.json").write_text(json.dumps(before))
            events = []
            def cleanup(new, old):
                state = json.loads((root / "state.json").read_text())
                self.assertEqual((state["image"], state["rollback"]), (new, old))
                events.append("cleanup")
            def replace(new, old):
                events.append("replace")
                if failure:
                    raise broker.Refusal("fixture failed replacement")
            patches = [
                patch.dict("sys.modules", {"fcntl": types.SimpleNamespace(flock=lambda *a: None, LOCK_EX=1, LOCK_NB=2)}),
                patch.object(broker, "STATE", root),
                patch.object(broker.sys, "argv", ["broker.py"]),
                patch.object(broker.os, "geteuid", return_value=0, create=True),
                patch.object(broker.socket, "gethostname", return_value="vps-4a10dd35"),
                patch.object(broker, "trusted_path"), patch.object(broker, "command"),
                patch.object(broker, "verify_oidc"), patch.object(broker, "verify_run"),
                patch.object(broker, "get_json", return_value={}),
                patch.object(broker, "live", return_value={"Image": current}),
                patch.object(broker.shutil, "disk_usage", return_value=types.SimpleNamespace(free=50 * 1024**3)),
                patch.object(broker, "copy_artifact"),
                patch.object(broker, "load_validator", return_value=lambda *a: None),
                patch.object(broker, "docker"),
                patch.object(broker, "inspect_image", return_value={"Id": candidate, "Os": "linux", "Architecture": "amd64", "Config": {"Labels": {"org.opencontainers.image.revision": request["sha"]}}}),
                patch.object(broker, "canary"), patch.object(broker, "backup"),
                patch.object(broker, "replace_runtime", side_effect=replace),
                patch.object(broker, "wait_healthy", side_effect=lambda: events.append("healthy")),
                patch.object(broker, "cleanup_after_verified_deploy", side_effect=cleanup),
                patch("builtins.print"),
            ]
            from contextlib import ExitStack
            with ExitStack() as stack:
                for item in patches:
                    stack.enter_context(item)
                if failure:
                    with self.assertRaises(broker.Refusal):
                        broker.deploy(request)
                    self.assertEqual(events, ["replace"])
                    self.assertEqual(json.loads((root / "state.json").read_text()), before)
                else:
                    broker.deploy(request)
                    self.assertEqual(events, ["healthy", "cleanup"] if retry else ["replace", "cleanup"])

    def test_successful_cutover_writes_state_before_cleanup(self):
        self.exercise()

    def test_failed_cutover_never_cleans_images(self):
        self.exercise(failure=True)

    def test_verified_retry_checks_health_before_cleanup_without_replacement(self):
        self.exercise(retry=True)


if __name__ == "__main__":
    unittest.main()
