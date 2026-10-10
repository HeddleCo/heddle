// SPDX-License-Identifier: Apache-2.0
// These are public test vectors and injected binding shapes, never deployable policy.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { canonicalJson, readPolicy, credentialDigest, readerAllowed, serviceAllowed } from './policy.mjs';
import { preparedEdgeFetch, preparedBridgeFetch, nativeConfig, CONTAINER_INSTANCE, NATIVE_ORIGIN, BRIDGE_ORIGIN } from './composition.mjs';
import { preparedContainerFetch } from './controller-runtime.mjs';
const READER = 'PUBLIC_READER_TEST_VECTOR_' + 'R'.repeat(32);
const SERVICE = 'PUBLIC_SERVICE_TEST_VECTOR_' + 'S'.repeat(32);
const pin = 'a'.repeat(40), otherPin = 'b'.repeat(40), now = () => 1000;
const bytes = new TextEncoder().encode('public synthetic bundle response fixture');
const hash = async data => Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', data)), v => v.toString(16).padStart(2, '0')).join('');
const digest = await hash(bytes);
const manifest = { schema: 1, repository: 'actual-agent-demo', source: 'native-app', thread: 'demo/priority-sort',
  state: 'hs-' + 'a'.repeat(52), git_oid: 'c'.repeat(40), mode: 'snapshot', policy_epoch: 1 };
const policy = {
  schema: 1, issued_at: 900, expires_at: 1800, catalog: 'native-view-01a0efd3', pins: { [pin]: manifest },
  readers: [{ sha256: await credentialDigest(`Bearer ${READER}`), expires_at: 1790,
    views: [[manifest.repository, manifest.source, manifest.thread, manifest.state, 1]] }],
  gateway_service: { sha256: await credentialDigest(`Bearer ${SERVICE}`), expires_at: 1780, pins: [pin] },
  bridge: { expires_at: 1700, pins: [pin], sources: ['native-app'] },
  sources: { 'native-app': { key: `native/${digest}.bundle`, sha256: digest,
    authorized_threads: ['main', 'demo/open-filter', 'demo/priority-sort'] } },
};
const copy = () => structuredClone(policy);
function bindings(p = policy) {
  const calls = [];
  const env = {
    GATEWAY_POLICY: canonicalJson(p), GATEWAY_SERVICE_CREDENTIAL: SERVICE, CATALOG_REPO: p.catalog,
    ARTIFACTS: { async get(repo) {
      calls.push(['catalog', repo]);
      return { async readFile({ ref, path }) { calls.push(['manifest', ref, path]); return new Blob([canonicalJson(manifest)]); },
        [Symbol.dispose]() { calls.push(['dispose']); } };
    } },
    NATIVE_CONTAINER: {
      idFromName(name) { assert.equal(name, CONTAINER_INSTANCE); return { toString: () => 'd'.repeat(64) }; },
      getByName(name) {
        calls.push(['container', name]);
        return { async fetch(request) { calls.push(['fetch', request]); return new Response('git bytes', { headers: { 'content-type': 'application/x-git-upload-pack-advertisement' } }); } };
      },
    },
    NATIVE_BUNDLES: { async get(key) { calls.push(['r2', key]); return { size: bytes.length, arrayBuffer: async () => bytes.buffer }; } },
  };
  return { env, calls };
}
const request = (authorization = `Bearer ${READER}`, extra = {}, selectedPin = pin) => new Request(`https://public.invalid/views/${selectedPin}.git/info/refs?service=git-upload-pack`, {
  headers: { authorization, ...extra },
});
const nativeRequest = (auth = `Bearer ${READER}`, service = `Bearer ${SERVICE}`) => new Request(`${NATIVE_ORIGIN}/views/${pin}.git/info/refs?service=git-upload-pack`, {
  headers: { authorization: auth, 'x-gateway-service-authorization': service },
});
const ctx = { containerId: 'd'.repeat(64), className: 'NativeGatewayContainer' };

