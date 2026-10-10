// SPDX-License-Identifier: Apache-2.0
// Independent wire tests. Raw malformed frames are built without the production encoder.
// The small golden vector is shared with the Rust native_frame unit tests.
import test from 'node:test';
import assert from 'node:assert/strict';
import { canonical } from './publication.mjs';
import { FRAME_MIME, MAX_FRAME_HEADER, encodeFrame, decodeFrame } from './native-frame.mjs';

const encoder = new TextEncoder();
const GOLDEN = '48474631000000627b227061727473223a5b7b226c656e677468223a342c226e616d65223a2270726f6f66227d5d2c227061796c6f6164223a7b226d6574686f64223a2276616c69646174652d706c616e222c2270726f6f665f70617274223a2270726f6f66227d7d0a00ff0180';
const proof = new Uint8Array([0, 255, 1, 128]);
const payload = { method: 'validate-plan', proof_part: 'proof' };
function rawFrame(payload, parts = [], body = new Uint8Array(), headerText) {
  const header = typeof headerText === 'string' ? encoder.encode(headerText) :
    headerText ?? encoder.encode(canonical({ payload, parts }));
  const raw = new Uint8Array(8 + header.length + body.length);
  raw.set([72, 71, 70, 49]); new DataView(raw.buffer).setUint32(4, header.length, false);
  raw.set(header, 8); raw.set(body, 8 + header.length); return raw;
}
function response(raw, { split = raw.length || 1, headers = {} } = {}) {
  let at = 0;
  return new Response(new ReadableStream({ pull(controller) {
    if (at === raw.length) { controller.close(); return; }
    const end = Math.min(at + split, raw.length); controller.enqueue(raw.subarray(at, end)); at = end;
  } }), { headers: { 'content-type': FRAME_MIME, ...headers } });
}
const deny = (raw, pattern) => assert.rejects(decodeFrame(response(raw)), pattern);

test('HGF1 matches the shared Rust golden bytes and preserves binary values at every fragment boundary', async () => {
  const frame = encodeFrame(payload, [{ name: 'proof', bytes: proof }]);
  const raw = new Uint8Array(await new Response(frame.body).arrayBuffer());
  assert.equal(frame.length, 110); assert.equal(Buffer.from(raw).toString('hex'), GOLDEN);
  assert.equal(new DataView(raw.buffer).getUint32(4, false), 98);
  for (const split of [1, 2, 3, 7, 8, 17, 98, 110]) {
    const decoded = await decodeFrame(response(Buffer.from(GOLDEN, 'hex'), { split }));
    assert.deepEqual(decoded.payload, payload); assert.deepEqual(decoded.parts.get('proof'), proof);
    assert.equal(decoded.output, null);
  }
});

test('frame references allow explicit aliases but reject duplicate names, unknown markers, and unused parts', async () => {
  const aliases = { proof_part: 'proof', nested: { proof_part: 'proof' } };
  assert.deepEqual((await decodeFrame(response(rawFrame(aliases, [{ name: 'proof', length: 4 }], proof)))).parts.get('proof'), proof);
  for (const [value, parts] of [
    [payload, [{ name: 'proof', length: 0 }, { name: 'proof', length: 0 }]],
    [{ proof_part: 'request' }, [{ name: 'request', length: 0 }]],
    [{ custom_part: 'proof' }, [{ name: 'proof', length: 0 }]],
    [{}, [{ name: 'proof', length: 0 }]],
    [{ proof_part: 'proof' }, []],
    [{ artifacts: [{ sha256: 'a'.repeat(64), bytes_part: 'artifact/' + 'b'.repeat(64) }] },
      [{ name: 'artifact/' + 'b'.repeat(64), length: 0 }]],
  ]) await deny(rawFrame(value, parts), /descriptor|reference|unused/);
});

test('frame headers reject malformed UTF-8, noncanonical JSON, duplicate keys, and extra envelope keys', async () => {
  for (const text of [
    '{"payload":{},"parts":[]}\n', '{"parts":[], "payload":{}}\n', '{"parts":[],"payload":{}}',
    '{"parts":[],"payload":{},"payload":{}}\n', '{"extra":false,"parts":[],"payload":{}}\n',
  ]) await deny(rawFrame({}, [], new Uint8Array(), text), /canonical/);
  const malformed = new Uint8Array([...encoder.encode('{"parts":[],"payload":{"label":"'), 0xc3, 0x28,
    ...encoder.encode('"}}\n')]);
  await deny(rawFrame({}, [], new Uint8Array(), malformed), /encoding|encoded data/i);
});

