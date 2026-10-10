import { FRAME_MIME, encodeFrame, decodeFrame } from '../native-frame.mjs';
import test from 'node:test';
import assert from 'node:assert/strict';
import entry, { HOSTED_ACTIVATION_REVIEWED, HostedPublication, preparedHostedFetch, preparedHostedObjectFetch } from './hosted-index.mjs';
const config = JSON.stringify({ toy: { catalog: 'toy-catalog', bootstrap_native: `hs-${'a'.repeat(52)}` } });
const request = (write = false, extra = {}) => new Request(`https://edge.invalid/repositories/toy.git/info/refs?service=git-${write ? 'receive' : 'upload'}-pack`, { headers: { authorization: 'Bearer test-only-session', ...extra } });
function env(authorized = true) {
  const seen = { native: [], object: [] };
  return { seen, HOSTED_REPOSITORIES: config, ARTIFACTS: {}, NATIVE_SERVICE_AUTHORIZATION: 'Bearer ' + 'S'.repeat(64),
    NATIVE_CONTAINER: { getByName: name => ({ async fetch(req) { assert.equal(name, 'hosted-native-runtime'); seen.native.push({ url: req.url, headers: [...req.headers], body: (await decodeFrame(req)).payload }); const frame = encodeFrame({ authorized }); return new Response(frame.body, { headers: { 'content-type': FRAME_MIME, 'content-length': String(frame.length) } }); } }) },
    NATIVE_PUBLICATIONS: { idFromName: name => `object:${name}`, getByName: name => ({ async fetch(req) {
      seen.object.push({ name, headers: [...req.headers], url: req.url });
      return new Response('fixture route response', { headers: { 'content-type': 'application/x-git-upload-pack-advertisement' } });
    } }) },
  };
}
test('real entrypoint and exported DO are activation-disabled without touching integrations', async () => {
  assert.equal(HOSTED_ACTIVATION_REVIEWED, false);
  const blocked = new Proxy({}, { get() { throw new Error('integration touched'); } });
  assert.equal((await entry.fetch(request(), blocked)).status, 503);
  assert.equal((await new HostedPublication({}, blocked).fetch(request())).status, 503);
});
test('prepared edge resolves exact registry then current native scope before Durable Object lookup', async () => {
  for (const write of [false, true]) {
    const e = env(); const response = await preparedHostedFetch(request(write, { 'x-actor': 'forged', 'x-gateway-service-authorization': 'forged' }), e);
    assert.equal(response.status, 200);
    assert.deepEqual(e.seen.native[0].body, { method: 'authorize', repository: 'toy', action: write ? 'write' : 'read' });
    assert.equal(e.seen.native[0].url, 'http://native-container.invalid:8080/native/v1');
    assert.deepEqual(e.seen.object[0].headers, [['authorization', 'Bearer test-only-session']]);
    assert.equal(e.seen.object[0].name, 'toy');
  }
});
test('denied native scope and unknown repository do not read Durable Object or Artifacts', async () => {
  const denied = env(false); assert.equal((await preparedHostedFetch(request(), denied)).status, 403);
  assert.equal(denied.seen.object.length, 0);
  const unknown = env();
  assert.equal((await preparedHostedFetch(new Request('https://edge.invalid/repositories/other.git/info/refs?service=git-upload-pack', { headers: { authorization: 'Bearer test-only-session' } }), unknown)).status, 403);
  assert.equal(unknown.seen.native.length, 0); assert.equal(unknown.seen.object.length, 0);
});
test('DO identity must match trusted repository namespace before storage or native access', async () => {
  const e = env(); const ctx = { id: { toString: () => 'object:other' }, storage: new Proxy({}, { get() { throw new Error('storage touched'); } }) };
  assert.equal((await preparedHostedObjectFetch(request(), ctx, e)).status, 403);
  assert.equal(e.seen.native.length, 0);
});
test('authentication exposes only challenge/exchange and requires configured authority service', async () => {
  const e = env(); let called = false;
  e.HOSTED_NATIVE_CONFIG = JSON.stringify({ authority_origin: 'https://weft.example.invalid' });
  const fetchAuthority = async req => { called = true; assert.equal(req.url, 'https://weft.example.invalid/git/auth/challenge'); assert.deepEqual([...req.headers], [['content-type', 'application/x-protobuf']]); return new Response(new Uint8Array([8, 1]), { headers: { 'content-type': 'application/x-protobuf' } }); };
  const auth = path => new Request(`https://edge.invalid${path}`, { method: 'POST', headers: { 'content-type': 'application/x-protobuf', 'x-actor': 'forged' }, body: new Uint8Array([8, 1]) });
  assert.equal((await preparedHostedFetch(auth('/git/auth/challenge'), e, fetchAuthority)).status, 200); assert.equal(called, true);
  called = false; assert.equal((await preparedHostedFetch(auth('/git/auth/authorize'), e, fetchAuthority)).status, 404); assert.equal(called, false);
});

test('stock Git receives Basic challenge before any native request when credentials are absent', async () => {
  const e = env();
  const response = await preparedHostedFetch(new Request(request().url), e);
  assert.equal(response.status, 401);
  assert.equal(response.headers.get('www-authenticate'), 'Basic realm="Heddle Git"');
  assert.equal(e.seen.native.length, 0); assert.equal(e.seen.object.length, 0);
});