test('strict policy validates canonical exact scopes and short expiry', () => {
  assert.deepEqual(readPolicy(canonicalJson(policy)), policy);
  assert.throws(() => readPolicy(JSON.stringify(policy)));
  assert.throws(() => readPolicy(canonicalJson(policy).replace('"schema":1', '"schema":1,"schema":1')));
  assert.throws(() => readPolicy('x'.repeat(65537)));
  for (const change of [p => p.expires_at = 10000, p => p.readers.push(p.readers[0]), p => p.readers[0].views = [],
    p => p.gateway_service.pins = [], p => p.bridge.sources = [], p => p.sources['native-app'].key = '../arbitrary',
    p => p.sources['native-app'].authorized_threads = ['main'], p => p.gateway_service.sha256 = p.readers[0].sha256,
    p => p.pins[pin].policy_epoch = 9007199254740992, p => p.readers[0].unexpected = true]) {
    const p = copy(); change(p); assert.throws(() => readPolicy(canonicalJson(p)));
  }
});

test('reader and hop authority are separate, exact and expired at boundary', async () => {
  assert.equal(await readerAllowed(policy, `Bearer ${READER}`, pin, 1000), true);
  assert.equal(await readerAllowed(policy, `Bearer ${SERVICE}`, pin, 1000), false);
  assert.equal(await readerAllowed(policy, `Bearer ${READER}`, otherPin, 1000), false);
  assert.equal(await readerAllowed(policy, `Bearer ${READER}`, pin, 1900), false);
  assert.equal(await readerAllowed(policy, `Bearer ${READER}`, pin, 99), false);
  assert.equal(await serviceAllowed(policy, `Bearer ${SERVICE}`, 'gateway', pin, 1000), true);
  assert.equal(await serviceAllowed(policy, `Bearer ${READER}`, 'gateway', pin, 1000), false);
  await assert.rejects(() => credentialDigest('Bearer fixture-selector'));
});

test('edge rejects absent or invalid authority before binding reads', async () => {
  for (const authorization of ['', 'Bearer fixture-selector', `Bearer ${SERVICE}`]) {
    const { env, calls } = bindings();
    assert.equal((await preparedEdgeFetch(request(authorization, { 'x-demo-reader': 'demo-reader' }), env, now)).status, 403);
    assert.deepEqual(calls, []);
  }
  for (const change of [env => delete env.GATEWAY_POLICY, env => delete env.GATEWAY_SERVICE_CREDENTIAL,
    env => env.GATEWAY_SERVICE_CREDENTIAL = READER, env => delete env.NATIVE_CONTAINER]) {
    const { env, calls } = bindings(); change(env);
    assert.equal((await preparedEdgeFetch(request(), env, now)).status, 503); assert.deepEqual(calls, []);
  }
  const { env, calls } = bindings();
  assert.equal((await preparedEdgeFetch(request(), env, () => 1800)).status, 503);
  assert.equal((await preparedEdgeFetch(request(undefined, {}, otherPin), env, now)).status, 503);
  assert.deepEqual(calls, []);
});

test('edge composes approved AUTH, pinned catalog and one fixed container', async () => {
  const { env, calls } = bindings();
  const response = await preparedEdgeFetch(request(undefined, {
    'x-demo-reader': 'spoof', 'x-gateway-service-authorization': 'spoof', origin: 'https://bad.invalid', 'x-forwarded-for': 'bad',
  }), env, now);
  assert.equal(response.status, 200); assert.equal(await response.text(), 'git bytes');
  assert.deepEqual(calls.slice(0, 4).map(c => c[0]), ['catalog', 'manifest', 'dispose', 'container']);
  assert.equal(calls[3][1], CONTAINER_INSTANCE);
  const outgoing = calls.find(c => c[0] === 'fetch')[1];
  assert.equal(new URL(outgoing.url).origin, NATIVE_ORIGIN);
  assert.equal(outgoing.headers.get('authorization'), `Bearer ${READER}`);
  assert.equal(outgoing.headers.get('x-gateway-service-authorization'), `Bearer ${SERVICE}`);
  for (const name of ['origin', 'x-forwarded-for', 'x-demo-reader']) assert.equal(outgoing.headers.has(name), false);
});

