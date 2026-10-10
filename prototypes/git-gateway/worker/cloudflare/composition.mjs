// SPDX-License-Identifier: Apache-2.0
// Executable local contract. The actual Worker entrypoint does not activate it.
import front from '../index.mjs';
import { gatewayTransport } from '../gateway-transport.mjs';
import { readNativeBundle } from '../native-source.mjs';
import { ArtifactsCatalog, readManifestBlob } from '../catalog.mjs';
import { canonicalJson, readPolicy, readerAllowed, serviceAllowed } from './policy.mjs';

export const CONTAINER_INSTANCE = 'approved-native-view-singleton';
export const NATIVE_ORIGIN = 'https://native-container.invalid';
export const BRIDGE_ORIGIN = 'http://gateway-bindings.internal';
const clock = () => Date.now() / 1000;
const failure = status => new Response(null, { status, headers: { 'cache-control': 'no-store' } });

export function edgeAuthority(policy, now = clock) {
  return {
    async fetch(input, init) {
      try {
        const req = new Request(input, init);
        if (req.url !== 'https://authority.invalid/authorize-view' || req.method !== 'POST') return failure(404);
        // This handler only receives the front door's generated tiny request.
        const text = await req.text();
        if (text.length > 512) return failure(403);
        const body = JSON.parse(text);
        if (!body || Array.isArray(body) || Object.keys(body).sort().join(',') !== 'catalog,pin' || body.catalog !== policy.catalog) return failure(403);
        return failure(await readerAllowed(policy, req.headers.get('authorization'), body.pin, now()) ? 200 : 403);
      } catch { return failure(403); }
    },
  };
}

function pinnedArtifacts(binding, policy) {
  // Check the actual remote immutable manifest against independently configured
  // policy bytes. Being present in a catalog never creates reader authority.
  return {
    async get(repository) {
      if (repository !== policy.catalog) throw new Error('Catalog unavailable');
      const repo = await binding.get(repository);
      return {
        async readFile({ ref, path }) {
          if (path !== 'manifest.json' || !Object.hasOwn(policy.pins, ref)) throw new Error('Pin unavailable');
          const blob = await repo.readFile({ ref, path });
          if (canonicalJson(await readManifestBlob(blob)) !== canonicalJson(policy.pins[ref]))
            throw new Error('Catalog does not match approved view');
          return blob;
        },
        [Symbol.dispose]() { repo[Symbol.dispose](); },
      };
    },
  };
}

export async function preparedEdgeFetch(request, env, now = clock) {
  try {
    const policy = readPolicy(env.GATEWAY_POLICY);
    if (env.CATALOG_REPO !== policy.catalog) return failure(503);
    const serviceBearer = `Bearer ${env.GATEWAY_SERVICE_CREDENTIAL}`;
    // Missing or expired hop secret denies before any catalog read or DO lookup.
    const url = new URL(request.url);
    const pin = /^\/views\/([0-9a-f]{40})\.git\//.exec(url.pathname)?.[1];
    if (!pin || !await serviceAllowed(policy, serviceBearer, 'gateway', pin, now())) return failure(503);
    if (!env.NATIVE_CONTAINER || !env.ARTIFACTS) return failure(503);
    const transport = gatewayTransport({ origin: NATIVE_ORIGIN,
      serviceCredential: env.GATEWAY_SERVICE_CREDENTIAL,
      fetchImpl: outgoing => {
        // Recheck current time immediately before crossing the next hop.
        return serviceAllowed(policy, serviceBearer, 'gateway', pin, now()).then(allowed => {
          if (!allowed) return failure(403);
          return env.NATIVE_CONTAINER.getByName(CONTAINER_INSTANCE).fetch(outgoing);
        });
      },
    });
    return front.fetch(request, {
      AUTH: edgeAuthority(policy, now), GATEWAY: transport,
      ARTIFACTS: pinnedArtifacts(env.ARTIFACTS, policy), CATALOG_REPO: policy.catalog,
      PUBLISHED_CATALOG_PINS: JSON.stringify(Object.keys(policy.pins)),
    });
  } catch { return failure(503); }
}

export function nativeConfig(policy) {
  const reader = policy.readers[0];
  const pins = Object.keys(policy.pins).sort();
  return {
    schema: 1,
    expires_at: Math.min(policy.expires_at, reader.expires_at, policy.gateway_service.expires_at, policy.bridge.expires_at),
    published_pins: pins,
    reader_sha256: reader.sha256,
    service_sha256: policy.gateway_service.sha256,
    views: pins.map(pin => policy.pins[pin]),
    descriptors: Object.fromEntries(Object.entries(policy.sources).map(([source, d]) => [source, d.sha256])),
    authorized_threads: Object.fromEntries(Object.entries(policy.sources).map(([source, d]) => [source, d.authorized_threads])),
  };
}

export async function preparedBridgeFetch(request, env, ctx, now = clock) {
  // ctx is the trusted outbound-handler context created by the Cloudflare
  // ContainerProxy, NEVER any value parsed from a header, URL, or request body.
  const url = new URL(request.url);
  if (url.origin !== BRIDGE_ORIGIN || url.search || url.hash || request.method !== 'GET' || request.body ||
      request.headers.has('authorization') || request.headers.has('x-gateway-service-authorization')) return failure(404);
  const match = /^\/(catalog|native|disclosure)\/([a-z0-9-]+)$/.exec(url.pathname);
  if (!match) return failure(404);
  try {
    if (!ctx || ctx.className !== 'NativeGatewayContainer' ||
        ctx.containerId !== env.NATIVE_CONTAINER.idFromName(CONTAINER_INSTANCE).toString()) return failure(403);
    const p = readPolicy(env.GATEWAY_POLICY);
    if (env.CATALOG_REPO !== p.catalog) return failure(503);
    const instant = now();
    if (!Number.isFinite(instant) || instant < p.issued_at || instant >= p.expires_at || instant >= p.bridge.expires_at) return failure(403);
    const stillActive = () => now() >= p.issued_at && now() < Math.min(p.expires_at, p.bridge.expires_at);
    const [, kind, target] = match;
    if (kind === 'disclosure') {
      if (!p.bridge.pins.includes(target)) return failure(403);
      // No canonical live Heddle authority is connected. Static demo policy and
      // immutable R2 bytes cannot attest current disclosure/admission/retention.
      // Never mint a proof here from the configured digest or a caller's claim.
      return failure(503);
    }
    if (kind === 'catalog') {
      if (!p.bridge.pins.includes(target)) return failure(403);
      const manifest = await new ArtifactsCatalog(pinnedArtifacts(env.ARTIFACTS, p), p.catalog, p.bridge.pins).resolve(target);
      if (!stillActive()) return failure(403);
      const body = canonicalJson(manifest);
      return new Response(body, { headers: { 'content-type': 'application/octet-stream', 'cache-control': 'no-store',
        'content-length': String(new TextEncoder().encode(body).length) } });
    }
    if (!p.bridge.sources.includes(target)) return failure(403);
    const bytes = await readNativeBundle(env.NATIVE_BUNDLES, p.sources[target]);
    if (!stillActive()) return failure(403);
    return new Response(bytes, { headers: { 'content-type': 'application/octet-stream', 'cache-control': 'no-store',
      'content-length': String(bytes.byteLength) } });
  } catch { return failure(503); }
}
