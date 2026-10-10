// SPDX-License-Identifier: Apache-2.0
// Static source-context proof only. This does not execute Docker or build an image.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile, stat } from 'node:fs/promises';
const here = path => new URL(path, import.meta.url);
test('hosted image source allowlist includes API compile-time protocol fixtures and excludes secrets', async () => {
  const rules = (await readFile(here('./Dockerfile.hosted.dockerignore'), 'utf8')).split('\n');
  for (const rule of ['!api/tests/', '!api/tests/fixtures/', '!api/tests/fixtures/**']) assert.ok(rules.includes(rule));
  const source = await readFile(here('../../../../../api/src/lib.rs'), 'utf8');
  const fixtures = [...source.matchAll(/include_str!\("\.\.\/tests\/fixtures\/([^"/]+)"\)/g)].map(match => match[1]);
  assert.ok(fixtures.length >= 6);
  for (const fixture of fixtures) assert.ok((await stat(here(`../../../../../api/tests/fixtures/${fixture}`))).isFile());
  for (const rule of ['**/.env*', '**/*.pem', '**/*.key', '**/identity.toml', '**/.heddle', '**/target'])
    assert.ok(rules.includes(rule));
  const dockerfile = await readFile(here('./Dockerfile.hosted'), 'utf8');
  assert.match(dockerfile, /COPY api\/ \/build\/api\//);
  assert.doesNotMatch(dockerfile, /gateway-fixture/);
});
