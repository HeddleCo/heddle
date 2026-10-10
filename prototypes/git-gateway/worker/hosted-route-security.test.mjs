// SPDX-License-Identifier: Apache-2.0
// Independent adversarial tests of the real composed edge and native-hop transport.
import test from 'node:test';
import assert from 'node:assert/strict';
import { createFrontDoor } from './index.mjs';
import { gatewayTransport } from './gateway-transport.mjs';
import { RECEIVE_REQUEST_LIMIT } from './git-route.mjs';

const front = createFrontDoor();
const service = 'PUBLIC_SYNTHETIC_SERVICE_' + 's'.repeat(32);
const session = 'Bearer PUBLIC_SYNTHETIC_SESSION_' + 'x'.repeat(32);
const discovery = '/repositories/toy.git/info/refs?service=git-receive-pack';
const receive = '/repositories/toy.git/git-receive-pack';
function fixture({ authStatus = 200, nativeStatus = 200 } = {}) {
  const authorizations = [], native = [];
  const env = {
    CATALOG_REPO: 'metadata-catalog',
    ARTIFACTS: { get() { throw new Error('stable route must resolve current catalog only behind native authority'); } },
    AUTH: { async fetch(url, init) {
      authorizations.push({ url, init, scope: JSON.parse(init.body) });
      return new Response(null, { status: authStatus });
    } },
    GATEWAY: gatewayTransport({ origin: 'https://native.test.invalid', serviceCredential: service,
      async fetchImpl(request) {
        native.push(request);
        return new Response(nativeStatus === 200 ? 'native protocol bytes' : null, { status: nativeStatus,
          headers: { 'content-type': 'application/x-git-receive-pack-result', location: 'https://attacker.invalid', 'set-cookie': 'must-not-escape' } });
      } }),
  };
  return { env, authorizations, native };
}
const get = (path, headers = {}) => new Request('https://edge.test.invalid' + path, { headers: { authorization: session, ...headers } });
const post = (path, body = 'PACK fixture', headers = {}) => new Request('https://edge.test.invalid' + path, {
  method: 'POST', body, duplex: 'half', headers: { authorization: session, 'content-type': 'application/x-git-receive-pack-request', ...headers },
});

test('both receive discovery and POST require the exact repository write grant at both hops', async () => {
  for (const request of [get(discovery), post(receive)]) {
    const f = fixture();
    const response = await front.fetch(request, f.env);
    assert.equal(response.status, 200);
    assert.equal(await response.text(), 'native protocol bytes');
    assert.equal(f.authorizations.length, 1);
    assert.equal(f.authorizations[0].url, 'https://authority.invalid/authorize-repository');
    assert.deepEqual(f.authorizations[0].scope, { catalog: 'metadata-catalog', repository: 'toy', operation: 'write' });
    assert.equal(f.authorizations[0].init.headers.authorization, session);
    assert.equal(f.native.length, 1);
    assert.equal(f.native[0].headers.get('authorization'), session);
    assert.equal(f.native[0].headers.get('x-gateway-service-authorization'), `Bearer ${service}`);
    assert.equal(response.headers.get('cache-control'), 'no-store');
    for (const header of ['location', 'set-cookie']) assert.equal(response.headers.get(header), null);
  }
});

test('upload discovery gets read scope and never silently widens to repository write', async () => {
  const f = fixture();
  const response = await front.fetch(get('/repositories/toy.git/info/refs?service=git-upload-pack'), f.env);
  assert.equal(response.status, 200); await response.text();
  assert.deepEqual(f.authorizations[0].scope, { catalog: 'metadata-catalog', repository: 'toy', operation: 'read' });
});

test('denied repository auth never consults catalog or sends native bytes', async () => {
  for (const request of [get(discovery), post(receive)]) {
    const f = fixture({ authStatus: 403 });
    const response = await front.fetch(request, f.env);
    assert.equal(response.status, 403);
    assert.equal(response.headers.get('cache-control'), 'no-store');
    assert.equal(f.native.length, 0);
  }
});

