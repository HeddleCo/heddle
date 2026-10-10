// SPDX-License-Identifier: Apache-2.0
// Shared front-door/native-hop route and byte-budget contract. Pinned views stay immutable.
export const READ_REQUEST_LIMIT = 1024 * 1024;
export const RECEIVE_REQUEST_LIMIT = 16 * 1024 * 1024 + 64 * 1024;
export function gitRoute(request, origin) {
  const url = new URL(request.url);
  if ((origin && url.origin !== origin) || url.username || url.password || request.url.includes('#') || url.pathname.includes('%'))
    return { error: 404 };
  const match = /^\/(views|repositories)\/([a-z0-9-]+)\.git\/(info\/refs|git-upload-pack|git-receive-pack|native-refresh|native-reconcile)$/.exec(url.pathname);
  if (!match) return { error: 404 };
  const [, kind, target, endpoint] = match;
  if ((kind === 'views' && !/^[0-9a-f]{40}$/.test(target)) ||
      (kind === 'repositories' && !/^[a-z][a-z0-9-]{0,63}$/.test(target))) return { error: 404 };
  if (endpoint === 'native-refresh' || endpoint === 'native-reconcile') {
    if (kind !== 'repositories' || request.method !== 'POST' || url.search || request.url.endsWith('?')) return { error: 405 };
    if (request.headers.get('content-type') !== 'application/json' || request.headers.has('content-encoding')) return { error: 415 };
    return { kind, target, endpoint, service: endpoint, write: true, path: url.pathname, limit: 1024 };
  }
  const service = endpoint === 'info/refs' ? url.search.slice('?service='.length) : endpoint;
  const write = service === 'git-receive-pack';
  if (!['git-upload-pack', 'git-receive-pack'].includes(service) || (kind === 'views' && write) ||
      !((request.method === 'GET' && endpoint === 'info/refs' && url.search === `?service=${service}`) ||
        (request.method === 'POST' && endpoint === service && !url.search)) || request.url.endsWith('?'))
    return { error: 405 };
  if (request.headers.has('content-encoding') || (request.method === 'POST' &&
      request.headers.get('content-type') !== `application/x-${service}-request`)) return { error: 415 };
  return { kind, target, endpoint, service, write, path: url.pathname + url.search,
    limit: write ? RECEIVE_REQUEST_LIMIT : READ_REQUEST_LIMIT };
}
