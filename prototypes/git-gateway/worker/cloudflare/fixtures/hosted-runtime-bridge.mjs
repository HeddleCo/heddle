// SPDX-License-Identifier: Apache-2.0
// Launch: node hosted-runtime-bridge.mjs /absolute/path/to/synthetic-config.json
// JSON-lines control on stdin; JSON-lines status on stdout. No user/session/content IDs
// are emitted. This is a loopback-only integration fixture, never a deployment entrypoint.
import { readFile, readdir, mkdir, realpath, stat } from 'node:fs/promises';
import { isAbsolute, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createInterface } from 'node:readline';
import { execFile as callbackExecFile } from 'node:child_process';
import { promisify } from 'node:util';
import { performance } from 'node:perf_hooks';
import { build } from 'esbuild';
import { Miniflare, convertV4MiniflareOptions, Log, LogLevel } from 'miniflare';
import { FRAME_MIME, MAX_FRAME_HEADER, MAX_FRAME_REQUEST, MAX_FRAME_RESPONSE } from '../../native-frame.mjs';

const execFile = promisify(callbackExecFile);
const root = dirname(fileURLToPath(import.meta.url));
const FIXED_NATIVE = 'http://native-container.invalid:8080/native/v1';
const METHODS = new Set(['authorize', 'bootstrap-plan', 'refresh-plan', 'prepare', 'validate-plan',
  'bootstrap', 'refresh', 'submit', 'catalog-publish', 'project']);