test('client identity, account, gateway and forwarding headers cannot cross the native trust boundary', async () => {
  const forged = {
    'x-gateway-service-authorization': 'Bearer attacker',
    'x-gateway-write-authorization': 'Bearer attacker',
    'x-heddle-actor': 'victim', 'x-heddle-account': 'victim', 'x-authenticated-user': 'victim',
    'x-forwarded-for': '127.0.0.1', 'x-forwarded-host': 'trusted.invalid', cookie: 'session=attacker',
    'content-length': '1', 'git-protocol': 'version=2',
  };
  const f = fixture();
  const response = await front.fetch(post(receive, 'bounded received bytes', forged), f.env);
  assert.equal(response.status, 200); await response.text();
  const native = f.native[0];
  for (const name of Object.keys(forged)) {
    if (name === 'x-gateway-service-authorization') assert.equal(native.headers.get(name), `Bearer ${service}`);
    else if (name === 'content-length') assert.equal(native.headers.get(name), String('bounded received bytes'.length));
    else if (name === 'git-protocol') assert.equal(native.headers.get(name), 'version=2');
    else assert.equal(native.headers.get(name), null, name);
  }
  assert.equal(await native.text(), 'bounded received bytes');
});

test('forbidden stable and pinned write spellings never reach authorization or native transport', async () => {
  const pin = 'a'.repeat(40);
  for (const path of [
    '/repositories/-toy.git/info/refs?service=git-receive-pack',
    '/repositories/Toy.git/info/refs?service=git-receive-pack',
    '/repositories/toy%2fgit.git/info/refs?service=git-receive-pack',
    '/repositories/toy.git/info/refs?service=git-receive-pack&service=git-upload-pack',
    '/repositories/toy.git/info/refs?service=git-receive-pack&actor=owner',
    '/repositories/toy.git/info/refs?service=git-receive-pack#untrusted',
    `/views/${pin}.git/info/refs?service=git-receive-pack`,
  ]) {
    const f = fixture();
    const response = await front.fetch(get(path), f.env);
    assert.ok([404, 405].includes(response.status), `${path}: ${response.status}`);
    assert.equal(f.authorizations.length, 0, path);
    assert.equal(f.native.length, 0, path);
  }
  for (const path of [receive + '?', receive + '?service=git-receive-pack', `/views/${pin}.git/git-receive-pack`]) {
    const f = fixture();
    const response = await front.fetch(post(path), f.env);
    assert.ok([404, 405].includes(response.status), path);
    assert.equal(f.authorizations.length, 0); assert.equal(f.native.length, 0);
  }
});

test('receive request bounds apply to actual bytes through both hops even with forged short length', async () => {
  const bytes = new Uint8Array(RECEIVE_REQUEST_LIMIT).fill(19);
  const f = fixture();
  const response = await front.fetch(post(receive, bytes, { 'content-length': '1' }), f.env);
  assert.equal(response.status, 200); await response.text();
  assert.equal(f.native[0].headers.get('content-length'), String(bytes.length));
  assert.deepEqual(new Uint8Array(await f.native[0].arrayBuffer()), bytes);
  const tooBig = fixture();
  const denied = await front.fetch(post(receive, new Uint8Array(RECEIVE_REQUEST_LIMIT + 1), { 'content-length': '1' }), tooBig.env);
  assert.equal(denied.status, 413);
  assert.equal(tooBig.native.length, 0, 'no partially forwarded hostile pack');
});

test('native redirects cannot relay credentials or turn a push into apparent success', async () => {
  for (const status of [301, 302, 303, 307, 308]) {
    const f = fixture({ nativeStatus: status });
    const response = await front.fetch(post(receive), f.env);
    assert.ok(response.status >= 400, status);
    assert.equal(response.headers.get('location'), null);
    assert.equal(f.native.length, 1);
    assert.equal(f.native[0].redirect, 'manual');
    assert.equal(await response.text(), '');
  }
});