test('edge checks remote manifest against exact approved bytes and disposes capability', async () => {
  const { env, calls } = bindings();
  env.ARTIFACTS.get = async () => ({ readFile: async () => new Blob([canonicalJson({ ...manifest, policy_epoch: 2 })]),
    [Symbol.dispose]() { calls.push(['dispose']); } });
  assert.equal((await preparedEdgeFetch(request(), env, now)).status, 503);
  assert.deepEqual(calls, [['dispose']]);
});

test('service expiry between catalog read and dispatch blocks container', async () => {
  const { env, calls } = bindings(); let count = 0;
  const response = await preparedEdgeFetch(request(), env, () => ++count < 3 ? 1000 : 1800);
  assert.equal(response.status, 403); await response.text();
  assert.equal(calls.some(c => c[0] === 'container'), false);
});

test('bridge requires actual platform context, never a spoofed identity header', async () => {
  for (const badCtx of [undefined, { ...ctx, containerId: 'e'.repeat(64) }, { ...ctx, className: 'OtherClass' }]) {
    const { env, calls } = bindings();
    const response = await preparedBridgeFetch(new Request(`${BRIDGE_ORIGIN}/native/native-app`, {
      headers: { 'x-container-id': ctx.containerId },
    }), env, badCtx, now);
    assert.equal(response.status, 403); assert.deepEqual(calls, []);
  }
});

test('bridge rejects nonfixed origin, route, method, credentials, unlisted source and expiry', async () => {
  for (const url of ['https://gateway-bindings.internal/native/native-app', 'http://other.internal/native/native-app',
    `${BRIDGE_ORIGIN}/native/native-app?other=1`, `${BRIDGE_ORIGIN}/native/../private`, `${BRIDGE_ORIGIN}/secrets`]) {
    const { env, calls } = bindings(); assert.equal((await preparedBridgeFetch(new Request(url), env, ctx, now)).status, 404); assert.deepEqual(calls, []);
  }
  for (const route of ['/native/unknown', `/catalog/${otherPin}`]) {
    const { env, calls } = bindings(); assert.equal((await preparedBridgeFetch(new Request(BRIDGE_ORIGIN + route), env, ctx, now)).status, 403); assert.deepEqual(calls, []);
  }
  for (const init of [{ method: 'POST' }, { headers: { authorization: `Bearer ${READER}` } }]) {
    const { env, calls } = bindings(); assert.equal((await preparedBridgeFetch(new Request(`${BRIDGE_ORIGIN}/native/native-app`, init), env, ctx, now)).status, 404); assert.deepEqual(calls, []);
  }
  const { env, calls } = bindings();
  assert.equal((await preparedBridgeFetch(new Request(`${BRIDGE_ORIGIN}/native/native-app`), env, ctx, () => 1700)).status, 403);
  assert.deepEqual(calls, []);
});

test('identity-bound bridge returns digest checked R2 bytes and independently pinned Artifacts metadata', async () => {
  const { env, calls } = bindings();
  const native = await preparedBridgeFetch(new Request(`${BRIDGE_ORIGIN}/native/native-app`), env, ctx, now);
  assert.equal(native.status, 200); assert.deepEqual(new Uint8Array(await native.arrayBuffer()), bytes);
  assert.deepEqual(calls, [['r2', `native/${digest}.bundle`]]);
  const catalog = await preparedBridgeFetch(new Request(`${BRIDGE_ORIGIN}/catalog/${pin}`), env, ctx, now);
  assert.equal(catalog.status, 200); assert.equal(await catalog.text(), canonicalJson(manifest));
  assert.equal(catalog.headers.get('cache-control'), 'no-store');
});

