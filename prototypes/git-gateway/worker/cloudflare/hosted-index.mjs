// SPDX-License-Identifier: Apache-2.0
// Concrete hosted-write entrypoint. Activation is deliberately a reviewed source-code gate.
// No bindings, bucket, repository, publisher or credentials are created by this module.
import front from '../index.mjs';
import { gitRoute } from '../git-route.mjs';
import { composeHostedGateway, nativeHttp } from '../native-http.mjs';
import { nativeContainerBinding } from './hosted-container-runtime.mjs';
import { sessionAuthorityBinding } from './session-authority.mjs';
export const HOSTED_ACTIVATION_REVIEWED = false;
const failure = status => new Response(null, { status, headers: { 'cache-control': 'no-store' } });
function configuration(env) {
  if (typeof env.HOSTED_REPOSITORIES !== 'string' || env.HOSTED_REPOSITORIES.length > 16 * 1024)
    throw new Error('Hosted repository registry unavailable');
  const repositories = JSON.parse(env.HOSTED_REPOSITORIES);
  if (!repositories || Array.isArray(repositories) || typeof repositories !== 'object' ||
      Object.keys(repositories).length < 1 || Object.keys(repositories).length > 128) throw new Error('Hosted registry limit');
  for (const [repository, value] of Object.entries(repositories)) {
    if (!/^[a-z][a-z0-9-]{0,63}$/.test(repository) || !value ||
        Object.keys(value).sort().join(',') !== 'bootstrap_native,catalog' ||
        !/^[a-z][a-z0-9-]{0,127}$/.test(value.catalog) || !/^hs-[0-9a-z]{52}$/.test(value.bootstrap_native))
      throw new Error('Invalid trusted hosted repository');
  }
  return repositories;
}
export async function preparedHostedObjectFetch(request, ctx, env) {
  try {
    const route = gitRoute(request);
    if (route.error || route.kind !== 'repositories') return failure(route.error || 404);
    if (!request.headers.get('authorization')) return new Response(null, { status: 401, headers: { 'cache-control': 'no-store', 'www-authenticate': 'Basic realm="Heddle Git"' } });
    const registry = configuration(env);
    if (!Object.hasOwn(registry, route.target) || !env.NATIVE_PUBLICATIONS ||
        ctx.id.toString() !== env.NATIVE_PUBLICATIONS.idFromName(route.target).toString()) return failure(403);
    const selected = registry[route.target];
    return await composeHostedGateway({ storage: ctx.storage, bucket: env.NATIVE_SOURCE, artifacts: env.ARTIFACTS,
      catalogNames: { [route.target]: selected.catalog }, bootstrapHeads: { [route.target]: selected.bootstrap_native },
      binding: nativeContainerBinding(env), serviceAuthorization: env.NATIVE_SERVICE_AUTHORIZATION }).fetch(request);
  } catch { return failure(503); }
}
export class HostedPublication {
  constructor(ctx, env) { this.ctx = ctx; this.env = env; }
  fetch(request) {
    if (!HOSTED_ACTIVATION_REVIEWED) return Promise.resolve(failure(503));
    return preparedHostedObjectFetch(request, this.ctx, this.env);
  }
}
export async function preparedHostedFetch(request, env, fetchImpl = fetch) {
  try {
    if (new URL(request.url).pathname.startsWith('/git/auth/'))
      return front.fetch(request, { AUTH: sessionAuthorityBinding(env, fetchImpl) });
    const route = gitRoute(request);
    if (route.error || route.kind !== 'repositories') return failure(route.error || 404);
    if (!request.headers.get('authorization')) return new Response(null, { status: 401, headers: { 'cache-control': 'no-store', 'www-authenticate': 'Basic realm="Heddle Git"' } });
    const registry = configuration(env);
    if (!Object.hasOwn(registry, route.target)) return failure(403);
    const selected = registry[route.target];
    const rpc = nativeHttp({ binding: nativeContainerBinding(env), serviceAuthorization: env.NATIVE_SERVICE_AUTHORIZATION });
    const authority = { async fetch(input, init) {
      const req = new Request(input, init);
      if (req.url !== 'https://authority.invalid/authorize-repository' || req.method !== 'POST') return failure(404);
      const body = await req.text(); if (body.length > 512) return failure(403);
      const value = JSON.parse(body);
      if (!value || Object.keys(value).sort().join(',') !== 'catalog,operation,repository' ||
          value.catalog !== selected.catalog || value.repository !== route.target || !['read', 'write'].includes(value.operation)) return failure(403);
      const result = await rpc('authorize', { repository: route.target, action: value.operation }, req.headers.get('authorization') || '');
      return failure(result.payload.authorized === true ? 200 : 403);
    } };
    return front.fetch(request, { AUTH: authority, ARTIFACTS: env.ARTIFACTS, CATALOG_REPO: selected.catalog,
      GATEWAY: { fetch(outgoing) {
        if (!env.NATIVE_PUBLICATIONS?.getByName) return failure(503);
        return env.NATIVE_PUBLICATIONS.getByName(route.target).fetch(outgoing);
      } },
    });
  } catch { return failure(503); }
}
export default { fetch(request, env) {
  return HOSTED_ACTIVATION_REVIEWED ? preparedHostedFetch(request, env) : failure(503);
} };
