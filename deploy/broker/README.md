# Restricted Vozen deployment broker

Host-specific implementation for `vps-4a10dd35`; do not install on other hosts.
The broker accepts **no command-line arguments** and one bounded JSON request on
stdin: `sha`, `run_id`, `sha256`, `token`. No shell commands, paths, environment
overrides, Docker flags, database restore or other services are exposed.

GitHub Actions OIDC must bind the SHA, successful CI run and archive digest in
the audience. The broker verifies the RSA signature using GitHub's fixed JWKS
endpoint, issuer, repository/owner immutable IDs, main ref, exact deployment
workflow, hosted runner, event, subject and expiry. It independently checks the
public CI metadata and current production branch. There are no GitHub access
tokens or administrative SSH keys on the publishing account.

The trust boundary includes repository writers able to modify the trusted main
workflow/production source and GitHub's signing service. A compromised trusted
workflow can authorize code for the bot. This is not a claim of complete host
security or an alternative to reviewing production changes.

Install code, validator, Compose and all their parents as root-owned and not
group/world writable. Runtime environment is a root-only snapshot inside the
unlocked encrypted volume. Models and Piper use root-owned read-only snapshots,
not mutable host paths from the account's checkout. Model updates require an
operator; regular image deploys do not update these snapshots.

Only `/usr/local/sbin/vozen-deploy-broker ""` is added to sudoers. The account
still cannot access the Docker socket, run general sudo, modify broker policy
or execute checkout scripts as root. Root's clean Docker environment, fixed
Compose project/container/mounts, UID 1000 and no-new-privileges constrain the
workload; container isolation is not a guarantee against kernel vulnerabilities.

Before cutover: lock, signed request, provenance, private artifact copy via
no-follow directory descriptors, checksum, bounded archive validation, image
revision/platform, credential-free canary, encrypted online SQLite backup and
retained previous image. Cutover stops only the encrypted Vozen supervisor and
recreates only its container. Failure attempts previous-image recovery and
restores supervision, never restoring an old database over new writes.

Staging, containerd content and overlayfs snapshots stay on the primary
filesystem. Space checks reserve the archive before copying, then both validated
content and snapshot allocations, including implicit parent directories, with
10% margin and 1 GiB spare. The authoritative OCI graph must match the Docker
image metadata before loading; archive and entry limits remain enforced.

Broker state is root-only and records the accepted CI revision/run/digest. An
identical verified retry checks health without recreating the container. A
manual runtime change requires operator reconciliation of state. Broker errors
do not disclose bearer tokens, configuration, user messages or Docker output.

After a healthy cutover and atomic state write, old `vozen-rust` images are
removed by exact digest with `--no-prune`, without force or global pruning. Untagged
parent images are not removed as a side effect. The verified current
image, broker-pinned rollback and every image referenced by a running or stopped
container are preserved. Images with another repository alias, no identifying
revision label or ambiguous metadata are retained. Docker may refuse an image
with multiple tags; such a refusal is reported as a cleanup warning, not a failed
release or a reason to roll back a healthy runtime. Verified retries also attempt
cleanup under the same deployment lock. No volume, data directory, database or
backup is a cleanup target.

The GitHub SSH key is dedicated to publication. Its authorized-keys entry uses
`restrict` and the root-owned forced command `vozen-publish-ssh`, which accepts
only `put SHA`, `deploy` and `cleanup SHA`. It cannot run a shell, SFTP/SCP,
forward ports/agents or choose another filesystem path. Existing unrelated
operator SSH keys are preserved; their permissions are not changed by this setup.

Checks:

```sh
python3 -m unittest discover -s deploy/broker -p 'test_*.py' -v
node deploy/broker/check_workflow.mjs
```

Python needs the distribution's `cryptography` package. Unit tests use ephemeral
fixture keys and mocked recovery operations; they do not replace the real
GitHub authentication/canary/replacement/health acceptance test.

References: [GitHub OIDC](https://docs.github.com/en/actions/reference/security/oidc),
[Docker security](https://docs.docker.com/engine/security/).
