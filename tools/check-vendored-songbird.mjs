import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync} from 'node:fs';

const root = new URL('../vendor/', import.meta.url);
const provenance = JSON.parse(readFileSync(new URL('songbird-provenance.json', root)));
assert.equal(provenance.crate, 'songbird');
assert.equal(provenance.version, '0.6.0');
for (const [name, expected] of Object.entries(provenance.originalFiles)) {
  assert(!name.includes('..') && !name.startsWith('/'));
  let content = Buffer.from(readFileSync(new URL('songbird/' + name, root), 'utf8').replace(/\r\n/g, '\n'));
  if (name === 'Cargo.toml') {
    const text = content.toString();
    const replacement = '[dependencies.ringbuf]\nversion = "0.5.3"';
    assert.equal(text.split(replacement).length, 2);
    content = Buffer.from(text.replace(replacement, '[dependencies.ringbuf]\nversion = "0.4"'));
  }
  assert.equal(createHash('sha256').update(content).digest('hex'), expected, name + ' drifted from upstream');
}
assert.match(readFileSync(new URL('../Cargo.lock', import.meta.url), 'utf8'), /name = "ringbuf"\nversion = "0\.5\.3"/);
console.log('Vendored Songbird source matches upstream; only ringbuf dependency differs.');
