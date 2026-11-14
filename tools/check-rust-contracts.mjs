import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const root = resolve(import.meta.dirname, '..');

function readJson(relativePath) {
  const path = resolve(root, relativePath);
  try {
    return JSON.parse(readFileSync(path, 'utf8'));
  } catch (error) {
    throw new Error(`invalid contract ${relativePath}: ${error.message}`);
  }
}

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

function assertUnique(values, label) {
  const unique = new Set(values);
  assert(unique.size === values.length, `${label} contains duplicates`);
}

function commandNames(commands, path = []) {
  return commands.flatMap((command) => {
    const commandPath = [...path, command.name];
    return [commandPath.join(' '), ...commandNames(command.options ?? [], commandPath)];
  });
}

const commands = readJson('contracts/discord-commands.json');
assert(commands.schema_version === 1, 'Discord command contract schema must be version 1');
assert(
  commands.public_commands?.length === 40,
  'Discord command contract must contain 40 public commands',
);
assert(
  commands.owner_commands?.length === 2,
  'Discord command contract must contain 2 owner commands',
);
assertUnique(
  commandNames([...commands.public_commands, ...commands.owner_commands]),
  'Discord command paths',
);

const schema = readJson('contracts/sqlite-schema.json');
assert(schema.schema_version === 1, 'SQLite contract schema must be version 1');
assert(schema.objects?.length > 0, 'SQLite contract must contain objects');
assertUnique(
  schema.objects.map((object) => object.name),
  'SQLite schema objects',
);
assert(
  schema.objects.every(
    (object) => ['table', 'index'].includes(object.type) && /^CREATE /.test(object.sql),
  ),
  'SQLite contract contains an invalid object',
);

const voiceI18n = readJson('contracts/voice-response-i18n.json');
assert(voiceI18n.schema_version === 1, 'voice response contract schema must be version 1');
assert(voiceI18n.default_locale === 'en', 'voice response contract must default to English');
assertUnique(voiceI18n.supported_locales, 'voice response locales');
assertUnique(voiceI18n.keys, 'voice response keys');
const voicePreferenceSink = readFileSync(
  resolve(root, 'crates/vozen-runtime/src/voice_preference_sink.rs'),
  'utf8',
);
for (const [, key] of voicePreferenceSink.matchAll(/"(voice\.[a-zA-Z]+(?:\.[a-zA-Z]+)*)"/g)) {
  assert(voiceI18n.keys.includes(key), `voice response contract is missing runtime key ${key}`);
}
for (const locale of voiceI18n.supported_locales) {
  const messages = voiceI18n.messages?.[locale];
  assert(messages, `voice response contract is missing locale ${locale}`);
  assert(
    JSON.stringify(Object.keys(messages).sort()) === JSON.stringify([...voiceI18n.keys].sort()),
    `voice response locale ${locale} has a different key set`,
  );
  assert(
    Object.values(messages).every((message) => typeof message === 'string' && message.trim()),
    `voice response locale ${locale} contains an empty message`,
  );
}

const voiceDisplay = readJson('contracts/voice-display-i18n.json');
assert(voiceDisplay.schema_version === 1, 'voice display contract schema must be version 1');
assertUnique(voiceDisplay.supported_locales, 'voice display locales');
assert(
  voiceDisplay.supported_locales.every((locale) => voiceDisplay.names?.[locale]),
  'voice display contract is missing a locale name map',
);

const gameContent = readJson('crates/vozen-discord/assets/game-content.json');
assert(gameContent.schema_version === 1, 'game content contract schema must be version 1');
const gameSections = Object.entries(gameContent).filter(
  ([key]) => key !== 'schema_version' && key !== 'generated_from',
);
assert(gameSections.length > 0, 'game content contract is empty');
assert(
  gameSections.every(([, value]) => value && typeof value === 'object'),
  'game content contract contains an invalid section',
);

console.log(
  `[check-rust-contracts] commands=${commands.public_commands.length}+${commands.owner_commands.length} ` +
    `schemaObjects=${schema.objects.length} locales=${voiceI18n.supported_locales.length} ` +
    `gameContent=${gameSections.length}`,
);
