import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { describe, expect, it } from 'vitest';
const source = (path) => readFileSync(resolve(process.cwd(), path), { encoding: 'utf8' });
// The site's assets are cache-busted by FILENAME (never a query string), so every rename churns
// these tests too. One constant each: the rename is then a one-line edit here, not a hunt.
const SITE_JS = 'site/js/main-v51.js';
const SITE_I18N = 'site/js/i18n-v41.js';
const SITE_CSS = 'site/css/main-v43.css';
const ACCOUNT_CSS = 'site/css/account-v6.css';
const BILLING_CSS = 'site/css/billing-v3.css';
/** Body of a top-level function in the site bundle, comments stripped. Comments are dropped
 *  because these assertions are about the markup a function RENDERS — a comment explaining why
 *  some wiring is avoided must not read as using it. */
const fnSource = (script, signature) => {
  const start = script.indexOf(signature);
  if (start < 0) throw new Error(`not found in site bundle: ${signature}`);
  const rest = script.slice(start + 1);
  const end = rest.indexOf('\n  function ');
  return (end < 0 ? rest : rest.slice(0, end)).replace(/^\s*\/\/.*$/gm, '');
};
const claimCardSource = () => fnSource(source(SITE_JS), 'function claimCard()');
const helpModalSource = () => fnSource(source(SITE_JS), 'function claimHelpModal()');
const i18nBundle = () => {
  const sandbox = {
    window: {},
  };
  new Function('window', source(SITE_I18N))(sandbox.window);
  return sandbox.window.VOZEN_I18N ?? {};
};
describe('operational security configuration', () => {
  it('accepts only the expected CI-built Docker archive', () => {
    const root = mkdtempSync(join(tmpdir(), 'vozen-image-archive-'));
    const python = process.platform === 'win32' ? 'python' : 'python3';
    const revision = 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb';
    const generator = `
import io, json, sys, tarfile
archive, revision, tag, label, unsafe = sys.argv[1:]
with tarfile.open(archive, "w:gz") as image:
    def add(name, payload):
        payload = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        member = tarfile.TarInfo(name)
        member.size = len(payload)
        image.addfile(member, io.BytesIO(payload))
    add("manifest.json", [{"Config": "config.json", "RepoTags": [tag], "Layers": ["layer.tar"]}])
    add("config.json", {"config": {"Labels": {"org.opencontainers.image.revision": label}}})
    add("layer.tar", b"")
    if unsafe:
        add(unsafe, b"unsafe")
`;
    const makeArchive = ({
      tag = `vozen-rust:${revision}`,
      label = revision,
      unsafe = '',
      name = 'image.tar.gz',
    } = {}) => {
      const archive = join(root, name);
      const created = spawnSync(python, ['-c', generator, archive, revision, tag, label, unsafe], {
        encoding: 'utf8',
      });
      expect(created.status, created.stderr).toBe(0);
      return archive;
    };
    const validate = (archive) =>
      spawnSync(python, ['scripts/validate-docker-image-archive.py', archive, revision], {
        cwd: process.cwd(),
        encoding: 'utf8',
      });

    try {
      expect(validate(makeArchive()).status).toBe(0);

      const wrongTag = validate(makeArchive({ tag: 'vozen-rust:wrong', name: 'wrong-tag.tar.gz' }));
      expect(wrongTag.status).toBe(1);
      expect(wrongTag.stderr).toContain('exactly the expected tag');

      const wrongLabel = validate(
        makeArchive({ label: revision.replace(/^b/, 'a'), name: 'wrong-label.tar.gz' }),
      );
      expect(wrongLabel.status).toBe(1);
      expect(wrongLabel.stderr).toContain('revision label mismatch');

      const traversal = validate(makeArchive({ unsafe: '../escape', name: 'traversal.tar.gz' }));
      expect(traversal.status).toBe(1);
      expect(traversal.stderr).toContain('unsafe production image artifact entry');
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  it('gates pull requests and GitHub Pages with the same site verification command', () => {
    const pkg = JSON.parse(source('package.json'));
    const ci = source('.github/workflows/ci.yml');
    const pages = source('.github/workflows/pages.yml');
    expect(pkg.scripts?.['check:site']).toContain(
      'vitest run site-tests/operationalHardening.test.mjs site-tests/siteTrust.test.mjs site-tests/siteI18n.test.mjs site-tests/dashboardCoreSettings.test.mjs site-tests/siteUxPolish.test.mjs site-tests/fullAuditRegression.test.mjs',
    );
    expect(pkg.scripts?.['check:site']).toContain('--pool forks --maxWorkers 1 --minWorkers 1');
    expect(ci).toMatch(/\n {2}site:\s*\n/);
    expect(ci).toMatch(/\n\s+- run: npm run check:site\s*\n/);
    expect(pages).toMatch(/\n\s+- run: npm run check:site\s*\n/);
    expect(pages).not.toMatch(/\n\s+run: npm run build:site\s*\n/);
  });
  it('keeps Pages path filters anchored to existing files or directories', () => {
    const pages = source('.github/workflows/pages.yml');
    const paths = [...pages.matchAll(/^\s+- '([^']+)'\s*$/gm)].map((match) => match[1]);
    expect(paths.length).toBeGreaterThan(0);
    for (const path of paths) {
      const directory = path.endsWith('/**') ? path.slice(0, -3) : path;
      expect(existsSync(resolve(process.cwd(), directory)), path).toBe(true);
    }
    expect(pages).not.toContain('tests/operationalHardening.test.ts');
  });
  it('diagnoses VPS deployment inputs before opening the SSH action', () => {
    const deploy = source('.github/workflows/deploy-bot.yml');
    expect(deploy).toContain('name: Validate VPS deploy inputs');
    expect(deploy).toContain('Missing VPS_HOST');
    expect(deploy).toContain('Missing VPS_USER');
    expect(deploy).toContain('Missing VPS_SSH_KEY');
    expect(deploy).toContain('debug: false');
  });
  it('rebuilds a pruned rollback image from trusted source without snapshotting user data', () => {
    const bash =
      process.env.VOZEN_TEST_BASH ??
      (process.platform === 'win32' ? 'C:/Program Files/Git/bin/bash.exe' : 'bash');
    if (process.platform === 'win32' && !existsSync(bash)) {
      throw new Error('Set VOZEN_TEST_BASH to a Git Bash-compatible executable.');
    }
    const deployScript = resolve(process.cwd(), 'scripts/deploy-rust-vps.sh').replaceAll('\\', '/');
    const rollbackSha = '28f3b1f14b8a0434599c71ca223540979c47534d';
    const targetSha = 'd0ad45df15cdbbe4dcf9117218efc5178c016b70';
    const runFixture = ({
      healthy,
      backupFails = false,
      cleanupFails = false,
      containerPresent = true,
      metadataPresent = false,
      prebuilt = false,
      prebuiltRevision = targetSha,
      rollbackBuildFails = false,
      rollbackHealthy = true,
      rollbackImageRevision = '',
      sourceMode = 'override',
      stateDirSymlink = false,
    }) => {
      const root = mkdtempSync(join(tmpdir(), 'vozen-deploy-'));
      try {
        const deployDir = join(root, 'deploy');
        const fakeBin = join(root, 'bin');
        const dockerLog = join(root, 'docker.log');
        const gitLog = join(root, 'git.log');
        const prodTagState = join(root, 'prod-tag');
        const rollbackState = join(root, 'rollback-active');
        const stateDir = join(root, 'state');
        const stateOpsLog = join(root, 'state-ops.log');
        mkdirSync(join(deployDir, 'rust-data'), { recursive: true });
        mkdirSync(fakeBin, { recursive: true });
        writeFileSync(join(deployDir, '.env.rust.prod'), 'DISCORD_TOKEN=test\n');
        writeFileSync(join(deployDir, 'rust-data', 'tts.db'), 'fixture');
        if (stateDirSymlink) {
          const stateTarget = join(root, 'state-target');
          mkdirSync(stateTarget);
          symlinkSync(stateTarget, stateDir, process.platform === 'win32' ? 'junction' : 'dir');
        }
        if (sourceMode === 'state') {
          mkdirSync(stateDir, { recursive: true });
          writeFileSync(join(stateDir, 'deployed-sha'), `${rollbackSha}\n`);
        }
        const writeCommand = (name, body) => {
          const command = join(fakeBin, name);
          writeFileSync(command, `#!/usr/bin/env bash\n${body}\n`, { mode: 0o755 });
          chmodSync(command, 0o755);
        };
        writeCommand('systemctl', 'exit 1');
        writeCommand(
          'install',
          'printf \'install %s\\n\' "$*" >> "$FAKE_STATE_OPS_LOG"\nmkdir -p "$4"',
        );
        writeCommand('chmod', 'printf \'chmod %s\\n\' "$*" >> "$FAKE_STATE_OPS_LOG"\nexit 0');
        writeCommand(
          'docker',
          String.raw`printf '%s\n' "$*" >> "$FAKE_DOCKER_LOG"
if [ "$1" = "container" ] && [ "$2" = "inspect" ]; then
  [ "$3" != "--format" ] && [ "$FAKE_CONTAINER_PRESENT" != "true" ] && exit 1
  if [ "$3" = "--format" ]; then
    if [ "$4" = "{{.Image}}" ]; then
      echo "$FAKE_IMAGE_ID"
    elif [[ "$4" == *"org.opencontainers.image.revision"* ]]; then
      echo "$FAKE_LABEL_SHA"
    elif [ "$FAKE_HEALTHY" = "true" ] || [ -f "$FAKE_ROLLBACK_STATE" ]; then
      echo "healthy"
    else
      echo "unhealthy"
    fi
  fi
  exit 0
fi
if [ "$1" = "image" ] && [ "$2" = "tag" ]; then
  [ "$3" = "sha256:missing" ] && exit 1
  [ "$4" = "vozen-rust:prod" ] && printf '%s\n' "$3" > "$FAKE_PROD_TAG_STATE"
  exit 0
fi
if [ "$1" = "image" ] && [ "$2" = "inspect" ] && [ "$3" = "--format" ]; then
  [ "$5" = "vozen-rust:artifact" ] && echo "$FAKE_PREBUILT_SHA"
  [ "$5" = "vozen-rust:rollback" ] && echo "$FAKE_ROLLBACK_IMAGE_SHA"
  [ "$5" = "vozen-rust:prod" ] && echo "$FAKE_PREVIOUS_PROD_ID"
  exit 0
fi
if [ "$1" = "build" ] && [[ "$*" == *"--tag vozen-rust:rollback"* ]]; then
  [ "$FAKE_ROLLBACK_BUILD_FAILS" = "true" ] && exit 1
fi
if [ "$1" = "logs" ]; then
  if [ "$FAKE_HEALTHY" = "true" ] || [ -f "$FAKE_ROLLBACK_STATE" ]; then
    echo "healthy: Ready"
  else
    echo "unhealthy"
  fi
fi
if [ "$1" = "compose" ] && [[ "$*" == *"--no-build"* ]] \
  && [ "$FAKE_ROLLBACK_HEALTHY" = "true" ] \
  && grep -Fq "image tag vozen-rust:rollback vozen-rust:prod" "$FAKE_DOCKER_LOG"; then
  touch "$FAKE_ROLLBACK_STATE"
fi
exit 0`,
        );
        writeCommand(
          'git',
          String.raw`printf '%s\n' "$*" >> "$FAKE_GIT_LOG"
if [ "$1" = "rev-parse" ] && [ "$2" = "HEAD" ]; then
  echo "$FAKE_TARGET_SHA"
  exit 0
fi
if [ "$1" = "cat-file" ] || [ "$1" = "merge-base" ]; then
  [ "$FAKE_SOURCE_TRUSTED" = "true" ]
  exit
fi
if [ "$1" = "worktree" ] && [ "$2" = "add" ]; then
  mkdir -p "$4"
  exit 0
fi
if [ "$1" = "worktree" ] && [ "$2" = "remove" ]; then
  if [ "$FAKE_CLEANUP_FAILS" = "true" ]; then
    rmdir "$4"
    exit 1
  fi
  rmdir "$4"
  exit 0
fi
exit 64`,
        );
        writeCommand(
          'python3',
          '[ "$1" = "scripts/backup-rust-db.py" ] && [ "$FAKE_BACKUP_FAILS" = "true" ] && exit 1\ncat >/dev/null || true\nexit 0',
        );
        writeCommand('curl', '[ "$FAKE_HEALTHY" = "true" ] || [ -f "$FAKE_ROLLBACK_STATE" ]');
        writeCommand('seq', 'echo 1');
        writeCommand('sleep', 'exit 0');
        const posix = (value) => {
          const normalized = value.replaceAll('\\', '/');
          return process.platform === 'win32'
            ? normalized.replace(/^([A-Za-z]):/, (_match, drive) => `/${drive.toLowerCase()}`)
            : normalized;
        };
        const result = spawnSync(
          bash,
          [
            '-c',
            'export PATH="$1:/usr/bin:/bin"; exec bash "$2"',
            'vozen-deploy-fixture',
            posix(fakeBin),
            deployScript,
          ],
          {
            encoding: 'utf8',
            timeout: 5_000,
            env: {
              ...process.env,
              FAKE_CONTAINER_PRESENT: String(containerPresent),
              FAKE_CLEANUP_FAILS: String(cleanupFails),
              FAKE_BACKUP_FAILS: String(backupFails),
              FAKE_DOCKER_LOG: posix(dockerLog),
              FAKE_GIT_LOG: posix(gitLog),
              FAKE_HEALTHY: String(healthy),
              FAKE_IMAGE_ID: metadataPresent ? 'sha256:present' : 'sha256:missing',
              FAKE_LABEL_SHA: sourceMode === 'label' ? rollbackSha : '',
              FAKE_PREBUILT_SHA: prebuiltRevision,
              FAKE_PREVIOUS_PROD_ID: 'sha256:previous-prod',
              FAKE_PROD_TAG_STATE: posix(prodTagState),
              FAKE_ROLLBACK_BUILD_FAILS: String(rollbackBuildFails),
              FAKE_ROLLBACK_HEALTHY: String(rollbackHealthy),
              FAKE_ROLLBACK_IMAGE_SHA: rollbackImageRevision,
              FAKE_ROLLBACK_SHA: rollbackSha,
              FAKE_ROLLBACK_STATE: posix(rollbackState),
              FAKE_SOURCE_TRUSTED: String(sourceMode !== 'invalid'),
              FAKE_STATE_OPS_LOG: posix(stateOpsLog),
              FAKE_TARGET_SHA: targetSha,
              VOZEN_BACKUP_DIR: posix(join(root, 'backups')),
              VOZEN_COMPOSE_FILE: 'docker-compose.rust.prod.yml',
              VOZEN_COMPOSE_PROJECT: 'vozen-prod',
              VOZEN_COMPOSE_SERVICE: 'vozen',
              VOZEN_DATABASE: 'rust-data/tts.db',
              VOZEN_DEPLOY_DIR: posix(deployDir),
              VOZEN_DEPLOY_STATE_DIR: posix(stateDir),
              VOZEN_HEALTH_URL: 'http://127.0.0.1:3001/health',
              VOZEN_PREBUILT_IMAGE: prebuilt ? 'vozen-rust:artifact' : '',
              VOZEN_ROLLBACK_IMAGE: 'vozen-rust:rollback',
              VOZEN_ROLLBACK_SOURCE_SHA: ['override', 'invalid'].includes(sourceMode)
                ? rollbackSha
                : '',
            },
          },
        );
        const calls = existsSync(dockerLog) ? readFileSync(dockerLog, 'utf8') : '';
        const gitCalls = existsSync(gitLog) ? readFileSync(gitLog, 'utf8') : '';
        const stateOps = existsSync(stateOpsLog) ? readFileSync(stateOpsLog, 'utf8') : '';
        const deployedShaPath = join(stateDir, 'deployed-sha');
        const deployedSha = existsSync(deployedShaPath)
          ? readFileSync(deployedShaPath, 'utf8').trim()
          : '';
        const prodImage = existsSync(prodTagState) ? readFileSync(prodTagState, 'utf8').trim() : '';
        return { calls, deployedSha, gitCalls, prodImage, result, stateOps };
      } finally {
        rmSync(root, { recursive: true, force: true });
      }
    };

    const healthy = runFixture({ healthy: true });
    expect(healthy.result.status, JSON.stringify(healthy)).toBe(0);
    expect(healthy.result.stdout).toContain(
      'Rebuilt rollback image from trusted source 28f3b1f14b8a.',
    );
    expect(healthy.calls).toContain(
      `build --build-arg VOZEN_REVISION=${rollbackSha} --file Dockerfile.rust --tag vozen-rust:rollback .`,
    );
    expect(healthy.gitCalls).toContain(`worktree add --detach`);
    expect(healthy.gitCalls).toContain(rollbackSha);
    expect(healthy.calls).toContain('compose -p vozen-prod');
    expect(healthy.deployedSha).toBe(targetSha);
    expect(healthy.stateOps).toContain('install -d -m 700');
    expect(healthy.stateOps).toContain('chmod 700');
    expect(healthy.stateOps).toMatch(/chmod 600 .*deployed-sha\./);
    expect(healthy.calls).not.toContain('container commit');
    expect(healthy.calls).not.toContain('image tag vozen-rust:rollback vozen-rust:prod');

    const prebuiltHealthy = runFixture({ healthy: true, prebuilt: true });
    expect(prebuiltHealthy.result.status, JSON.stringify(prebuiltHealthy)).toBe(0);
    expect(prebuiltHealthy.result.stdout).toContain('Using CI-built production image');
    expect(prebuiltHealthy.calls).toContain('image tag vozen-rust:artifact vozen-rust:prod');
    expect(prebuiltHealthy.calls).toContain('up -d --force-recreate --no-build vozen');
    expect(prebuiltHealthy.calls).not.toContain(
      'compose -p vozen-prod -f docker-compose.rust.prod.yml build vozen',
    );

    const reusedRollback = runFixture({
      healthy: true,
      prebuilt: true,
      rollbackImageRevision: rollbackSha,
    });
    expect(reusedRollback.result.status, JSON.stringify(reusedRollback)).toBe(0);
    expect(reusedRollback.result.stdout).toContain(
      'Using rollback image verified at trusted source',
    );
    expect(reusedRollback.calls).not.toContain('build --build-arg');

    const mismatchedPrebuilt = runFixture({
      healthy: true,
      prebuilt: true,
      prebuiltRevision: rollbackSha,
    });
    expect(mismatchedPrebuilt.result.status).toBe(1);
    expect(mismatchedPrebuilt.result.stderr).toContain(
      'Refusing deploy: prebuilt image revision does not match the checked-out commit.',
    );
    expect(mismatchedPrebuilt.calls).not.toContain('up -d --force-recreate');

    const unhealthy = runFixture({ healthy: false });
    expect(unhealthy.result.status).toBe(1);
    expect(unhealthy.result.stderr).toContain('Rolling back to the previous Rust image.');
    expect(unhealthy.result.stderr).toContain('Rollback health verification: ok');
    expect(unhealthy.deployedSha).toBe('');
    expect(unhealthy.calls).toContain('image tag vozen-rust:rollback vozen-rust:prod');
    expect(unhealthy.calls).toContain('up -d --force-recreate --no-build vozen');

    const unhealthyPrebuilt = runFixture({ healthy: false, prebuilt: true });
    expect(unhealthyPrebuilt.result.status).toBe(1);
    expect(unhealthyPrebuilt.calls).toContain('image rm vozen-rust:artifact');
    expect(unhealthyPrebuilt.prodImage).toBe('vozen-rust:rollback');

    const failedBackup = runFixture({ healthy: true, prebuilt: true, backupFails: true });
    expect(failedBackup.result.status).toBe(1);
    expect(failedBackup.calls).toContain('image rm vozen-rust:artifact');
    expect(failedBackup.prodImage).toBe('sha256:previous-prod');
    expect(failedBackup.calls).not.toContain('up -d --force-recreate --no-build vozen');

    const noRollbackBuild = runFixture({ healthy: true, rollbackBuildFails: true });
    expect(noRollbackBuild.result.status).toBe(1);
    expect(noRollbackBuild.result.stderr).toContain(
      'Refusing deploy: unable to rebuild the rollback image from trusted source.',
    );
    expect(noRollbackBuild.calls).not.toContain('compose -p vozen-prod');

    const cleanupFailure = runFixture({ healthy: true, cleanupFails: true });
    expect(cleanupFailure.result.status).toBe(1);
    expect(cleanupFailure.result.stderr).toContain(
      'Refusing deploy: rollback worktree cleanup failed.',
    );
    expect(cleanupFailure.calls).not.toContain('compose -p vozen-prod');

    const invalidSource = runFixture({ healthy: true, sourceMode: 'invalid' });
    expect(invalidSource.result.status).toBe(1);
    expect(invalidSource.result.stderr).toContain(
      'Refusing deploy: unable to identify a trusted rollback source commit.',
    );
    expect(invalidSource.calls).not.toContain('build --build-arg');

    const stateSource = runFixture({ healthy: true, sourceMode: 'state' });
    expect(stateSource.result.status, JSON.stringify(stateSource)).toBe(0);
    expect(stateSource.gitCalls).not.toContain('rev-list');

    const labelledSource = runFixture({ healthy: true, sourceMode: 'label' });
    expect(labelledSource.result.status, JSON.stringify(labelledSource)).toBe(0);
    expect(labelledSource.calls).toContain('org.opencontainers.image.revision');

    const missingSource = runFixture({ healthy: true, sourceMode: 'missing' });
    expect(missingSource.result.status).toBe(1);
    expect(missingSource.result.stderr).toContain(
      'Refusing deploy: unable to identify a trusted rollback source commit.',
    );

    const symlinkedState = runFixture({ healthy: true, stateDirSymlink: true });
    expect(symlinkedState.result.status).toBe(1);
    expect(symlinkedState.result.stderr).toContain(
      'Refusing deploy: deployment state directory must not be a symlink.',
    );
    expect(symlinkedState.calls).toBe('');
    expect(symlinkedState.gitCalls).toBe('');

    const normalRollback = runFixture({ healthy: false, metadataPresent: true });
    expect(normalRollback.result.status).toBe(1);
    expect(normalRollback.calls).not.toContain('build --build-arg');
    expect(normalRollback.calls).toContain('image tag sha256:present vozen-rust:rollback');
    expect(normalRollback.calls).toContain('image tag vozen-rust:rollback vozen-rust:prod');
    expect(normalRollback.calls).toContain('up -d --force-recreate --no-build vozen');

    const failedRollback = runFixture({ healthy: false, rollbackHealthy: false });
    expect(failedRollback.result.status).toBe(1);
    expect(failedRollback.result.stderr).toContain('Rollback container did not become healthy.');
    expect(failedRollback.result.stderr).not.toContain('Rollback health verification: ok');
    expect(failedRollback.calls).toContain('up -d --force-recreate --no-build vozen');
    expect(failedRollback.deployedSha).toBe('');

    const failedWithState = runFixture({ healthy: false, sourceMode: 'state' });
    expect(failedWithState.result.status).toBe(1);
    expect(failedWithState.deployedSha).toBe(rollbackSha);

    const firstDeploy = runFixture({ healthy: true, containerPresent: false });
    expect(firstDeploy.result.status).toBe(0);
    expect(firstDeploy.calls).toContain('build vozen');
    expect(firstDeploy.calls).toContain('up -d --force-recreate vozen');
    expect(firstDeploy.calls).not.toContain('image tag');
    expect(firstDeploy.calls).not.toContain('container commit');
    expect(firstDeploy.calls).not.toContain('--no-build');
    const deployScriptSource = source('scripts/deploy-rust-vps.sh');
    const composeSource = source('docker-compose.rust.prod.yml');
    const dockerfileSource = source('Dockerfile.rust');
    expect(deployScriptSource).not.toContain('docker container commit');
    expect(deployScriptSource).toContain('export VOZEN_BUILD_SHA="$(git rev-parse HEAD)"');
    expect(composeSource).toContain('VOZEN_REVISION: ${VOZEN_BUILD_SHA:-unknown}');
    expect(composeSource).toMatch(/tmpfs:\s*\n\s+- \/tmp:mode=1777/);
    expect(dockerfileSource).toContain('FROM rust:1.97.1-bookworm AS builder');
    expect(dockerfileSource).not.toMatch(/^COPY .*rust-toolchain/m);
    expect(dockerfileSource).toContain('org.opencontainers.image.revision');
    const ciWorkflow = source('.github/workflows/ci.yml');
    expect(ciWorkflow).toContain('--build-arg "VOZEN_REVISION=$GITHUB_SHA"');
    expect(ciWorkflow).toContain('docker image save "vozen-rust:$GITHUB_SHA"');
    expect(ciWorkflow).toContain('name: vozen-rust-image-${{ github.sha }}');
    expect(ciWorkflow).toContain(
      'actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a',
    );
    const deployWorkflow = source('.github/workflows/deploy-bot.yml');
    const runDeploy = deployWorkflow.slice(
      deployWorkflow.indexOf('run_rust_deploy()'),
      deployWorkflow.indexOf('if ! git diff --quiet'),
    );
    expect(runDeploy).toMatch(
      /if \[ ! -f "\$deploy_state" \][\s\S]*&& \[\[ ! "\$running_revision" =~ \^\[0-9a-f\]\{40\}\$ \]\]; then[\s\S]*rollback_source_sha="28f3b1f6df4d1f84d22e0afa2718c657dffcae80"/,
    );
    expect(runDeploy).toContain('VOZEN_DEPLOY_STATE_DIR="$deploy_state_dir"');
    expect(runDeploy).toContain('VOZEN_ROLLBACK_SOURCE_SHA="$rollback_source_sha"');
    expect(runDeploy).toContain('bash scripts/deploy-rust-vps.sh');
    const sameShaRetry = deployWorkflow.slice(
      deployWorkflow.indexOf('if [ "$current_commit" = "$target_commit" ]'),
      deployWorkflow.indexOf('if git merge-base --is-ancestor "$target_commit"'),
    );
    expect(sameShaRetry).toContain('run_rust_deploy');
    expect(sameShaRetry).not.toContain('docker compose');
    expect(deployWorkflow).toContain('28f3b1f6df4d1f84d22e0afa2718c657dffcae80');
  }, 30_000);
  it('keeps payment credentials in the VPS runtime file instead of the SSH command line', () => {
    const deploy = source('.github/workflows/deploy-bot.yml');
    expect(deploy).toContain('require_runtime_secret STRIPE_SECRET_KEY');
    expect(deploy).toContain('require_runtime_secret STRIPE_PUBLISHABLE_KEY');
    expect(deploy).toContain('require_runtime_secret STRIPE_WEBHOOK_SECRET');
    expect(deploy).toContain('the remote command line is observable to local processes');
    expect(deploy).not.toContain('STRIPE_SECRET_KEY: ${{ secrets.STRIPE_SECRET_KEY }}');
    expect(deploy).not.toContain('STRIPE_PUBLISHABLE_KEY: ${{ secrets.STRIPE_PUBLISHABLE_KEY }}');
    expect(deploy).not.toContain('STRIPE_WEBHOOK_SECRET: ${{ secrets.STRIPE_WEBHOOK_SECRET }}');
    expect(deploy).not.toMatch(/^\s*envs:\s*.*STRIPE_/m);
  });
  it('binds the owner console to the Helper OAuth application in production', () => {
    const productionEnv = source('.env.rust.prod.example');
    expect(productionEnv).toContain('ADMIN_CLIENT_ID=1526211106081734666');
    expect(productionEnv).toContain(
      'ADMIN_PANEL_ORIGIN=https://rexy40407.github.io',
    );
  });
  it('keeps the Night Signal treatment scoped to Discord entry points', () => {
    const css = source(SITE_CSS);
    const index = source('site/index.html');
    const account = source('site/account.html');
    const dashboard = source('site/dashboard.html');
    for (const pagePath of [
      'site/index.html',
      'site/account.html',
      'site/dashboard.html',
      'site/privacy.html',
      'site/terms.html',
    ]) {
      const page = source(pagePath);
      expect(page, pagePath).toContain('css/main-v43.css');
      expect(page, pagePath).not.toContain('css/main-v41.css');
    }
    expect(existsSync(resolve(process.cwd(), 'site/css/main-v41.css'))).toBe(false);
    for (const [pagePath, page] of [
      ['site/index.html', index],
      ['site/account.html', account],
      ['site/dashboard.html', dashboard],
    ]) {
      const navLoginClasses = page.match(
        /<button class="([^"]+)" id="navLogin" type="button">/,
      )?.[1];
      expect(navLoginClasses, pagePath).toContain('btn--discord-cta');
    }
    const inviteClasses = [...index.matchAll(/<a class="([^"]*\bjs-invite\b[^"]*)"/g)].map(
      (match) => match[1],
    );
    expect(inviteClasses).toHaveLength(3);
    for (const classes of inviteClasses) expect(classes).toContain('btn--discord-cta');
    expect(css).toMatch(
      /\.btn--discord-cta\s*\{[^}]*color:\s*#fff;[^}]*linear-gradient\(115deg,\s*#4f46e5 0%,\s*#365bc9 52%,\s*#0f766e 100%\)/s,
    );
    expect(css).toContain('.btn--discord-cta:hover');
    expect(css).toContain('.btn--discord-cta:active');
    expect(css).toContain('.btn--discord-cta:focus-visible');
    expect(css).toMatch(/@media\s*\(prefers-reduced-motion:\s*reduce\)[\s\S]*\.btn--discord-cta/);
    const dashboardScript = source('site/js/dashboard-v8.js');
    expect(dashboardScript).toContain('var BTN = "btn btn--primary";');
    expect(dashboardScript).not.toContain('var BTN = "btn btn--primary btn--discord-cta";');
  });
  it('keeps the account redesign isolated, responsive, and wired to the versioned runtime', () => {
    const page = source('site/account.html');
    const css = source(ACCOUNT_CSS);
    expect(page).toContain('css/account-v6.css');
    expect(page).not.toContain('css/account-v5.css');
    expect(css).toMatch(/body\.page-account \.ppanel__av\s*\{\s*border-radius:\s*50%;\s*\}\s*$/);
    expect(page).toContain('<header class="nav" id="nav">');
    expect(page).toContain('class="account-workspace"');
    expect(page).toContain('class="account-membership"');
    expect(page).toContain('class="account-tasklist"');
    expect(page).toContain('id="accountBilling"');
    expect(page).not.toContain('id="accountActivateOpen"');
    expect(page).toContain('js/main-v51.js');
    expect(page).toContain('css/billing-v3.css?v=compact-checkout-v1');
    expect(page).toContain('https://js.stripe.com/dahlia/stripe.js');
    expect(page).toContain('frame-src https://checkout.stripe.com https://js.stripe.com');
    expect(css).toContain('body.page-account');
    expect(css).toMatch(/@media\s*\(max-width:\s*760px\)/);
    expect(css).toMatch(/@media\s*\(min-width:\s*1280px\)\s*and\s*\(min-height:\s*800px\)/);
    expect(css).toContain('clamp(430px, 33.333%, 520px)');
    expect(css).not.toMatch(/body\.page-account\s+\.nav\s*\{[^}]*display:\s*none;?[^}]*\}/s);
  });
  it('keeps the account journey focused and the session exit visible', () => {
    const script = source(SITE_JS);
    const claim = claimCardSource();
    expect(claim).toContain('return "";');
    expect(claim).not.toContain('Stripe subscriptions are managed from the account panel.');
    expect(script).toContain('function billingCheckoutModal(plan, interval)');
    expect(script).toContain('function ensureStripeJs()');
    expect(script).toContain('createEmbeddedCheckoutPage');
    expect(script).not.toContain('stripe.initCheckout');
    expect(script).not.toContain('createPaymentElement');
    expect(script).toContain('payload.clientSecret');
    expect(script).toContain('window.location.assign(payload.url)');
    expect(script).toContain('function continueHostedBillingCheckout()');
    const billingErrorSource = script.slice(
      script.indexOf('function showBillingError('),
      script.indexOf('function closeBillingCheckout'),
    );
    expect(billingErrorSource).not.toContain('claim.loginAgain');
    expect(script).not.toContain('vozenDiscordAuth');
    expect(script).not.toContain('window.open("about:blank"');
    expect(script).not.toContain('vozenDiscordLogin');
    expect(script).not.toContain('vozen:discord-auth');
    expect(script).toContain('login({ billing: true });');
    expect(script).toContain('BILLING_INTENT_KEY');
    expect(script).toContain('const BILLING_OAUTH_REDIRECT = new URL("/", location.href).href');
    expect(script).toContain(
      'options && options.billing === true ? BILLING_OAUTH_REDIRECT : OAUTH_REDIRECT',
    );
    expect(script).toContain('location.replace("/#premium")');
    expect(script).toContain('history.replaceState(null, "", "#premium")');
    const checkoutSource = script.slice(
      script.indexOf('async function startCheckout('),
      script.indexOf('const billingInterval'),
    );
    expect(checkoutSource).not.toContain('window.location.href');
    expect(claim).toContain('id="activate-purchase"');
    expect(claim).toContain('role="dialog"');
    expect(claim).toContain('id="ppClaimClose"');
    expect(claim).toContain('<details class="ppanel__receipt"');
    expect(claim).toContain('class="ppanel__activatebtn"');
    expect(script).toContain('class="ppanel__logout-icon"');
    expect(script).toContain('function openPurchaseActivation()');
    expect(script).toContain('function mountPurchaseActivation(el)');
  });
  it('loads Stripe.js for the on-site embedded Checkout and cache-busts the runtime', () => {
    const page = source('site/index.html');
    expect(page).toContain('css/billing-v3.css?v=compact-checkout-v1');
    expect(page).toContain('js/i18n-v41.js?v=payment-element');
    expect(page).toContain(
      '<script defer data-vozen-stripe src="https://js.stripe.com/dahlia/stripe.js"></script>',
    );
    expect(page).toContain('js/main-v51.js?v=embedded-checkout-v1');
    expect(source(SITE_JS)).toContain('BILLING_COPY_FALLBACKS');
  });
  it('keeps the embedded checkout error inside the dark blurred modal', () => {
    const css = source(BILLING_CSS);
    const script = source(SITE_JS);
    expect(css).toContain('backdrop-filter: blur(12px) saturate(0.72)');
    expect(css).toContain('width: min(100%, 600px)');
    expect(css).toContain('.billing-elements');
    expect(css).toContain('min-height: 420px');
    expect(css).toContain('background: #0c1220');
    expect(css).not.toContain('background: #fff');
    expect(script).toContain('billing-modal__error-icon');
    expect(script).toContain('window.scrollY');
    expect(script).toContain('window.scrollTo(0, billingScrollY)');
    expect(script).toContain('billingBodyStyle');
    expect(script).toContain('pointerdown');
    expect(script).toContain('preserveScrollBeforeFocus');
    expect(script).toContain('billingPendingScrollY');
    expect(script).toContain('document.documentElement.insertAdjacentHTML');
  });
  it('cache-busts the checkout layout whenever the billing stylesheet changes', () => {
    for (const pagePath of ['site/index.html', 'site/account.html']) {
      const page = source(pagePath);
      expect(page, pagePath).toContain('css/billing-v3.css');
      expect(page, pagePath).not.toContain('css/billing-v1.css');
      expect(page, pagePath).not.toContain('css/billing-v2.css');
    }
  });
  it('keeps the Cloudflare CSP aligned with the self-hosted-font privacy promise', () => {
    const script = source('tools/cf-security-headers.mjs');
    expect(script).not.toContain('fonts.googleapis.com');
    expect(script).not.toContain('fonts.gstatic.com');
    expect(script).toContain("style-src 'self' 'unsafe-inline'");
    expect(script).toContain("font-src 'self'");
    expect(script).toContain("script-src 'self' https://js.stripe.com https://*.js.stripe.com");
    expect(script).toContain('frame-src https://checkout.stripe.com https://js.stripe.com');
    expect(script).toContain('https://api.stripe.com');
    expect(script).toContain('https://link.com');
    expect(script).toContain('payment=(self');
    expect(source('.github/workflows/security-headers.yml')).toContain('CF_API_TOKEN');
  });
  it('verifies both downloaded Kokoro model assets against pinned SHA-256 hashes', () => {
    const script = source('tools/setup-kokoro.ps1');
    expect(script).toContain('7D5DF8ECF7D4B1878015A32686053FD0EEBE2BC377234608764CC0EF3636A6C5');
    expect(script).toContain('BCA610B8308E8D99F32E6FE4197E7EC01679264EFED0CAC9140FE9C29F1FBF7D');
    expect(script).toContain('Get-FileHash');
  });
  it('does not ship byte-identical font files under duplicate names', () => {
    const fontDir = resolve(process.cwd(), 'site/assets/fonts');
    const byHash = new Map();
    for (const name of readdirSync(fontDir)) {
      const bytes = readFileSync(resolve(fontDir, name));
      const hash = createHash('sha256').update(bytes).digest('hex');
      expect(byHash.get(hash), `${name} duplicates ${byHash.get(hash)}`).toBeUndefined();
      byHash.set(hash, name);
    }
  });
  it('keeps every font URL in the site stylesheet resolvable', () => {
    const css = source(SITE_CSS);
    const urls = [...css.matchAll(/url\("\.\.\/assets\/fonts\/([^"?]+)"\)/g)].map(
      (match) => match[1],
    );
    expect(urls.length).toBeGreaterThan(0);
    for (const name of urls) {
      expect(existsSync(resolve(process.cwd(), 'site/assets/fonts', name)), name).toBe(true);
    }
  });
  it('localizes account accessibility labels from the canonical dictionary', () => {
    const script = source(SITE_JS);
    expect(script).toContain('t("account.copyDiscordId")');
    expect(script).toContain('t("account.closeActivation")');
    expect(script).not.toContain('aria-label="Copy Discord ID"');
  });
  // Delivery happens when the buyer activates here, so the checkbox itself must explicitly name
  // immediate performance and the applicable loss of the withdrawal right before either path.
  it('gates pass activation behind an express consent checkbox', () => {
    const script = source(SITE_JS);
    expect(script).toContain('id="ppClaimConsent"');
    expect(script).toContain('claim.consent');
    // The guard must refuse when unticked — failing open would activate the pass with no
    // acknowledgement at all.
    expect(script).toMatch(/if \(!consent \|\| !consent\.checked\)/);
    expect(script).toContain('claim.consentRequired');
  });
  it('renders explicit immediate-delivery consent with a real terms link', () => {
    const card = fnSource(source(SITE_JS), 'function claimCard()');
    expect(card).toContain('claim.consent');
    expect(card).toContain('claim.consentTerms');
    expect(card).toMatch(/<a href="\/terms"/);
    // The <a> is injected in place of the {terms} placeholder inside the trusted consent copy.
    expect(card).toContain('.replace("{terms}"');
    const english = i18nBundle().en['claim.consent'];
    expect(english).toMatch(/immediate activation/i);
    expect(english).toMatch(/withdrawal right/i);
  });
  it('implements instant activation with strict success parsing and one-shot OAuth resume', () => {
    const script = source(SITE_JS);
    const card = claimCardSource();
    const activation = fnSource(script, 'async function doInstantActivation(');
    expect(card).toContain('id="ppActivateBtn"');
    expect(card).toContain('claim.giftNote');
    expect(card.indexOf('ppActivateBtn')).toBeLessThan(card.indexOf('ppClaimCode'));
    expect(script).toContain('const ACTIVATION_TERMS_VERSION = "2026-07-19"');
    expect(script).toContain('u.searchParams.set("scope", "identify email")');
    expect(activation).toContain('PREMIUM_API_BASE + "/api/activate"');
    expect(activation).toContain('termsAccepted: true');
    expect(activation).toMatch(/res\.status === 200 && body\.ok === true/);
    expect(script).toContain('const ACTIVATION_INTENT_TTL_MS = 5 * 60 * 1000');
    expect(script).toContain('sessionStorage.removeItem(ACTIVATION_INTENT_KEY)');
    expect(script).toContain('allowRelogin: false');
    expect(script).toContain('downloadActivationConfirmation');
    expect(script).toContain('acceptedAtIso');
  });
  // The claim field takes the whole receipt URL now (extractReceiptCode, src/premium/claim.ts),
  // so the copy must stop teaching people to perform surgery on an address bar — "the code
  // after txid=" was never a reasonable thing to ask, and on the monthly receipt it actively
  // misled: the code sits mid-URL, so selecting to the end drags &mode=g along.
  //
  // Asserted as an absence, deliberately. Checking that ten languages each "say to paste the
  // link" is not something a string match can honestly do — but the surgical instruction is
  // one literal token, and its absence is checkable in every language.
  it('no longer asks buyers to extract the code from the URL', () => {
    const bundle = source(SITE_I18N);
    const sandbox = {
      window: {},
    };
    new Function('window', bundle)(sandbox.window);
    const all = sandbox.window.VOZEN_I18N ?? {};
    const langs = Object.keys(all);
    expect(langs.length).toBeGreaterThan(0);
    for (const lang of langs) {
      for (const key of [
        'claim.hint',
        'claim.placeholder',
        'claim.useReceiptCode',
        'claim.notfound',
      ]) {
        expect(all[lang][key], `${lang} ${key} exists`).toBeTruthy();
        expect(all[lang][key], `${lang} ${key} still says txid=`).not.toContain('txid=');
      }
    }
  });
  // Closing the receipt tab is not a dead end — Ko-fi emails the buyer a receipt — but the card
  // never said so, which made it one in practice. The line has to name the email first (the copy
  // every buyer has), and hand the genuinely-stuck tail to the help modal (plan 036) rather than
  // dumping them straight on support.
  it('offers a way back when the buyer no longer has the receipt', () => {
    const card = claimCardSource();
    expect(card).toContain('claim.lost');
    expect(card).toContain('claim.lostHelp');
    // The modal is the escape hatch now; support lives inside it, one step further in.
    expect(card).toContain('ppClaimHelpOpen');
  });
  // The `.js-support` wiring at the top of the file runs ONCE over the document at load, and both
  // the claim card and the help modal are injected later, after OAuth. An anchor leaning on that
  // wiring would render with no href at all — so the markup must carry the URL itself. A silent
  // hrefless link is exactly the failure the recovery path exists to prevent.
  it('renders the support link with a real href, not the one-time wiring', () => {
    const modal = helpModalSource();
    expect(modal).toContain('${SUPPORT_URL}');
    expect(modal, 'must not rely on the one-time .js-support wiring').not.toContain('js-support');
  });
  // The Ko-fi email receipt shows `Ref: S-M1X823C9FW` — the only code-looking string in the whole
  // email, and one we can NEVER accept: the webhook payload has no such field, so no pending row
  // carries it. Someone hunting for a code finds that, pastes it, and gets a flat "no purchase
  // found" for a purchase they really made. Catch the shape before the request leaves the browser
  // and send them somewhere useful instead.
  it('catches the Ko-fi order Ref before it reaches the server', () => {
    const script = source(SITE_JS);
    expect(script).toMatch(/REF_RE\s*=/);
    const claim = fnSource(script, 'async function doClaim(');
    expect(claim).toContain('REF_RE');
    expect(claim).toContain('claim.help.refPasted');
    // Must be decided BEFORE the fetch: reaching the server means a 404 the buyer cannot act on.
    const refAt = claim.indexOf('REF_RE');
    const fetchAt = claim.indexOf('fetch(');
    expect(refAt, 'Ref check must precede the fetch').toBeLessThan(fetchAt);
  });
  // Every dismissal a modal can offer, because someone who cannot close it is trapped on the one
  // page they went to for help. Esc is the one most often forgotten.
  it('makes the help modal dismissable and announced', () => {
    const modal = helpModalSource();
    expect(modal).toContain('role="dialog"');
    expect(modal).toContain('aria-modal="true"');
    expect(modal).toContain('aria-labelledby');
    const script = source(SITE_JS);
    expect(script).toContain('ppClaimHelpClose'); // the X
    expect(script).toMatch(/Escape/); // keyboard
    expect(script).toContain('ppClaimHelpBackdrop'); // click-outside
  });
  // The help request carries the EMAIL, not the Ref: Ko-fi's transaction search matches by email,
  // so the Ref is useless to the owner (verified against the live seller panel, 2026-07-17). The
  // email is a lookup hint, not proof — the owner still confirms the paid order and grants by hand.
  it('collects the Ko-fi email in the help modal and posts it, not the Ref', () => {
    const modal = helpModalSource();
    expect(modal).toContain('id="ppClaimHelpEmail"');
    expect(modal).toContain('type="email"');
    expect(modal).toContain('claim.help.emailPlaceholder');
    const script = source(SITE_JS);
    // The POST body must send { email: ... } — a lingering { ref } would reach the endpoint's
    // bad_email guard and the buyer would get nothing.
    expect(script).toMatch(/body:\s*JSON\.stringify\(\{\s*email:/);
  });
  it('translates the recovery and help copy into every advertised site language', () => {
    const all = i18nBundle();
    const langs = Object.keys(all);
    expect(langs.length).toBeGreaterThan(0);
    for (const lang of langs) {
      // Split into separate keys on purpose: no translation has to carry markup through esc().
      for (const key of [
        'claim.lost',
        'claim.lostHelp',
        'claim.help.title',
        'claim.help.step1',
        'claim.help.step2',
        'claim.help.emailPlaceholder',
        'claim.help.send',
        'claim.help.refPasted',
        'claim.help.notEmail',
        'claim.help.sent',
        'claim.help.stillStuck',
      ]) {
        expect(all[lang][key], `${lang} ${key}`).toBeTruthy();
      }
    }
  });
  it('translates the consent copy into every advertised site language', () => {
    const bundle = source(SITE_I18N);
    const sandbox = {
      window: {},
    };
    new Function('window', bundle)(sandbox.window);
    const all = sandbox.window.VOZEN_I18N ?? {};
    const langs = Object.keys(all);
    expect(langs.length).toBeGreaterThan(0);
    for (const lang of langs) {
      // Untranslated consent text is not a cosmetic gap: someone who cannot read what they
      // are accepting has not knowingly accepted it.
      expect(all[lang]['claim.consent'], `${lang} claim.consent`).toBeTruthy();
      expect(all[lang]['claim.consentRequired'], `${lang} claim.consentRequired`).toBeTruthy();
      expect(all[lang]['claim.consentTerms'], `${lang} claim.consentTerms`).toBeTruthy();
      // Every consent string must keep the {terms} placeholder — that is where the clickable
      // terms link is injected. A translation that drops it would render a linkless sentence.
      expect(all[lang]['claim.consent'], `${lang} keeps the {terms} slot`).toContain('{terms}');
      for (const key of [
        'claim.instantBtn',
        'claim.instantWorking',
        'claim.giftNote',
        'claim.orReceipt',
        'claim.activationOk',
        'claim.downloadConfirmation',
        'claim.emailMissing',
        'claim.emailUnverified',
        'claim.serviceUnavailable',
        'claim.loginAgain',
        'claim.resumeExpired',
      ]) {
        expect(all[lang][key], `${lang} ${key}`).toBeTruthy();
      }
    }
  });
  // The terms link inside the consent line must look clickable — a distinct colour, not the dim
  // body colour it would inherit from .ppanel__claimconsent. Without it the only affordance is the
  // cursor, which nobody sees; the buyer never realises the terms are a link.
  it('styles the consent terms link as clickable', () => {
    const css = source(SITE_CSS);
    expect(css).toMatch(/\.ppanel__claimconsent a\s*\{[^}]*var\(--aqua\)/);
  });
  it('documents the Stripe digital checkout contract', () => {
    const terms = source('site/terms.html');
    expect(terms).toMatch(/14-day withdrawal right/i);
    expect(terms).toMatch(/digital subscriptions processed by Stripe/i);
    expect(terms).toMatch(/does not collect a shipping address/i);
    expect(terms).not.toMatch(/Ko-fi purchase/i);
  });
  it('keeps Stripe-hosted checkout as a safe fallback and configures embedded Checkout', () => {
    const stripe = source('crates/vozen-api/src/stripe_api.rs');
    const hosted = stripe.slice(
      stripe.indexOf('async fn checkout('),
      stripe.indexOf('async fn checkout_elements('),
    );
    const elements = stripe.slice(
      stripe.indexOf('async fn checkout_elements('),
      stripe.indexOf('async fn portal('),
    );
    expect(hosted).toContain('("ui_mode", "hosted_page".to_owned())');
    expect(hosted).toContain('("success_url", success_url)');
    expect(hosted).toContain('("cancel_url", cancel_url)');
    expect(hosted).toContain('v.get("url")');
    expect(elements).toContain('("ui_mode", "embedded_page".to_owned())');
    expect(elements).toContain('("redirect_on_completion", "never".to_owned())');
    expect(elements).toContain('"clientSecret"');
    expect(elements).toContain('"publishableKey"');
    expect(elements).toContain('json_no_store_response');
    expect(elements).toContain('checkout_identity_mismatch');
    expect(stripe).toContain('.route("/api/billing/checkout/status", any(checkout_status))');
    expect(stripe).toContain('fn valid_stripe_session_id');
    expect(stripe).toContain('.header("Stripe-Version", STRIPE_API_VERSION)');
    expect(elements).not.toContain('"return_url"');
  });
});