test('R2 digest mismatch, absence and byte limit fail closed', async () => {
  for (const object of [null, { size: 9 * 1024 * 1024 }, { size: 3, arrayBuffer: async () => new Uint8Array([1, 2, 3]).buffer }]) {
    const { env } = bindings(); env.NATIVE_BUNDLES.get = async () => object;
    assert.equal((await preparedBridgeFetch(new Request(`${BRIDGE_ORIGIN}/native/native-app`), env, ctx, now)).status, 503);
  }
});

test('native startup contains only bounded approved configuration and digests', () => {
  const config = nativeConfig(policy);
  assert.deepEqual(Object.keys(config).sort(), ['schema', 'expires_at', 'published_pins', 'reader_sha256', 'service_sha256', 'views', 'descriptors', 'authorized_threads'].sort());
  assert.equal(config.expires_at, 1700); assert.deepEqual(config.published_pins, [pin]);
  assert.deepEqual(config.views, [manifest]); assert.equal(config.descriptors['native-app'], digest);
  assert.equal(JSON.stringify(config).includes(READER), false); assert.equal(JSON.stringify(config).includes(SERVICE), false);
});

function runtime() {
  const calls = [];
  return { calls, id: ctx.containerId, running: false,
    async startAndWaitForPorts(args) { calls.push(['start', args]); },
    async containerFetch(req, port) { calls.push(['native', req, port]); return new Response('native bytes'); },
  };
}
test('controller validates both gates and namespace identity before native startup', async () => {
  for (const req of [nativeRequest('', `Bearer ${SERVICE}`), nativeRequest(`Bearer ${READER}`, ''), nativeRequest(`Bearer ${SERVICE}`, `Bearer ${SERVICE}`)]) {
    const { env } = bindings(); const rt = runtime(); const state = {};
    assert.equal((await preparedContainerFetch(req, rt, env, state, now)).status, 403);
    assert.deepEqual(rt.calls, []); assert.equal(state.busy, false);
  }
  const { env } = bindings(); const rt = runtime(); rt.id = 'x';
  assert.equal((await preparedContainerFetch(nativeRequest(), rt, env, {}, now)).status, 403);
  assert.deepEqual(rt.calls, []);
});

test('controller singleflight includes complete response and stops admitting until consumed', async () => {
  const { env } = bindings(); const rt = runtime(); const state = {};
  const first = await preparedContainerFetch(nativeRequest(), rt, env, state, now);
  assert.equal(first.status, 200); assert.equal(state.busy, true);
  assert.equal((await preparedContainerFetch(nativeRequest(), rt, env, state, now)).status, 429);
  assert.equal(await first.text(), 'native bytes'); assert.equal(state.busy, false);
  const [, options] = rt.calls[0]; assert.equal(options.startOptions.enableInternet, false);
  assert.deepEqual(Object.keys(options.startOptions.envVars), ['GATEWAY_DEMO_CONFIG_JSON']);
  assert.deepEqual(JSON.parse(options.startOptions.envVars.GATEWAY_DEMO_CONFIG_JSON), nativeConfig(policy));
  assert.equal(rt.calls[1][2], 8080);
});

test('controller cancellation releases singleflight and stale running configuration is denied', async () => {
  const { env } = bindings(); const rt = runtime(); const state = {};
  const response = await preparedContainerFetch(nativeRequest(), rt, env, state, now);
  await response.body.cancel(); assert.equal(state.busy, false);
  rt.running = true; state.startedConfig = 'stale'; rt.calls.length = 0;
  assert.equal((await preparedContainerFetch(nativeRequest(), rt, env, state, now)).status, 503);
  assert.deepEqual(rt.calls, []);
});

test('controller rechecks expiry after startup', async () => {
  const { env } = bindings(); const rt = runtime(); let instant = 1000;
  rt.startAndWaitForPorts = async () => { instant = 1900; };
  assert.equal((await preparedContainerFetch(nativeRequest(), rt, env, {}, () => instant)).status, 403);
  assert.deepEqual(rt.calls, []);
});