const phases = Object.create(null);
let mf, config, options, chosenPort, readyUrl, dropNextSubmit = false, droppedSubmitResponses = 0;
let pauseNextProject = false, projectPause = null;
let memoryTimer, sampling;
const observedWorkerd = new Set();
const memory = {
  sample_interval_ms: 200, child_tree_limit: 64,
  scope: 'Actual local processes. workerd includes runtime, Durable Objects and R2; this is not Cloudflare isolate heap, billing, or production capacity evidence.',
  node: { samples: 0, current_rss_bytes: 0, peak_sampled_rss_bytes: 0, os_high_water_rss_bytes: 0 },
  workerd: { samples: 0, processes_observed: 0, max_concurrent_processes: 0, current_combined_rss_bytes: 0,
    peak_combined_rss_bytes: 0, peak_single_process_rss_bytes: 0, peak_os_high_water_rss_bytes: 0,
    child_tree_truncations: 0, read_failures: 0, observed_processes_capped: false },
};
let stopping = false;
const emit = value => process.stdout.write(JSON.stringify(value) + '\n');
const check = (ok, code) => { if (!ok) throw Object.assign(new Error(code), { fixtureCode: code }); };
const errorCode = error => error?.fixtureCode ?? 'fixture-operation-failed';
async function sampleMemory() {
  if (sampling) return sampling;
  sampling = (async () => {
    const rss = process.memoryUsage().rss;
    memory.node.samples++; memory.node.current_rss_bytes = rss;
    memory.node.peak_sampled_rss_bytes = Math.max(memory.node.peak_sampled_rss_bytes, rss);
    memory.node.os_high_water_rss_bytes = Math.max(memory.node.os_high_water_rss_bytes, process.resourceUsage().maxRSS * 1024);
    // Some local runtimes omit /proc/<pid>/task/<pid>/children. A bounded PPid
    // index provides the same own-child relationship without reading command lines.
    const names = (await readdir('/proc')).filter(name => /^[1-9][0-9]*$/.test(name)).sort((a, b) => Number(a) - Number(b));
    const index = new Map(); let truncated = names.length > memory.child_tree_limit;
    for (const name of names.slice(0, memory.child_tree_limit)) {
      try {
        const status = await readFile(`/proc/${name}/status`, 'utf8');
        check(status.length <= 16384, 'fixture-process-status-bound');
        const parent = /^PPid:\s+([0-9]+)$/m.exec(status);
        check(parent, 'fixture-process-parent-unavailable');
        index.set(Number(name), { parent: Number(parent[1]), status });
      } catch { memory.workerd.read_failures++; }
    }
    const pending = [process.pid], seen = new Set(); let combined = 0, count = 0;
    while (pending.length && seen.size < memory.child_tree_limit) {
      const pid = pending.shift(); if (seen.has(pid)) continue; seen.add(pid);
      for (const [child, value] of index) if (value.parent === pid) pending.push(child);
      if (pid === process.pid || !index.has(pid)) continue;
      try {
        if ((await readFile(`/proc/${pid}/comm`, 'utf8')).trim() !== 'workerd') continue;
        const { status } = index.get(pid);
        const resident = /^VmRSS:\s+([0-9]+) kB$/m.exec(status), high = /^VmHWM:\s+([0-9]+) kB$/m.exec(status);
        check(resident && high, 'fixture-process-memory-unavailable');
        const current = Number(resident[1]) * 1024, highWater = Number(high[1]) * 1024;
        combined += current; count++;
        memory.workerd.peak_single_process_rss_bytes = Math.max(memory.workerd.peak_single_process_rss_bytes, current);
        memory.workerd.peak_os_high_water_rss_bytes = Math.max(memory.workerd.peak_os_high_water_rss_bytes, highWater);
        if (observedWorkerd.size < memory.child_tree_limit) observedWorkerd.add(pid);
        else if (!observedWorkerd.has(pid)) memory.workerd.observed_processes_capped = true;
      } catch { memory.workerd.read_failures++; }
    }
    memory.workerd.samples++; memory.workerd.current_combined_rss_bytes = combined;
    memory.workerd.peak_combined_rss_bytes = Math.max(memory.workerd.peak_combined_rss_bytes, combined);
    memory.workerd.max_concurrent_processes = Math.max(memory.workerd.max_concurrent_processes, count);
    memory.workerd.processes_observed = observedWorkerd.size;
    memory.workerd.child_tree_truncations += Number(truncated || pending.length > 0);
  })();
  try { await sampling; } finally { sampling = undefined; }
}
function beginPhase(phase) {
  const start = performance.now(); let finished = false;
  return (failed = false) => {
    if (finished) return; finished = true;
    const count = phases[phase] ??= { count: 0, failed: 0, elapsed_ms: 0 };
    count.count++; count.failed += Number(failed);
    count.elapsed_ms = Math.round((count.elapsed_ms + performance.now() - start) * 1000) / 1000;
  };
}
async function measure(phase, action) {
  const finish = beginPhase(phase); let failed = false;
  try {
    const result = await action();
    if (result instanceof Response && result.status !== 200) failed = true;
    return result;
  } catch (error) { failed = true; throw error; }
  finally { finish(failed); }
}
async function bounded(response, maximum) {
  const length = response.headers.get('content-length');
  check(length === null || /^(0|[1-9][0-9]*)$/.test(length) && Number(length) <= maximum, 'fixture-response-bound');
  const chunks = []; let size = 0;
  if (response.body) {
    const reader = response.body.getReader();
    try {
      while (true) {
        const { done, value } = await reader.read(); if (done) break;
        size += value.byteLength; check(size <= maximum, 'fixture-response-bound'); chunks.push(value);
      }
    } catch (error) { void reader.cancel().catch(() => {}); throw error; }
    finally { reader.releaseLock(); }
  }
  const bytes = new Uint8Array(size); let offset = 0;
  for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.byteLength; }
  return bytes;
}
// Preserve backpressure from workerd to the actual single-request Rust listener.
// Only a chunk and a byte count are held; no complete projected body is materialized.
export function boundedProjectionBody(response, maximum = MAX_FRAME_RESPONSE) {
  const declared = response.headers.get('content-length');
  check(/^[1-9][0-9]*$/.test(declared ?? '') && Number(declared) <= maximum && response.body,
    'fixture-response-bound');
  const reader = response.body.getReader(), length = Number(declared);
  const finish = beginPhase('native_project_response_stream'); let total = 0;
  return new ReadableStream({
    async pull(controller) {
      try {
        const { value, done } = await reader.read();
        if (done) {
          check(total === length, 'fixture-response-length'); reader.releaseLock(); finish(); controller.close(); return;
        }
        total += value.byteLength;
        check(total <= length && total <= maximum, 'fixture-response-bound'); controller.enqueue(value);
      } catch (error) { void reader.cancel(error).catch(() => {}); reader.releaseLock(); finish(true); controller.error(error); }
    },
    async cancel(reason) { try { await reader.cancel(reason); } finally { reader.releaseLock(); finish(true); } },
  }, { highWaterMark: 0 });
}
async function maybePauseProject(signal) {
  if (!pauseNextProject) return;
  pauseNextProject = false;
  await measure('fixture_project_pause', () => new Promise((resolve, reject) => {
    let timer;
    const complete = error => {
      clearTimeout(timer); signal.removeEventListener('abort', aborted); projectPause = null;
      if (error) reject(error); else resolve();
    };
    const aborted = () => complete(Object.assign(new Error(), { fixtureCode: 'fixture-project-pause-aborted' }));
    projectPause = { release: () => complete(), fail: code => complete(Object.assign(new Error(), { fixtureCode: code })) };
    timer = setTimeout(() => projectPause?.fail('fixture-project-pause-timeout'), 30000);
    signal.addEventListener('abort', aborted, { once: true });
    if (signal.aborted) aborted();
  }));
}
// Read only the bounded routing header. The native listener remains responsible for
// canonical metadata and attachment validation; every original frame byte is forwarded.
export async function readNativeFrameRequest(request) {
  check(request.headers.get('content-type') === FRAME_MIME, 'fixture-native-frame-required');
  const declared = request.headers.get('content-length');
  check(/^[1-9][0-9]*$/.test(declared ?? '') && Number(declared) <= MAX_FRAME_REQUEST,
    'fixture-native-body-bound');
  const length = Number(declared), reader = request.body?.getReader();
  check(reader && length >= 8, 'fixture-native-frame-truncated');
  const chunks = []; let total = 0, index = 0;
  const cancel = reason => { void reader.cancel(reason).catch(() => {}); reader.releaseLock(); };
  async function prefix(size) {
    check(size <= length, 'fixture-native-frame-truncated');
    while (total < size) {
      const { value, done } = await reader.read();
      check(!done, 'fixture-native-frame-truncated');
      total += value.byteLength;
      check(total <= length, 'fixture-native-frame-length'); chunks.push(value);
    }
    const bytes = new Uint8Array(size); let offset = 0;
    for (const chunk of chunks) {
      const part = chunk.subarray(0, Math.min(chunk.byteLength, size - offset));
      bytes.set(part, offset); offset += part.byteLength;
      if (offset === size) break;
    }
    return bytes;
  }
  try {
    const first = await prefix(8);
    check(first[0] === 72 && first[1] === 71 && first[2] === 70 && first[3] === 49,
      'fixture-native-frame-magic');
    const headerLength = new DataView(first.buffer).getUint32(4, false);
    check(headerLength > 0 && headerLength <= MAX_FRAME_HEADER, 'fixture-native-header-bound');
    const bytes = await prefix(8 + headerLength);
    const header = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes.subarray(8)));
    check(header && typeof header === 'object' && !Array.isArray(header) &&
      header.payload && typeof header.payload === 'object' && !Array.isArray(header.payload) &&
      Array.isArray(header.parts) && METHODS.has(header.payload.method), 'fixture-native-method-refused');
    const body = new ReadableStream({
      async pull(controller) {
        try {
          if (index < chunks.length) { const chunk = chunks[index]; chunks[index++] = null; controller.enqueue(chunk); return; }
          const { value, done } = await reader.read();
          if (done) {
            check(total === length, 'fixture-native-frame-length'); reader.releaseLock(); controller.close(); return;
          }
          total += value.byteLength; check(total <= length, 'fixture-native-frame-length'); controller.enqueue(value);
        } catch (error) { cancel(error); controller.error(error); }
      },
      cancel,
    });
    return { method: header.payload.method, body, length };
  } catch (error) { cancel(error); throw error; }
}
async function nativeService(request) {
  let requestBody;
  try {
    check(request.url === FIXED_NATIVE && request.method === 'POST', 'fixture-native-route-refused');
    const frame = await readNativeFrameRequest(request);
    const { method, length } = frame; requestBody = frame.body;
    if (method === 'project') await maybePauseProject(request.signal);
    return await measure(`native_${method}`, async () => {
      // Only the transport host changes. Both authorization headers and every original
      // HGF1 frame byte pass unchanged to the real configured loopback Rust listener.
      const headers = new Headers(request.headers);
      headers.delete('host'); headers.set('content-length', String(length));
      const response = await fetch(`${config.native_origin}/native/v1`, {
        method: 'POST', headers, body: requestBody, duplex: 'half', redirect: 'manual',
        signal: AbortSignal.any([request.signal, AbortSignal.timeout(125000)]),
      });
      if (response.status !== 200) process.stderr.write(JSON.stringify({
        event: 'fixture_native_status', method, status: response.status,
      }) + '\n');
      const outgoing = new Headers(response.headers);
      outgoing.delete('transfer-encoding');
      if (method === 'project' && response.status === 200)
        return new Response(boundedProjectionBody(response), { status: response.status, headers: outgoing });
      const body = await bounded(response, MAX_FRAME_RESPONSE);
      if (method === 'submit' && response.status === 200 && dropNextSubmit) {
        dropNextSubmit = false; droppedSubmitResponses++;
        // The complete real receipt arrived before this one injected response loss.
        // Returning failure leaves the real coordinator's prepared journal durable.
        return new Response(null, { status: 503 });
      }
      outgoing.delete('content-length');
      return new Response(body, { status: response.status, headers: outgoing });
    });
  } catch (error) {
    process.stderr.write(JSON.stringify({ event: 'fixture_native_failure', code: errorCode(error) }) + '\n');
    if (requestBody && !requestBody.locked) void requestBody.cancel().catch(() => {});
    return new Response(null, { status: 503 });
  }
}
async function catalogRead(request) {
  try {
    const url = new URL(request.url);
    check(request.method === 'GET' && url.origin === 'http://catalog-fixture.invalid' &&
      !url.search && !url.hash && /^\/manifest\/[0-9a-f]{40}$/.test(url.pathname) &&
      !request.headers.has('authorization') && !request.headers.has('x-gateway-service-authorization'),
    'fixture-catalog-read-refused');
    return await measure('catalog_git_read', async () => {
      const pin = url.pathname.slice('/manifest/'.length);
      const { stdout } = await execFile('git', ['--no-pager', '--no-optional-locks', '--git-dir', config.catalog_git_dir,
        'show', `${pin}:manifest.json`], { encoding: 'buffer', maxBuffer: 65536, timeout: 30000,
        env: { ...process.env, GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null',
          GIT_NO_REPLACE_OBJECTS: '1', GIT_TERMINAL_PROMPT: '0' } });
      check(stdout.length > 0 && stdout.length <= 65536, 'fixture-catalog-metadata-bound');
      return new Response(stdout, { headers: { 'content-type': 'application/json' } });
    });
  } catch { return new Response(null, { status: 503 }); }
}
async function start() {
  options.port = chosenPort;
  mf = new Miniflare(convertV4MiniflareOptions(options));
  const url = await mf.ready;
  chosenPort = Number(url.port); readyUrl = url.origin;
  return readyUrl;
}
async function stop() {
  if (stopping) return; stopping = true;
  clearInterval(memoryTimer); await sampleMemory();
  projectPause?.fail('fixture-stopping');
  await mf?.dispose(); mf = undefined;
}
function stats() {
  return { actual: ['workerd R2', 'SQLite Durable Object', 'Rust native HTTP', 'local Git catalog reads'],
    synthetic_fault: 'Optional one-time response loss after completed real submit',
    drop_armed: dropNextSubmit, dropped_submit_responses: droppedSubmitResponses,
    pause_next_project: pauseNextProject, project_paused: projectPause !== null,
    phase_scope: 'native_project ends at response headers; native_project_response_stream ends at body completion or cancellation; other native phases include bounded response consumption',
    memory,
    phases: Object.fromEntries(Object.entries(phases).sort()) };
}
async function main() {
  check(process.argv.length === 3 && isAbsolute(process.argv[2]), 'fixture-config-path-required');
  const raw = await readFile(process.argv[2]); check(raw.length <= 65536, 'fixture-config-bound');
  config = JSON.parse(raw.toString('utf8'));
  check(config && typeof config === 'object' && !Array.isArray(config), 'fixture-config-invalid');
  const keys = ['native_origin', 'service_authorization', 'repository', 'catalog_name', 'bootstrap_native_state',
    'catalog_git_dir', 'persistence_path', 'listen_port'];
  check(Object.keys(config).every(key => keys.includes(key)), 'fixture-config-unknown-field');
  const native = new URL(config.native_origin);
  check(native.protocol === 'http:' && native.hostname === '127.0.0.1' && native.port && native.pathname === '/' &&
    !native.username && !native.password && !native.search && !native.hash, 'fixture-native-loopback-required');
  config.native_origin = native.origin;
  check(typeof config.service_authorization === 'string' && /^Bearer [^\r\n]{32,256}$/.test(config.service_authorization),
    'fixture-service-grant-required');
  check(/^[a-z][a-z0-9-]{0,63}$/.test(config.repository) && /^[a-z][a-z0-9-]{0,127}$/.test(config.catalog_name) &&
    /^hs-[0-9a-z]{52}$/.test(config.bootstrap_native_state), 'fixture-scope-invalid');
  check(isAbsolute(config.catalog_git_dir) && isAbsolute(config.persistence_path), 'fixture-paths-absolute-required');
  config.catalog_git_dir = await realpath(config.catalog_git_dir);
  check((await stat(config.catalog_git_dir)).isDirectory(), 'fixture-catalog-directory-required');
  const bare = await execFile('git', ['--git-dir', config.catalog_git_dir, 'rev-parse', '--is-bare-repository'],
    { encoding: 'utf8', timeout: 10000, maxBuffer: 4096 });
  check(bare.stdout.trim() === 'true', 'fixture-bare-catalog-required');
  await mkdir(config.persistence_path, { recursive: true });
  chosenPort = config.listen_port ?? 0;
  check(Number.isSafeInteger(chosenPort) && chosenPort >= 0 && chosenPort <= 65535, 'fixture-port-invalid');
  await sampleMemory();
  memoryTimer = setInterval(() => { void sampleMemory().catch(() => { memory.workerd.read_failures++; }); }, memory.sample_interval_ms);
  memoryTimer.unref();
  const bundle = await build({ entryPoints: [join(root, 'hosted-runtime-bridge-worker.mjs')], bundle: true,
    format: 'esm', platform: 'browser', target: 'es2022', write: false, logLevel: 'silent' });
  options = { name: 'heddle-real-hosted-runtime-fixture', script: bundle.outputFiles[0].text, modules: true,
    compatibilityDate: '2026-10-01', host: '127.0.0.1', port: chosenPort,
    durableObjects: { PUBLICATIONS: { className: 'HostedRuntimeBridgeObject', useSQLite: true } },
    r2Buckets: { NATIVE_SOURCE: 'heddle-real-native-source-fixture' },
    resourcePersistencePath: config.persistence_path, resourceTmpPath: join(config.persistence_path, 'runtime-tmp'),
    bindings: { CATALOG_NAME: config.catalog_name, NATIVE_SERVICE_AUTHORIZATION: config.service_authorization,
      HOSTED_REPOSITORIES: JSON.stringify({ [config.repository]: { catalog: config.catalog_name,
        bootstrap_native: config.bootstrap_native_state } }) },
    serviceBindings: { NATIVE_HTTP: nativeService, CATALOG_READ: catalogRead },
    cf: false, telemetry: { enabled: false }, log: new Log(LogLevel.ERROR),
    outboundService: () => new Response(null, { status: 503 }),
  };
  emit({ ready_url: await start() });
  const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
  for await (const line of lines) {
    let command;
    try {
      check(line.length <= 1024, 'fixture-control-bound');
      const value = JSON.parse(line);
      check(value && Object.keys(value).join(',') === 'command', 'fixture-control-invalid');
      command = value.command;
      if (command === 'drop-next-submit-response') {
        check(!dropNextSubmit, 'fixture-fault-already-armed'); dropNextSubmit = true;
        emit({ command, status: 'armed' });
      } else if (command === 'pause-next-project') {
        check(!pauseNextProject && projectPause === null, 'fixture-project-pause-already-armed');
        pauseNextProject = true; emit({ command, status: 'armed' });
      } else if (command === 'release-project') {
        check(projectPause !== null, 'fixture-project-not-paused');
        emit({ command, status: 'released' }); projectPause.release();
      } else if (command === 'restart') {
        projectPause?.fail('fixture-restarting');
        await measure('workerd_restart', async () => { await mf.dispose(); await start(); });
        emit({ command, status: 'ready', ready_url: readyUrl });
      } else if (command === 'stats') { await sampleMemory(); emit({ command, status: 'ready', ...stats() }); }
      else if (command === 'stop') {
        await stop(); emit({ command, status: 'stopped', ...stats() }); lines.close(); return;
      } else throw Object.assign(new Error(), { fixtureCode: 'fixture-control-unknown' });
    } catch (error) {
      emit({ ...(typeof command === 'string' && ['drop-next-submit-response', 'pause-next-project', 'release-project', 'restart', 'stats', 'stop'].includes(command) ? { command } : {}),
        status: 'error', code: errorCode(error) });
    }
  }
  await stop();
}
if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  process.on('SIGTERM', () => { void stop().finally(() => process.exit(0)); });
  process.on('SIGINT', () => { void stop().finally(() => process.exit(0)); });
  main().catch(async error => { emit({ status: 'error', code: errorCode(error) }); await stop(); process.exitCode = 1; });
}
