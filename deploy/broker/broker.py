"""Fixed-target Vozen deploy broker. Requests are signed by GitHub Actions OIDC."""
from __future__ import annotations

import base64
import hashlib
import importlib.util
import json
import os
import pathlib
import re
import select
import signal
import shutil
import socket
import sqlite3
import stat
import subprocess
import sys
import tempfile
import time
import urllib.request

from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import padding, rsa

REPOSITORY = "Vozen-Team/Vozen_TTS"
REPOSITORY_ID = "1308788653"
OWNER_ID = "343175426"
ISSUER = "https://token.actions.githubusercontent.com"
LIB = pathlib.Path("/usr/local/lib/vozen-deploy-broker")
STATE = pathlib.Path("/var/lib/vozen-deploy-broker")
SECURE = pathlib.Path("/srv/vozen-secure/broker")
CONTAINER = "vozen-rust-prod-vozen-1"
UNIT = "vozen-encrypted-runtime.service"
MAX_ARCHIVE = 2 * 1024**3
CLEAN_ENV = {"PATH": "/usr/sbin:/usr/bin:/sbin:/bin", "HOME": "/root", "LC_ALL": "C",
             "DOCKER_HOST": "unix:///var/run/docker.sock",
             "DOCKER_CONFIG": str(STATE / "docker-config")}


class Refusal(RuntimeError):
    """A fixed, non-sensitive operator diagnostic."""


def require(condition, message):
    if not condition:
        raise Refusal(message)


def strict_json(raw):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            require(key not in result, "duplicate JSON key")
            result[key] = value
        return result
    return json.loads(raw, object_pairs_hook=unique)


def decode(value):
    require(isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9_-]+", value),
            "invalid base64url")
    return base64.urlsafe_b64decode(value + "=" * (-len(value) % 4))


def get_json(url):
    request = urllib.request.Request(url, headers={"User-Agent": "Vozen-deploy-broker",
                                                  "Accept": "application/json"})
    with urllib.request.urlopen(request, timeout=20) as response:
        require(response.geturl() == url, "unexpected metadata redirect")
        raw = response.read(1024 * 1024 + 1)
    require(len(raw) <= 1024 * 1024, "metadata size limit")
    return strict_json(raw)


def request_fields(request):
    require(isinstance(request, dict), "request must be an object")
    require(set(request) == {"sha", "run_id", "sha256", "token"}, "invalid request fields")
    for name, pattern in (("sha", r"[0-9a-f]{40}"), ("run_id", r"[1-9][0-9]{0,19}"),
                          ("sha256", r"[0-9a-f]{64}")):
        require(isinstance(request[name], str) and re.fullmatch(pattern, request[name]),
                "invalid request identifier")
    require(isinstance(request["token"], str) and len(request["token"]) <= 16000,
            "invalid token size")
    return "vozen-deploy:{sha}:{run_id}:{sha256}".format(**request)


def verify_oidc(request, jwks, now):
    audience = request_fields(request)
    parts = request["token"].split(".")
    require(len(parts) == 3, "invalid token structure")
    header, claims = [strict_json(decode(value)) for value in parts[:2]]
    require(isinstance(header, dict) and isinstance(claims, dict), "invalid JWT objects")
    require(header.get("alg") == "RS256" and not header.get("crit"), "unsupported JWT algorithm")
    keys = [key for key in jwks["keys"] if key.get("kid") == header.get("kid")
            and key.get("kty") == "RSA" and key.get("use") == "sig"]
    require(len(keys) == 1, "unknown signing key")
    key = keys[0]
    public = rsa.RSAPublicNumbers(int.from_bytes(decode(key["e"]), "big"),
                                 int.from_bytes(decode(key["n"]), "big")).public_key()
    public.verify(decode(parts[2]), (parts[0] + "." + parts[1]).encode(),
                  padding.PKCS1v15(), hashes.SHA256())
    expected = {"iss": ISSUER, "aud": audience, "repository": REPOSITORY,
                "repository_id": REPOSITORY_ID, "repository_owner_id": OWNER_ID,
                "ref": "refs/heads/main", "runner_environment": "github-hosted",
                "workflow_ref": REPOSITORY + "/.github/workflows/deploy-bot.yml@refs/heads/main"}
    require(all(claims.get(key) == value for key, value in expected.items()), "OIDC trust mismatch")
    require(claims.get("event_name") in {"workflow_dispatch", "workflow_run"}, "untrusted event")
    subjects = {"repo:" + REPOSITORY + ":ref:refs/heads/main",
                "repo:Vozen-Team@" + OWNER_ID + "/Vozen_TTS@" + REPOSITORY_ID + ":ref:refs/heads/main"}
    require(claims.get("sub") in subjects, "untrusted subject")
    for name in ("exp", "iat", "nbf"):
        require(type(claims.get(name)) is int, "invalid token time")
    require(claims["nbf"] <= now + 30 and claims["iat"] <= now + 30
            and claims["iat"] >= now - 600 and claims["exp"] > now
            and 0 < claims["exp"] - claims["iat"] <= 600, "expired or invalid token time")
    return claims


