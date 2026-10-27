import { readFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import assert from 'node:assert/strict';
const text = readFileSync(new URL('./deploy-bot.yml', import.meta.url), 'utf8').replace(/\r\n/g, '\n');
assert(text.includes('id-token: write'));
assert(text.includes("vozen@146.59.147.110 'deploy'"));
assert(text.includes('StrictHostKeyChecking=yes'));
assert(text.includes('.head_repository.id == $id'));
assert(text.includes('.head_sha == $sha'));
assert(!text.includes('VPS_ADMIN'));
assert(!text.includes('sudo docker'));
assert(!text.includes('prune'));
assert(!text.includes('bash scripts/'));
assert(!text.includes('debug: true'));
const lines = text.split('\n');
let checked = 0;
for (let i = 0; i < lines.length; i++) {
  const match = lines[i].match(/^(\s+)run: \|$/);
  if (!match) continue;
  const indent = match[1].length + 2;
  const body = [];
  for (let j = i + 1; j < lines.length; j++) {
    if (!lines[j].trim()) {
      body.push('');
      continue;
    }
    if (!lines[j].startsWith(' '.repeat(indent))) break;
    body.push(lines[j].slice(indent));
  }
  const result = spawnSync(
    process.platform === 'win32' ? 'C:/Program Files/Git/bin/bash.exe' : 'bash',
    ['-n'],
    { input: body.join('\n'), encoding: 'utf8' },
  );
  assert.equal(result.status, 0, result.stderr);
  checked++;
}
assert.equal(checked, 6);
console.log(`Broker workflow safety assertions and ${checked} Bash blocks passed.`);
