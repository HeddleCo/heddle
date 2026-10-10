// SPDX-License-Identifier: Apache-2.0
import test from 'node:test';
import assert from 'node:assert/strict';
import { FRAME_MIME, MAX_FRAME_HEADER, encodeFrame, decodeFrame } from './native-frame.mjs';
import { canonical } from './publication.mjs';
const bytes = value => new TextEncoder().encode(value);
const response = frame => new Response(frame.body, { headers: { 'content-type': FRAME_MIME, 'content-length': String(frame.length) } });
const raw = (header, tail = new Uint8Array()) => {
  const text = typeof header === 'string' ? header : canonical(header), body = bytes(text), result = new Uint8Array(8 + body.length + tail.length);
  result.set([72,71,70,49]); new DataView(result.buffer).setUint32(4, body.length); result.set(body, 8); result.set(tail, 8 + body.length); return result;
};
const rawResponse = value => new Response(value, { headers: { 'content-type': FRAME_MIME } });
test('native frame roundtrips exact binary parts without encoding expansion and allows shared references', async () => {
  const name = 'artifact/' + 'a'.repeat(64), source = Uint8Array.from({ length: 50003 }, (_, i) => i % 256);
  const payload = { proof_part: 'proof', artifacts: [{ sha256: 'a'.repeat(64), bytes_part: name }, { sha256: 'a'.repeat(64), bytes_part: name }] };
  const frame = encodeFrame(payload, [{ name: 'proof', bytes: bytes('é☃') }, { name, bytes: source }]);
  assert.ok(frame.length < source.length + 1000);
  const result = await decodeFrame(response(frame)); assert.deepEqual(result.payload, payload);
  assert.deepEqual(result.parts.get(name), source); assert.deepEqual(result.parts.get('proof'), bytes('é☃'));
});
test('frame rejects duplicate unused cross-artifact unknown markers and noncanonical headers', async () => {
  const data = bytes('x'), descriptor = { name: 'proof', length: 1 };
  for (const header of [
    { payload: {}, parts: [descriptor] },
    { payload: { proof_part: 'proof' }, parts: [descriptor, descriptor] },
    { payload: { future_part: 'proof' }, parts: [descriptor] },
    { payload: { proof_part: 'request' }, parts: [{ name: 'request', length: 1 }] },
    { payload: { artifacts: [{ sha256: 'b'.repeat(64), bytes_part: 'artifact/' + 'a'.repeat(64) }] }, parts: [{ name: 'artifact/' + 'a'.repeat(64), length: 1 }] },
    '{"parts":[],"payload":{}}',
    { payload: {}, parts: [], extra: true },
  ]) await assert.rejects(decodeFrame(rawResponse(raw(header, data))));
  assert.throws(() => encodeFrame({}, [{ name: 'proof', bytes: data }]));
});
test('frame enforces header native total and exact body length before completion', async () => {
  const huge = new Uint8Array(8); huge.set([72,71,70,49]); new DataView(huge.buffer).setUint32(4, MAX_FRAME_HEADER + 1);
  await assert.rejects(decodeFrame(rawResponse(huge)), /header limit/);
  await assert.rejects(decodeFrame(rawResponse(raw({ payload: { proof_part: 'proof' }, parts: [{ name: 'proof', length: 2 }] }, bytes('x')))), /Truncated/);
  await assert.rejects(decodeFrame(rawResponse(raw({ payload: {}, parts: [] }, bytes('extra')))), /Trailing/);
  const frame = encodeFrame({}); await assert.rejects(decodeFrame(new Response(frame.body, { headers: { 'content-type': FRAME_MIME, 'content-length': String(frame.length + 1) } })), /length differs/);
  const bad = raw({ payload: {}, parts: [] }); bad[0] = 0; await assert.rejects(decodeFrame(rawResponse(bad)), /magic/);
});
test('projection output streams with bounded chunks, completion checks and cancellation', async () => {
  const source = new Uint8Array(150000).fill(7), frame = encodeFrame({ output_part: 'output', status: 200 }, [{ name: 'output', bytes: source }]);
  const result = await decodeFrame(response(frame), { streamOutput: true });
  assert.equal(result.parts.size, 0); const reader = result.output.getReader(); let total = 0;
  for (;;) { const { done, value } = await reader.read(); if (done) break; assert.ok(value.length <= 65536); total += value.length; }
  assert.equal(total, source.length);
  const trailing = await decodeFrame(rawResponse(raw({ payload: { output_part: 'output' }, parts: [{ name: 'output', length: 1 }] }, bytes('xx'))), { streamOutput: true });
  await assert.rejects(new Response(trailing.output).arrayBuffer(), /Trailing/);
  let cancelled = false;
  const initial = raw({ payload: { output_part: 'output' }, parts: [{ name: 'output', length: 100 }] });
  const stream = new ReadableStream({ start(controller) { controller.enqueue(initial); }, cancel() { cancelled = true; } });
  const pending = await decodeFrame(new Response(stream, { headers: { 'content-type': FRAME_MIME } }), { streamOutput: true });
  await pending.output.cancel(); assert.equal(cancelled, true);
});