def verify_run(request, run, head):
    require(run.get("id") == int(request["run_id"]) and run.get("name") == "CI"
            and run.get("event") == "push" and run.get("status") == "completed"
            and run.get("conclusion") == "success"
            and run.get("head_repository", {}).get("id") == int(REPOSITORY_ID)
            and run.get("head_branch") == "migration/vozen-rust"
            and run.get("head_sha") == request["sha"], "CI provenance mismatch")
    require(head.get("sha") == request["sha"], "production revision has moved")


def command(arguments, *, env=None, timeout=90, merge_output=False):
    result = subprocess.run(arguments, env=env or CLEAN_ENV, cwd="/", stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout)
    require(result.returncode == 0, "fixed deployment command failed: " + pathlib.Path(arguments[0]).name)
    output = result.stdout + (result.stderr if merge_output else b"")
    return output.decode("utf-8", errors="replace").strip()


def docker(*arguments, **options):
    return command(["/usr/bin/docker", *arguments], **options)


def trusted_path(path, directory=False):
    for part in (path, *path.parents):
        info = part.lstat()
        require(not stat.S_ISLNK(info.st_mode) and info.st_uid == 0
                and not info.st_mode & 0o022, "untrusted broker filesystem")
    require(path.is_dir() if directory else path.is_file(), "missing broker filesystem")


def copy_artifact(request, target):
    # Walk through dirfds, never following attacker-controlled symlinks. A file
    # renamed during transfer is still the same opened inode; its copied hash
    # must match the signed audience before any Docker call.
    parts = ["home", "vozen", "vozen-publish-incoming", request["sha"],
             "vozen-rust-" + request["sha"] + ".tar.gz"]
    descriptor = os.open("/", os.O_RDONLY | os.O_DIRECTORY)
    try:
        for part in parts[:-1]:
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
        file_descriptor = os.open(parts[-1], os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK,
                                  dir_fd=descriptor)
        with os.fdopen(file_descriptor, "rb") as source, target.open("xb") as destination:
            info = os.fstat(source.fileno())
            require(stat.S_ISREG(info.st_mode) and 0 < info.st_size <= MAX_ARCHIVE,
                    "invalid artifact file")
            digest, total = hashlib.sha256(), 0
            while chunk := source.read(1024 * 1024):
                total += len(chunk)
                require(total <= MAX_ARCHIVE, "artifact size limit")
                digest.update(chunk)
                destination.write(chunk)
            require(total == info.st_size and digest.hexdigest() == request["sha256"],
                    "artifact checksum mismatch")
    finally:
        os.close(descriptor)


