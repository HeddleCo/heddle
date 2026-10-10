// SPDX-License-Identifier: Apache-2.0
// Bounded raw native bridge. Only the small canonical metadata header is buffered wholesale.
import { canonical } from './publication.mjs';
import { abortable } from './abortable.mjs';
export const FRAME_MIME = 'application/vnd.heddle.native-frame-v1';
export const MAX_FRAME_HEADER = 256 * 1024, MAX_FRAME_REQUEST = 112 * 1024 * 1024, MAX_FRAME_RESPONSE = 144 * 1024 * 1024;
const encoder = new TextEncoder(), decoder = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });
const check = (value, reason) => { if (!value) throw new Error(reason); };
const maxPart = name => name === 'proof' ? 17 * 1024 * 1024 : name === 'request' ? 17 * 1024 * 1024 :
  name === 'output' ? 96 * 1024 * 1024 : name === 'manifest' ? 64 * 1024 : /^artifact\/[0-9a-f]{64}$/.test(name) ? 64 * 1024 * 1024 : -1;
function validate(payload, parts, maximum, headerLength) {
  check(payload && typeof payload === 'object' && !Array.isArray(payload), 'Frame payload required');
  check(Array.isArray(parts) && parts.length <= 260, 'Frame part count');
  const names = new Set(); let length = 8 + headerLength, native = 0, artifacts = 0;
  for (const part of parts) {
    check(part && Object.keys(part).sort().join(',') === 'length,name' && !names.has(part.name) &&
      Number.isSafeInteger(part.length) && part.length >= 0 && part.length <= maxPart(part.name), 'Frame part descriptor');
    names.add(part.name); length += part.length;
    if (part.name.startsWith('artifact/')) { native += part.length; artifacts++; }
  }
  check(length <= maximum && native <= 64 * 1024 * 1024 && artifacts <= 256, 'Frame byte limit');
  const references = new Set();
  function visit(value, depth = 0) {
    check(depth <= 64, 'Frame metadata depth');
    if (value === null || typeof value !== 'object') return;
    if (Array.isArray(value)) { for (const child of value) visit(child, depth + 1); return; }
    for (const [key, child] of Object.entries(value)) {
      if (key.endsWith('_part')) {
        const expected = ({ proof_part: 'proof', request_part: 'request', output_part: 'output', manifest_part: 'manifest' })[key] ??
          (key === 'bytes_part' && /^[0-9a-f]{64}$/.test(value.sha256) ? `artifact/${value.sha256}` : null);
        check(expected !== null && child === expected && names.has(child), 'Frame reference mismatch'); references.add(child);
      } else visit(child, depth + 1);
    }
  }
  visit(payload); check(references.size === names.size, 'Frame unused part'); return length;
}
export function encodeFrame(payload, parts = [], maximum = MAX_FRAME_REQUEST) {
  const descriptors = parts.map(part => { check(part?.bytes instanceof Uint8Array, 'Frame bytes required');
    return { name: part.name, length: part.bytes.byteLength }; });
  const header = encoder.encode(canonical({ payload, parts: descriptors }));
  check(header.length > 0 && header.length <= MAX_FRAME_HEADER, 'Frame header limit');
  const length = validate(payload, descriptors, maximum, header.length);
  const prefix = new Uint8Array(8); prefix.set([72, 71, 70, 49]); new DataView(prefix.buffer).setUint32(4, header.length);
  // The Container controller applies one native FixedLengthStream immediately before its
  // socket hop. Stacking two native fixed-length streams deadlocks workerd's optimized pump.
  let next = 0; const chunks = [prefix, header, ...parts.map(part => part.bytes)];
  const body = new ReadableStream({ pull(controller) {
    if (next === chunks.length) { chunks.length = 0; controller.close(); }
    else { controller.enqueue(chunks[next]); chunks[next++] = null; }
  }, cancel() { chunks.length = 0; } });
  return { body, length };
}
export async function decodeFrame(response, { maximum = MAX_FRAME_RESPONSE, streamOutput = false, signal } = {}) {
  check(response?.headers?.get('content-type') === FRAME_MIME && !response.headers.has('content-encoding') && response.body, 'Frame response type');
  const reader = response.body.getReader(); let pending = null, offset = 0, consumed = 0, closed = false;
  const close = () => { if (!closed) { closed = true; reader.releaseLock(); } };
  async function next() {
    const item = await abortable(reader.read(), signal);
    if (item.done) return null;
    check(item.value instanceof Uint8Array, 'Frame stream bytes'); return item.value;
  }
  async function exact(length) {
    const bytes = new Uint8Array(length); let at = 0;
    while (at < length) {
      if (!pending || offset === pending.length) { pending = await next(); offset = 0; check(pending, 'Truncated native frame'); }
      const take = Math.min(length - at, pending.length - offset);
      bytes.set(pending.subarray(offset, offset + take), at); offset += take; at += take; consumed += take;
      check(consumed <= maximum, 'Frame byte limit');
    }
    return bytes;
  }
  async function finish() {
    check(!pending || offset === pending.length, 'Trailing native frame bytes');
    while (true) { const tail = await next(); if (tail === null) break; check(tail.length === 0, 'Trailing native frame bytes'); }
    close();
  }
  async function fail(error) { await reader.cancel(error).catch(() => {}); close(); throw error; }
  try {
    const prefix = await exact(8); check(prefix[0] === 72 && prefix[1] === 71 && prefix[2] === 70 && prefix[3] === 49, 'Native frame magic');
    const headerLength = new DataView(prefix.buffer).getUint32(4); check(headerLength > 0 && headerLength <= MAX_FRAME_HEADER, 'Frame header limit');
    const headerBytes = await exact(headerLength), text = decoder.decode(headerBytes), header = JSON.parse(text);
    check(header && Object.keys(header).sort().join(',') === 'parts,payload' && canonical(header) === text, 'Noncanonical frame header');
    const length = validate(header.payload, header.parts, maximum, headerLength);
    const declared = response.headers.get('content-length');
    check(declared === null || /^(0|[1-9][0-9]*)$/.test(declared) && Number(declared) === length, 'Frame length differs');
    const parts = new Map();
    if (streamOutput && header.parts.some(part => part.name === 'output')) {
      check(header.parts.length === 1 && header.parts[0].name === 'output', 'Projection output must be isolated');
      let remaining = header.parts[0].length;
      const output = new ReadableStream({ async pull(controller) {
        try {
          if (remaining === 0) { await finish(); controller.close(); return; }
          if (!pending || offset === pending.length) { pending = await next(); offset = 0; check(pending, 'Truncated native output'); }
          const take = Math.min(remaining, pending.length - offset, 64 * 1024);
          const chunk = pending.subarray(offset, offset + take); offset += take; remaining -= take; consumed += take;
          controller.enqueue(chunk);
        } catch (error) { try { await fail(error); } catch (error) { controller.error(error); } }
      }, async cancel(reason) { await reader.cancel(reason).catch(() => {}); close(); pending = null; } }, { highWaterMark: 0 });
      return { payload: header.payload, parts, output };
    }
    for (const part of header.parts) parts.set(part.name, await exact(part.length));
    await finish(); return { payload: header.payload, parts, output: null };
  } catch (error) { return fail(error); }
}
