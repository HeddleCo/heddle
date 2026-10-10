// SPDX-License-Identifier: Apache-2.0
// LOCAL SYNTHETIC TEST HARNESS ONLY. Never included in the container image.
// This intentionally maps a test-only HTTPS origin to loopback HTTP. It proves
// code composition, not TLS, Cloudflare bindings, or deployed service identity.
import { readFileSync } from 'node:fs';
import { createServer } from 'node:http';
import worker from './index.mjs';
import { gatewayTransport } from './gateway-transport.mjs';

const fixture = JSON.parse(readFileSync(process.argv[2], 'utf8'));
const READER = 'PUBLIC_TEST_VECTOR_READER_NOT_SECRET_0000000';
const SERVICE = 'PUBLIC_TEST_VECTOR_SERVICE_NOT_SECRET_000000';
if (!Number.isInteger(fixture.port) || fixture.port < 1 || fixture.port > 65535)
  throw new Error('loopback fixture port required');
const pins = Object.keys(fixture.manifests);
const env = {
  CATALOG_REPO: 'local-fixture', PUBLISHED_CATALOG_PINS: JSON.stringify(pins),
  AUTH: { async fetch(_url, init) {
    const { pin } = JSON.parse(init.body);
    return new Response(null, { status: init.headers.authorization === 'Bearer ' + READER && pins.includes(pin) ? 200 : 403 });
  } },
  ARTIFACTS: { async get() {
    return { async readFile({ ref }) { return Object.hasOwn(fixture.manifests, ref) ? new Blob([fixture.manifests[ref]]) : null; },
      [Symbol.dispose]() {} };
  } },
  GATEWAY: gatewayTransport({ origin: 'https://internal.invalid', serviceCredential: SERVICE,
    fetchImpl: async (request, init) => {
      const input = new Request(request, init);
      const url = new URL(input.url);
      if (url.origin !== 'https://internal.invalid') throw new Error('unexpected test origin');
      return fetch(new Request(`http://127.0.0.1:${fixture.port}${url.pathname}${url.search}`, input));
    } }),
};
const server = createServer(async (incoming, outgoing) => {
  try {
    const chunks = []; let length = 0;
    for await (const chunk of incoming) {
      length += chunk.length;
      if (length > 1048576) { outgoing.writeHead(413); outgoing.end(); return; }
      chunks.push(chunk);
    }
    const init = { method: incoming.method, headers: incoming.headers };
    if (incoming.method === 'POST') init.body = Buffer.concat(chunks);
    const response = await worker.fetch(new Request('https://local-fixture.invalid' + incoming.url, init), env);
    outgoing.writeHead(response.status, Object.fromEntries(response.headers));
    for await (const chunk of response.body ?? []) outgoing.write(chunk);
    outgoing.end();
  } catch {
    outgoing.writeHead(503); outgoing.end();
  }
});
server.requestTimeout = 30000;
server.headersTimeout = 10000;
server.listen(0, '127.0.0.1', () => console.log('LOCAL_FIXTURE_WORKER_PORT=' + server.address().port));