def load_validator():
    path = LIB / "validate_archive.py"
    trusted_path(path)
    spec = importlib.util.spec_from_file_location("vozen_archive_validator", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.validate_archive


def inspect_image(image):
    result = strict_json(docker("image", "inspect", image))
    require(len(result) == 1 and re.fullmatch(r"sha256:[0-9a-f]{64}", result[0]["Id"]),
            "invalid image metadata")
    return result[0]


def live():
    return strict_json(docker("container", "inspect", CONTAINER))[0]


def retention_guard(current, rollback):
    """Called under the deployment lock, only after a verified successful cutover."""
    for image in (current, rollback):
        require(isinstance(image, str) and re.fullmatch(r"sha256:[0-9a-f]{64}", image),
                "invalid retained image")
    state = strict_json((STATE / "state.json").read_bytes())
    require(state.get("broker_verified") is True and state.get("image") == current
            and state.get("rollback") == rollback, "image retention state mismatch")
    container = live()
    require(container["Image"] == current
            and container["State"].get("Health", {}).get("Status") == "healthy",
            "image retention requires healthy verified runtime")
    require(inspect_image("vozen-rust:rollback")["Id"] == rollback,
            "image retention rollback tag mismatch")
    return container["Id"]


def container_images():
    """Protect running AND stopped containers, including unrelated Docker workloads."""
    images = set()
    for identifier in docker("container", "ls", "--all", "--quiet", "--no-trunc").split():
        require(re.fullmatch(r"[0-9a-f]{64}", identifier), "invalid container identifier")
        containers = strict_json(docker("container", "inspect", identifier))
        require(isinstance(containers, list) and len(containers) == 1
                and containers[0].get("Id") == identifier, "invalid container metadata")
        image = containers[0].get("Image")
        require(isinstance(image, str) and re.fullmatch(r"sha256:[0-9a-f]{64}", image),
                "invalid container image")
        images.add(image)
    return images


def old_vozen_image(metadata):
    tags = metadata.get("RepoTags")
    if not tags:
        return False  # An untagged image has no reliable repository ownership here.
    require(isinstance(tags, list) and all(isinstance(tag, str) for tag in tags),
            "invalid image tags")
    if not all(re.fullmatch(r"vozen-rust:[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}", tag)
               for tag in tags):
        return False  # Preserve images with aliases in another repository.
    revision = metadata.get("Config", {}).get("Labels", {}).get("org.opencontainers.image.revision")
    return isinstance(revision, str) and re.fullmatch(r"[0-9a-f]{40}", revision) is not None


def cleanup_old_images(current, rollback):
    """Remove exact unused Vozen image IDs; never prune or operate on volumes/files."""
    container_id = retention_guard(current, rollback)
    identifiers = set(docker("image", "ls", "--quiet", "--no-trunc", "vozen-rust").split())
    require(all(re.fullmatch(r"sha256:[0-9a-f]{64}", image) for image in identifiers),
            "invalid image listing")
    require({current, rollback} <= identifiers, "retained Vozen image is missing")
    protected = {current, rollback} | container_images()
    candidates = []
    for image in sorted(identifiers - protected):
        metadata = inspect_image(image)
        require(metadata["Id"] == image, "candidate image identity mismatch")
        if old_vozen_image(metadata):
            candidates.append(image)
    removed = 0
    for image in candidates:
        require(retention_guard(current, rollback) == container_id,
                "runtime changed during image retention")
        if image in container_images():
            continue
        metadata = inspect_image(image)
        require(metadata["Id"] == image, "candidate image identity changed")
        if not old_vozen_image(metadata):
            continue
        # Removing an ID without --force lets Docker refuse newly referenced or multiply
        # tagged images. Never untag by name: an operator could have moved that tag.
        docker("image", "rm", image)
        removed += 1
    require(retention_guard(current, rollback) == container_id,
            "runtime changed after image retention")
    return removed


def cleanup_after_verified_deploy(current, rollback):
    # A capacity-maintenance failure must not turn an already healthy cutover into a
    # failed deployment, trigger a rollback, or interfere with archive cleanup.
    try:
        removed = cleanup_old_images(current, rollback)
        print("Old Vozen image cleanup completed: " + str(removed) + " image(s) removed.", flush=True)
    except Exception:
        print("Warning: old Vozen image cleanup skipped or incomplete; inspect disk capacity.",
              flush=True)


def database_check(connection):
    require(connection.execute("PRAGMA integrity_check").fetchone() == ("ok",)
            and not connection.execute("PRAGMA foreign_key_check").fetchall(), "database verification failed")


def backup():
    source = pathlib.Path("/srv/vozen-secure/data/tts.db")
    require(source.resolve() == source and source.is_file(), "unexpected database path")
    descriptor, name = tempfile.mkstemp(prefix="predeploy-", suffix=".db", dir=SECURE)
    os.close(descriptor)
    with sqlite3.connect(source.as_uri() + "?mode=ro", uri=True, timeout=30) as original:
        database_check(original)
        with sqlite3.connect(name) as copy:
            original.backup(copy)
            database_check(copy)
    print("Encrypted online database backup verified.", flush=True)


def compose(image):
    env = {**CLEAN_ENV, "VOZEN_BROKER_IMAGE": image}
    command(["/usr/bin/docker", "compose", "--env-file", "/dev/null", "-p", "vozen-rust-prod",
             "-f", str(LIB / "compose.yml"), "up", "-d", "--force-recreate", "--no-build", "vozen"],
            env=env, timeout=180)


def wait_healthy():
    for _ in range(48):
        try:
            container = live()
            if container["State"].get("Health", {}).get("Status") == "healthy":
                with urllib.request.urlopen("http://127.0.0.1:3001/health", timeout=5) as response:
                    require(response.geturl() == "http://127.0.0.1:3001/health", "health redirect")
                    healthy = strict_json(response.read(4096)).get("status") == "ok"
                if healthy and "healthy: Ready" in docker("logs", "--tail", "400", CONTAINER, merge_output=True):
                    return
        except Exception:
            pass
        time.sleep(5)
    raise Refusal("runtime health verification failed")


def canary(image):
    name = "vozen-broker-canary"
    require(not docker("ps", "-aq", "--filter", "name=^/" + name + "$"), "canary name already exists")
    created = False
    try:
        docker("run", "-d", "--name", name, "--network", "none", "--read-only",
               "--user", "1000:1000", "--security-opt", "no-new-privileges:true",
               "--cap-drop", "ALL", "--memory", "512m", "--cpus", "1",
               "--tmpfs", "/tmp:rw,noexec,nosuid,size=64m",
               "--entrypoint", "/usr/local/bin/vozen-runtime",
               "-e", "RUST_IMAGE_SMOKE=true", "-e", "HEALTH_PORT=8080", image)
        created = True
        for _ in range(60):
            try:
                docker("exec", name, "/usr/bin/curl", "-fsS", "--max-time", "2",
                       "http://127.0.0.1:8080/health")
                print("Credential-free canary verified.", flush=True)
                return
            except Refusal:
                time.sleep(1)
        raise Refusal("canary verification failed")
    finally:
        if created:
            docker("rm", "--force", name)


def replace_runtime(candidate, previous):
    # Keep image recovery separate from database restoration. Stop supervision
    # only after artifact, canary and online backup checks have passed.
    supervisor_stopped = False
    replaced = False
    try:
        supervisor_stopped = True
        command(["/usr/bin/systemctl", "stop", UNIT])
        replaced = True
        compose(candidate)
        command(["/usr/bin/systemctl", "reset-failed", UNIT])
        command(["/usr/bin/systemctl", "start", UNIT])
        supervisor_stopped = False
        wait_healthy()
        require(live()["Image"] == candidate, "active image does not match accepted candidate")
        with sqlite3.connect("file:/srv/vozen-secure/data/tts.db?mode=ro", uri=True, timeout=30) as db:
            database_check(db)
    except Exception:
        try:
            if replaced:
                supervisor_stopped = True
                command(["/usr/bin/systemctl", "stop", UNIT])
                compose(previous)
        finally:
            if supervisor_stopped:
                command(["/usr/bin/systemctl", "reset-failed", UNIT])
                command(["/usr/bin/systemctl", "start", UNIT])
                supervisor_stopped = False
        if replaced:
            wait_healthy()
        raise


def deploy(request):
    import fcntl
    require(os.geteuid() == 0 and socket.gethostname() == "vps-4a10dd35", "wrong broker host/account")
    require(len(sys.argv) == 1, "arguments are not accepted")
    trusted_path(LIB, directory=True)
    trusted_path(LIB / "broker.py")
    trusted_path(LIB / "compose.yml")
    trusted_path(STATE, directory=True)
    trusted_path(STATE / "models", directory=True)
    trusted_path(STATE / "piper", directory=True)
    command(["/usr/local/sbin/vozen-data-guard"])
    trusted_path(SECURE, directory=True)
    trusted_path(SECURE / "runtime.env")
    request_fields(request)
    verify_oidc(request, get_json(ISSUER + "/.well-known/jwks"), int(time.time()))
    verify_run(request, get_json("https://api.github.com/repos/" + REPOSITORY + "/actions/runs/" + request["run_id"]),
               get_json("https://api.github.com/repos/" + REPOSITORY + "/commits/migration/vozen-rust"))
    with (STATE / "lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        state = strict_json((STATE / "state.json").read_bytes())
        require(int(request["run_id"]) >= int(state["run_id"]), "stale CI request")
        previous = live()["Image"]
        require(previous == state["image"], "runtime changed outside broker; operator reconciliation required")
        if (state.get("broker_verified") is True and state.get("sha") == request["sha"]
                and state.get("run_id") == request["run_id"]
                and state.get("archive_sha256") == request["sha256"]):
            wait_healthy()
            cleanup_after_verified_deploy(state["image"], state.get("rollback"))
            print("Vozen revision already deployed and verified; no replacement.", flush=True)
            return
        require(shutil.disk_usage(STATE).free > MAX_ARCHIVE + 8 * 1024**3 + 1024**3,
                "insufficient deployment disk headroom")
        staging = pathlib.Path(tempfile.mkdtemp(prefix="stage-", dir=STATE))
        archive = staging / "image.tar.gz"
        try:
            copy_artifact(request, archive)
            load_validator()(archive, request["sha"])
            docker("load", "--input", str(archive), timeout=900)
            metadata = inspect_image("vozen-rust:" + request["sha"])
            require(metadata.get("Os") == "linux" and metadata.get("Architecture") == "amd64"
                    and metadata["Config"].get("Labels", {}).get("org.opencontainers.image.revision") == request["sha"],
                    "candidate image mismatch")
            candidate = metadata["Id"]
            canary(candidate)
            backup()
            docker("image", "tag", previous, "vozen-rust:rollback")
            replace_runtime(candidate, previous)
            new_state = {"sha": request["sha"], "run_id": request["run_id"], "image": candidate,
                         "rollback": previous, "time": int(time.time()), "broker_verified": True,
                         "archive_sha256": request["sha256"]}
            descriptor, temporary = tempfile.mkstemp(prefix="state-", dir=STATE)
            with os.fdopen(descriptor, "w") as stream:
                json.dump(new_state, stream)
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temporary, STATE / "state.json")
            print("Verified Vozen deployment: " + request["sha"] + " " + candidate, flush=True)
            cleanup_after_verified_deploy(candidate, previous)
        finally:
            archive.unlink(missing_ok=True)
            staging.rmdir()


def cancelled(_signal, _frame):
    raise Refusal("deployment interrupted")


def main():
    try:
        signal.signal(signal.SIGTERM, cancelled)
        signal.signal(signal.SIGINT, cancelled)
        deadline, raw = time.monotonic() + 30, bytearray()
        while True:
            remaining = deadline - time.monotonic()
            require(remaining > 0 and select.select([sys.stdin.fileno()], [], [], remaining)[0],
                    "request input timeout")
            chunk = os.read(sys.stdin.fileno(), 20001 - len(raw))
            if not chunk:
                break
            raw.extend(chunk)
            require(len(raw) <= 20000, "request size limit")
        deploy(strict_json(raw))
    except Exception as error:
        # Never echo bearer tokens, private environment, user messages, or raw
        # external-command output. Detailed state is inspected by the operator.
        reason = str(error) if isinstance(error, Refusal) else type(error).__name__
        print("Vozen broker refused or failed: " + reason, file=sys.stderr)
        raise SystemExit(1)


if __name__ == "__main__":
    main()
