// SPDX-License-Identifier: Apache-2.0
// Injected controller contract, separately unit-tested; activation remains blocked.
import { canonicalJson, readPolicy, readerAllowed, serviceAllowed } from './policy.mjs';
import { nativeConfig, CONTAINER_INSTANCE, NATIVE_ORIGIN } from './composition.mjs';
const failure = status => new Response(null, { status, headers: { 'cache-control': 'no-store' } });

export async function preparedContainerFetch(request, runtime, env, state, now = () => Date.now() / 1000) {
  const url = new URL(request.url);
  const match = /^\/views\/([0-9a-f]{40})\.git\/(info\/refs|git-upload-pack)$/.exec(url.pathname);
  if (url.origin !== NATIVE_ORIGIN || url.hash || !match) return failure(404);
  const [, pin, endpoint] = match;
  if (!((request.method === 'GET' && endpoint === 'info/refs' && url.search === '?service=git-upload-pack') ||
    (request.method === 'POST' && endpoint === 'git-upload-pack' && !url.search))) return failure(405);
  if (state.busy) return failure(429);
  state.busy = true;
  let handedOff = false;
  try {
    if (runtime.id !== env.NATIVE_CONTAINER.idFromName(CONTAINER_INSTANCE).toString()) return failure(403);
    const policy = readPolicy(env.GATEWAY_POLICY);
    try {
      if (!await serviceAllowed(policy, request.headers.get('x-gateway-service-authorization'), 'gateway', pin, now()) ||
          !await readerAllowed(policy, request.headers.get('authorization'), pin, now())) return failure(403);
    } catch { return failure(403); }
    const config = canonicalJson(nativeConfig(policy));
    // Existing running instances with unknown/stale env snapshots do not serve.
    // A separately approved stop/restart is needed after changing policy/config.
    if (runtime.running && state.startedConfig !== config) return failure(503);
    await runtime.startAndWaitForPorts({ ports: [8080],
      startOptions: { enableInternet: false, envVars: { GATEWAY_DEMO_CONFIG_JSON: config } },
      cancellationOptions: { abort: request.signal, instanceGetTimeoutMS: 10_000, portReadyTimeoutMS: 10_000 } });
    state.startedConfig = config;
    // Startup may cross expiry. Recheck before dispatching to native ingress.
    if (!await readerAllowed(policy, request.headers.get('authorization'), pin, now()) ||
        !await serviceAllowed(policy, request.headers.get('x-gateway-service-authorization'), 'gateway', pin, now())) return failure(403);
    const upstream = await runtime.containerFetch(request, 8080);
    if (!upstream.body) return new Response(null, { status: upstream.status, headers: upstream.headers });
    // Hold singleflight until bytes finish/cancel, not merely until headers arrive.
    const reader = upstream.body.getReader();
    let done = false, timer, streamController;
    const finish = () => { if (!done) { done = true; clearTimeout(timer); state.busy = false; } };
    const stream = new ReadableStream({
      start(controller) {
        streamController = controller;
        timer = setTimeout(() => { finish(); void reader.cancel().catch(() => {}); controller.error(new Error('Gateway response deadline')); }, 30_000);
      },
      async pull(controller) {
        try {
          const result = await reader.read();
          if (done) return;
          if (result.done) { finish(); controller.close(); }
          else controller.enqueue(result.value);
        } catch { if (!done) { finish(); streamController.error(new Error('Gateway response unavailable')); } }
      },
      async cancel() { finish(); await reader.cancel().catch(() => {}); },
    }, { highWaterMark: 0 });
    handedOff = true;
    return new Response(stream, { status: upstream.status, headers: upstream.headers });
  } catch { return failure(503); }
  finally { if (!handedOff) state.busy = false; }
}