test('frame prefix bounds are enforced before allocating or reading oversized metadata', async () => {
  for (const length of [0, MAX_FRAME_HEADER + 1, 0xffffffff]) {
    const raw = new Uint8Array([72, 71, 70, 49, 0, 0, 0, 0]);
    new DataView(raw.buffer).setUint32(4, length); await deny(raw, /header limit/);
  }
  const bad = Buffer.from(GOLDEN, 'hex'); bad[0] ^= 1; await deny(bad, /magic/);
});

test('part lengths and aggregate source limits reject overflow, unsafe integers, and out-of-domain sizes', async () => {
  for (const length of [-1, 0.5, Number.MAX_SAFE_INTEGER + 1, 17 * 1024 * 1024 + 1])
    await deny(rawFrame(payload, [{ name: 'proof', length }]), /descriptor/);
  for (const [name, limit] of [['request', 17 * 1024 * 1024], ['output', 96 * 1024 * 1024], ['manifest', 64 * 1024]])
    await deny(rawFrame({ [`${name}_part`]: name }, [{ name, length: limit + 1 }]), /descriptor/);
  await deny(rawFrame({}, [{ name: 'unknown', length: 0 }]), /descriptor/);
  const names = ['a', 'b'].map(value => value.repeat(64));
  await deny(rawFrame({ artifacts: names.map(sha256 => ({ sha256, bytes_part: `artifact/${sha256}` })) },
    names.map(name => ({ name: `artifact/${name}`, length: 40 * 1024 * 1024 }))), /byte limit/);
  await assert.rejects(decodeFrame(response(Buffer.from(GOLDEN, 'hex')), { maximum: 109 }), /byte limit/);
});

test('total descriptor and artifact counts are independently bounded', async () => {
  const artifacts = Array.from({ length: 257 }, (_, index) => {
    const sha256 = index.toString(16).padStart(64, '0'); return { sha256, bytes_part: `artifact/${sha256}` };
  });
  const parts = artifacts.map(artifact => ({ name: artifact.bytes_part, length: 0 }));
  await deny(rawFrame({ artifacts }, parts), /count|byte limit/);
  const names = ['proof', 'request', 'output', 'manifest'];
  const atLimit = await decodeFrame(response(rawFrame({ artifacts: artifacts.slice(0, 256),
    ...Object.fromEntries(names.map(name => [`${name}_part`, name])) },
  [...parts.slice(0, 256), ...names.map(name => ({ name, length: 0 }))])));
  assert.equal(atLimit.parts.size, 260);
  const tooMany = Array.from({ length: 261 }, () => ({ name: 'proof', length: 0 }));
  await deny(rawFrame(payload, tooMany), /count/);
});

test('every truncated golden frame and trailing frame byte is rejected', async () => {
  const raw = Buffer.from(GOLDEN, 'hex');
  for (let cut = 0; cut < raw.length; cut++) await deny(raw.subarray(0, cut), /Truncated/);
  for (const suffix of [new Uint8Array([0]), new Uint8Array([255]), raw])
    await deny(new Uint8Array([...raw, ...suffix]), /Trailing/);
});

test('declared HTTP length, content type, and encoding cannot override frame boundaries', async () => {
  for (const value of ['109', '111', '-1', '0110', '1e2', '9007199254740992'])
    await assert.rejects(decodeFrame(response(Buffer.from(GOLDEN, 'hex'), { headers: { 'content-length': value } })), /length differs/);
  for (const headers of [{ 'content-type': 'application/json' }, { 'content-encoding': 'gzip' }])
    await assert.rejects(decodeFrame(response(Buffer.from(GOLDEN, 'hex'), { headers })), /response type/);
});

test('streaming output must be isolated and still checks binary bytes, truncation, and trailing input', async () => {
  const raw = rawFrame({ status: 200, output_part: 'output' }, [{ name: 'output', length: proof.length }], proof);
  const valid = await decodeFrame(response(raw, { split: 1 }), { streamOutput: true });
  assert.equal(valid.parts.size, 0); assert.deepEqual(new Uint8Array(await new Response(valid.output).arrayBuffer()), proof);
  for (const invalid of [raw.subarray(0, raw.length - 1), new Uint8Array([...raw, 0])]) {
    const decoded = await decodeFrame(response(invalid), { streamOutput: true });
    await assert.rejects(new Response(decoded.output).arrayBuffer(), /Truncated|Trailing/);
  }
  await assert.rejects(decodeFrame(response(rawFrame({ output_part: 'output', proof_part: 'proof' },
    [{ name: 'output', length: 0 }, { name: 'proof', length: 0 }])), { streamOutput: true }), /isolated/);
});
