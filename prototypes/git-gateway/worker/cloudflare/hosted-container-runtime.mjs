// SPDX-License-Identifier: Apache-2.0
import { canonical, digest } from '../publication.mjs';
import { FRAME_MIME, MAX_FRAME_REQUEST } from '../native-frame.mjs';
import { abortable } from '../abortable.mjs';
import { nativeServiceSecret } from '../native-service.mjs';
export const HOSTED_CONTAINER_INSTANCE = 'hosted-native-runtime';
const encoder = new TextEncoder();
const failure = status => new Response(null, { status, headers: { 'cache-control': 'no-store' } });
const secretNames = ['GATEWAY_HOSTED_BISCUIT', 'GATEWAY_HOSTED_SIGNER_PEM', 'GATEWAY_HOSTED_SOURCE_AUTHOR_JSON', 'GATEWAY_ARTIFACTS_CREDENTIAL'];
export function nativeContainerBinding(env) {
  if (!env.NATIVE_CONTAINER?.getByName) throw new Error('Native Container binding absent');
  return { fetch: request => env.NATIVE_CONTAINER.getByName(HOSTED_CONTAINER_INSTANCE).fetch(request) };
}
export async function hostedContainerFetch(request, runtime, env, state, gates = {}) {
  // These gates are source-level review choices supplied by the real controller, never HTTP
  // parameters or environment flags. Network opt-in means general Iroh/QUIC egress, not an
  // HTTP allowlist: Cloudflare blocks non-HTTP ports when enableInternet is false.
  if (gates.activation !== true || gates.nativeNetwork !== true) return failure(503);
  const url = new URL(request.url);
  if (url.href !== 'http://native-container.invalid:8080/native/v1' || request.method !== 'POST' ||
      request.headers.get('content-type') !== FRAME_MIME || request.headers.has('content-encoding')) return failure(404);
  if (state.busy) return failure(429);
  state.busy = true; let handedOff = false, output;
  const deadline = new AbortController(), timer = setTimeout(() => deadline.abort(new Error('Native request deadline')), 120000);
  const aborted = () => deadline.abort(request.signal.reason);
  request.signal.addEventListener('abort', aborted, { once: true });
  if (request.signal.aborted) aborted();
  const cleanup = () => { clearTimeout(timer); request.signal.removeEventListener('abort', aborted); state.busy = false; };
  try {
    if (runtime.id !== env.NATIVE_CONTAINER.idFromName(HOSTED_CONTAINER_INSTANCE).toString()) return failure(403);
    const service = env.NATIVE_SERVICE_AUTHORIZATION, serviceSecret = nativeServiceSecret(service);
    if (typeof service !== 'string' || !service.length || service.length > 4096 || /[\r\n]/.test(service) ||
        await digest(encoder.encode(request.headers.get('x-gateway-service-authorization') || '')) !== await digest(encoder.encode(service))) return failure(403);
    if (!request.headers.get('authorization')) return failure(403);
    const length = request.headers.get('content-length');
    if (length === null || !request.body) return failure(411);
    if (!/^[1-9][0-9]*$/.test(length) || Number(length) > MAX_FRAME_REQUEST) return failure(413);
    if (typeof env.HOSTED_NATIVE_CONFIG !== 'string' || env.HOSTED_NATIVE_CONFIG.length > 64 * 1024) return failure(503);
    const config = JSON.parse(env.HOSTED_NATIVE_CONFIG), root = '/tmp/heddle-hosted';
    if (!config || config.schema !== 1 || !config.scope || typeof config.scope.repository !== 'string') return failure(503);
    const registry = JSON.parse(env.HOSTED_REPOSITORIES);
    if (Object.keys(registry).length !== 1 || !Object.hasOwn(registry, config.scope.repository)) return failure(503);
    // The actual runtime revalidates complete canonical scope and pinned descriptor. Only the
    // operator's existing material is passed; caller-selected paths or secret names are ignored.
    for (const name of secretNames)
      if (typeof env[name] !== 'string' || !env[name].length || env[name].length > 128 * 1024) return failure(503);
    const optionalMint = env.GATEWAY_HOSTED_MINT_ATTACHMENT_JSON || '';
    if (typeof optionalMint !== 'string' || optionalMint.length > 128 * 1024) return failure(503);
    const configured = { ...config, scratch: `${root}/scratch`, gateway_biscuit: `${root}/biscuit`,
      gateway_signer_pem: `${root}/signer.pem`, gateway_source_author: `${root}/source-author.json`,
      gateway_mint_attachment: optionalMint ? `${root}/mint-attachment.json` : null,
      artifacts_credential: `${root}/artifacts-credential`, service_sha256: await digest(encoder.encode(serviceSecret)) };
    const envVars = Object.fromEntries(secretNames.map(name => [name, env[name]]));
    envVars.GATEWAY_HOSTED_CONFIG_JSON = canonical(configured);
    envVars.GATEWAY_HOSTED_MINT_ATTACHMENT_JSON = optionalMint;
    const fingerprint = await digest(encoder.encode(canonical(envVars)));
    if (runtime.running && state.startedFingerprint !== fingerprint) return failure(503);
    await abortable(runtime.startAndWaitForPorts({ ports: [8080], startOptions: { enableInternet: true, envVars },
      cancellationOptions: { abort: deadline.signal, instanceGetTimeoutMS: 10_000, portReadyTimeoutMS: 10_000 } }), deadline.signal);
    state.startedFingerprint = fingerprint;
    const headers = new Headers();
    for (const name of ['authorization', 'content-type', 'x-gateway-service-authorization']) headers.set(name, request.headers.get(name));
    // Workers ignores caller-set Content-Length on generic streams. A FixedLengthStream
    // both enforces the bound and tells the socket transport to use Content-Length instead
    // of chunked encoding, which the deliberately strict native HTTP parser rejects.
    const fixed = new FixedLengthStream(Number(length));
    const pumping = request.body.pipeTo(fixed.writable, { signal: deadline.signal });
    void pumping.catch(() => {});
    output = await abortable(runtime.containerFetch(new Request(request.url, { method: 'POST', headers,
      body: fixed.readable, redirect: 'manual', signal: deadline.signal, duplex: 'half' }), 8080), deadline.signal,
      late => { void late?.body?.cancel().catch(() => {}); });
    await abortable(pumping, deadline.signal);
    if (!output.body) return new Response(null, { status: output.status, headers: output.headers });
    const reader = output.body.getReader(); let finished = false, streamController;
    const finish = () => { if (!finished) { finished = true; cleanup(); deadline.signal.removeEventListener('abort', expired); } };
    const expired = () => { if (!finished) { finish(); void reader.cancel().catch(() => {}); streamController.error(deadline.signal.reason); } };
    const stream = new ReadableStream({
      start(controller) { streamController = controller; deadline.signal.addEventListener('abort', expired, { once: true }); if (deadline.signal.aborted) expired(); },
      async pull(controller) {
        try { const item = await abortable(reader.read(), deadline.signal); if (finished) return;
          if (item.done) { finish(); controller.close(); } else controller.enqueue(item.value);
        } catch (error) { if (!finished) { finish(); controller.error(error); } }
      },
      async cancel() { finish(); await reader.cancel().catch(() => {}); },
    }, { highWaterMark: 0 });
    handedOff = true;
    return new Response(stream, { status: output.status, headers: output.headers });
  } catch { deadline.abort(new Error('Native request failed')); if (output?.body && !output.body.locked) void output.body.cancel().catch(() => {}); return failure(503); }
  finally { if (!handedOff) cleanup(); }
}
