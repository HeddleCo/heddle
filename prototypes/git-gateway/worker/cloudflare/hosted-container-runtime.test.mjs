import test from 'node:test';
import assert from 'node:assert/strict';
import { webcrypto } from 'node:crypto';
import { hostedContainerFetch, HOSTED_CONTAINER_INSTANCE } from './hosted-container-runtime.mjs';
import { FRAME_MIME } from '../native-frame.mjs';
import { digest } from '../publication.mjs';
const SERVICE = 'Bearer ' + 'S'.repeat(64);
if (!globalThis.crypto) globalThis.crypto = webcrypto;
// Node-only stand-in. The socket test separately exercises workerd's native FixedLengthStream.
globalThis.FixedLengthStream ??= class extends TransformStream {
  constructor(length) { let seen = 0; super({ transform(chunk, controller) {
    seen += chunk.byteLength; if (seen > length) throw new Error('long body'); controller.enqueue(chunk);
  }, flush() { if (seen !== length) throw new Error('short body'); } }); }
};
const request = (extra = {}) => new Request('http://native-container.invalid:8080/native/v1', { method: 'POST',
  headers: { 'content-type': FRAME_MIME, 'content-length': '3', authorization: 'Bearer test-only-user', 'x-gateway-service-authorization': SERVICE, ...extra }, body: '{}\n' });
function fixture() {
  const seen = [], env = { NATIVE_CONTAINER: { idFromName: name => `id:${name}` }, NATIVE_SERVICE_AUTHORIZATION: SERVICE,
    HOSTED_REPOSITORIES: JSON.stringify({ toy: { catalog: 'catalog', bootstrap_native: 'hs-fixture' } }),
    HOSTED_NATIVE_CONFIG: JSON.stringify({ schema: 1, scope: { repository: 'toy' }, gateway_signer_pem: '/caller-wrong-path' }),
    GATEWAY_HOSTED_BISCUIT: 'non-authorizing-fixture-biscuit', GATEWAY_HOSTED_SIGNER_PEM: 'not-a-real-key',
    GATEWAY_HOSTED_SOURCE_AUTHOR_JSON: '{}', GATEWAY_ARTIFACTS_CREDENTIAL: 'non-authorizing-fixture-artifacts' };
  const runtime = { id: `id:${HOSTED_CONTAINER_INSTANCE}`, running: false,
    async startAndWaitForPorts(value) { seen.push({ start: value }); },
    async containerFetch(req, port) { seen.push({ request: req, port, body: await req.text() }); return Response.json({ fixture: true }); } };
  return { seen, env, runtime, state: { busy: false } };
}
test('native Container activation and QUIC network opt-in both default deny before integrations', async () => {
  const touched = new Proxy({}, { get() { throw new Error('integration accessed'); } });
  for (const gates of [{}, { activation: true }, { nativeNetwork: true }, { activation: true, nativeNetwork: false }])
    assert.equal((await hostedContainerFetch(request(), touched, touched, touched, gates)).status, 503);
});
test('wrong virtual origin, service grant, instance, missing secret and oversized body never start Container', async () => {
  for (const mutate of [f => { f.runtime.id = 'wrong'; }, f => { f.env.NATIVE_SERVICE_AUTHORIZATION = 'different'; },
    f => { delete f.env.GATEWAY_HOSTED_BISCUIT; }]) {
    const f = fixture(); mutate(f);
    const response = await hostedContainerFetch(request(), f.runtime, f.env, f.state, { activation: true, nativeNetwork: true });
    assert.notEqual(response.status, 200); assert.equal(f.seen.length, 0); assert.equal(f.state.busy, false);
  }
  const f = fixture();
  assert.equal((await hostedContainerFetch(new Request('http://evil.invalid/native/v1'), f.runtime, f.env, f.state, { activation: true, nativeNetwork: true })).status, 404);
  assert.equal((await hostedContainerFetch(request({ 'content-length': '999999999' }), f.runtime, f.env, f.state, { activation: true, nativeNetwork: true })).status, 413);
  assert.equal(f.seen.length, 0);
});
test('explicit local opt-in passes existing fixture material only, fixed paths, and bounded singleflight', async () => {
  const f = fixture();
  const response = await hostedContainerFetch(request({ 'x-actor': 'forged' }), f.runtime, f.env, f.state, { activation: true, nativeNetwork: true });
  assert.equal(response.status, 200); assert.equal(f.seen[0].start.startOptions.enableInternet, true);
  const startup = f.seen[0].start.startOptions.envVars;
  assert.equal(startup.GATEWAY_HOSTED_SIGNER_PEM, 'not-a-real-key');
  const config = JSON.parse(startup.GATEWAY_HOSTED_CONFIG_JSON);
  assert.equal(config.gateway_signer_pem, '/tmp/heddle-hosted/signer.pem');
  assert.equal(config.scratch, '/tmp/heddle-hosted/scratch');
  assert.equal(config.service_sha256, 'eafca4c0fae9d1fc15080c4a3b9170bfbecec255164f6328fbf663279481e808');
  assert.equal(config.service_sha256, await digest(new TextEncoder().encode(SERVICE.slice(7))));
  assert.notEqual(config.service_sha256, await digest(new TextEncoder().encode(SERVICE)));
  assert.equal(f.seen[1].request.headers.get('x-gateway-service-authorization'), SERVICE);
  assert.equal(f.seen[1].request.headers.has('x-actor'), false);
  assert.equal(f.seen[1].port, 8080); assert.equal(f.state.busy, true);
  assert.equal((await hostedContainerFetch(request(), f.runtime, f.env, f.state, { activation: true, nativeNetwork: true })).status, 429);
  await response.text(); assert.equal(f.state.busy, false);
  assert.equal(Object.values(f.state).some(v => typeof v === 'string' && v.includes('fixture')), false);
});
test('a running Container with unknown or changed configuration fails closed until explicit restart', async () => {
  const f = fixture(); f.runtime.running = true;
  assert.equal((await hostedContainerFetch(request(), f.runtime, f.env, f.state, { activation: true, nativeNetwork: true })).status, 503);
  assert.equal(f.seen.length, 0);
});
test('missing and contradictory stream lengths fail closed without a successful native response', async () => {
  const f = fixture(); const absent = request(); absent.headers.delete('content-length');
  assert.equal((await hostedContainerFetch(absent, f.runtime, f.env, f.state, { activation: true, nativeNetwork: true })).status, 411);
  assert.equal(f.seen.length, 0);
  for (const length of ['2', '4']) {
    const response = await hostedContainerFetch(request({ 'content-length': length }), f.runtime, f.env, f.state,
      { activation: true, nativeNetwork: true });
    assert.equal(response.status, 503); assert.equal(f.state.busy, false);
  }
});
