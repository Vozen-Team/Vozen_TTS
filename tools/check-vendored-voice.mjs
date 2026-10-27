import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';

const root = new URL('../vendor/', import.meta.url);
for (const [crate, version] of [
  ['songbird', '0.6.0'],
  ['davey', '0.1.4'],
]) {
  const provenance = JSON.parse(readFileSync(new URL(crate + '-provenance.json', root)));
  assert.equal(provenance.crate, crate);
  assert.equal(provenance.version, version);
  const changes =
    crate === 'songbird'
      ? [['[dependencies.ringbuf]\nversion = "0.4"', '[dependencies.ringbuf]\nversion = "0.5.3"']]
      : provenance.manifestChanges;
  for (const [name, expected] of Object.entries(provenance.originalFiles)) {
    assert(!name.includes('..') && !name.startsWith('/'));
    let content = Buffer.from(
      readFileSync(new URL(crate + '/' + name, root), 'utf8').replace(/\r\n/g, '\n'),
    );
    if (name === 'Cargo.toml') {
      let text = content.toString();
      for (const [original, replacement] of changes) {
        assert.equal(text.split(replacement).length, 2);
        text = text.replace(replacement, original);
      }
      content = Buffer.from(text);
    }
    assert.equal(
      createHash('sha256').update(content).digest('hex'),
      expected,
      name + ' drifted from upstream',
    );
  }
}
assert.match(
  readFileSync(new URL('../Cargo.lock', import.meta.url), 'utf8'),
  /name = "ringbuf"\nversion = "0\.5\.3"/,
);
console.log(
  'Vendored voice library implementations match upstream; only documented dependencies differ.',
);