test('draft has disabled routes, missing image, no public exposures or secret/test defaults', async () => {
  const config = JSON.parse(await readFile(new URL('./wrangler.draft.json', import.meta.url)));
  assert.equal(config.workers_dev, false); assert.equal(config.preview_urls, false);
  assert.equal(config.containers[0].max_instances, 1); assert.equal(config.containers[0].instance_type, 'basic');
  assert.equal(config.containers[0].scheduling_policy, 'default'); assert.equal(config.containers[0].ssh.enabled, false);
  assert.match(config.containers[0].image, /BLOCKED/); assert.equal(config.routes, undefined);
  for (const name of ['GATEWAY_POLICY', 'GATEWAY_SERVICE_CREDENTIAL']) assert.equal(config.vars[name], undefined);
  const controller = await readFile(new URL('./controller.mjs', import.meta.url), 'utf8');
  assert.match(controller, /ACTIVATION_REVIEWED = false/); assert.match(controller, /enableInternet = false/);
  assert.doesNotMatch(controller, /allowedHosts\s*=/); assert.doesNotMatch(controller, /\.outbound\s*=/);
  const packageJson = JSON.parse(await readFile(new URL('./package.json', import.meta.url)));
  assert.equal(packageJson.private, true); assert.equal(packageJson.scripts.deploy, undefined);
});

test('bridge withholds responses if authority expires during either storage read', async () => {
  for (const kind of ['native', 'catalog']) {
    const { env } = bindings(); let time = 1000;
    if (kind === 'native') env.NATIVE_BUNDLES.get = async () => {
      time = 1700; return { size: bytes.length, arrayBuffer: async () => bytes.buffer };
    };
    else env.ARTIFACTS.get = async () => ({
      async readFile() { time = 1700; return new Blob([canonicalJson(manifest)]); }, [Symbol.dispose]() {},
    });
    const path = kind === 'native' ? '/native/native-app' : `/catalog/${pin}`;
    assert.equal((await preparedBridgeFetch(new Request(BRIDGE_ORIGIN + path), env, ctx, () => time)).status, 403);
  }
});

test('edge re-reads configured policy on each request and rejects catalog mismatch', async () => {
  const { env, calls } = bindings();
  let response = await preparedEdgeFetch(request(), env, now); assert.equal(response.status, 200); await response.text();
  const changed = copy(); changed.readers[0].sha256 = '9'.repeat(64); env.GATEWAY_POLICY = canonicalJson(changed); calls.length = 0;
  assert.equal((await preparedEdgeFetch(request(), env, now)).status, 403); assert.deepEqual(calls, []);
  env.CATALOG_REPO = 'unapproved-catalog';
  assert.equal((await preparedEdgeFetch(request(), env, now)).status, 503); assert.deepEqual(calls, []);
});

test('dedicated Cloudflare image uses native Rust only and no fixture/authority files', async () => {
  const dockerfile = await readFile(new URL('./Dockerfile', import.meta.url), 'utf8');
  const ignores = await readFile(new URL('./Dockerfile.dockerignore', import.meta.url), 'utf8');
  assert.match(dockerfile, /USER 65532:65532/);
  assert.match(dockerfile, /ENTRYPOINT \["\/usr\/local\/bin\/gateway_host"\]/);
  assert.doesNotMatch(dockerfile, /python3|\.py\b/);
  assert.match(dockerfile, /--example gateway_host/);
  assert.doesNotMatch(dockerfile, /COPY[^\n]*(?:\.dev\.vars|\.env|test_|demo|native\.bundle|catalog\.git|readers\.json|services\.json)/);
  assert.match(ignores, /\*\*\/\.env\*/); assert.match(ignores, /\*\*\/\.heddle/);
  assert.doesNotMatch(ignores, /!prototypes/);
});

test('live disclosure is denied until canonical Heddle authority is connected', async () => {
  const { env, calls } = bindings();
  const response = await preparedBridgeFetch(new Request(BRIDGE_ORIGIN + `/disclosure/${pin}`), env, ctx, now);
  assert.equal(response.status, 503);
  assert.equal(response.headers.get('cache-control'), 'no-store');
  assert.deepEqual(calls, []);
});
