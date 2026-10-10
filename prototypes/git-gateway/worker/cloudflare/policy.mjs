// SPDX-License-Identifier: Apache-2.0
// A bounded, explicitly configured MVP policy, not canonical hosted authority.
import { validateManifest } from '../catalog.mjs';
const OID = /^[0-9a-f]{40}$/;
const DIGEST = /^[0-9a-f]{64}$/;
const NAME = /^[a-z][a-z0-9-]{0,63}$/;
const THREAD = /^[a-z][a-z0-9-]{0,63}(?:\/[a-z][a-z0-9-]{0,63}){0,7}$/;
const TOKEN = /^[A-Za-z0-9_-]{32,256}$/;
const LIMIT = 64 * 1024;
const scope = m => [m.repository, m.source, m.thread, m.state, m.policy_epoch];
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
const assert = value => { if (!value) throw new Error('Gateway policy unavailable'); };
function keys(value, expected) {
  assert(value && typeof value === 'object' && !Array.isArray(value));
  assert(same(Object.keys(value).sort(), [...expected].sort()));
}
function list(value, validate, max = 1024) {
  assert(Array.isArray(value) && value.length <= max);
  for (const item of value) assert(validate(item));
  assert(new Set(value.map(item => JSON.stringify(item))).size === value.length);
}
function expiry(value) { assert(Number.isSafeInteger(value) && value >= 0); }
function grant(value, field, validate) {
  keys(value, ['sha256', 'expires_at', field]);
  assert(typeof value.sha256 === 'string' && DIGEST.test(value.sha256));
  expiry(value.expires_at); list(value[field], validate);
}
export function canonicalJson(value) {
  function ordered(v) {
    if (Array.isArray(v)) return v.map(ordered);
    if (v && typeof v === 'object') return Object.fromEntries(Object.keys(v).sort().map(k => [k, ordered(v[k])]));
    return v;
  }
  return JSON.stringify(ordered(value)) + '\n';
}
export function readPolicy(text) {
  assert(typeof text === 'string' && new TextEncoder().encode(text).length <= LIMIT);
  const p = JSON.parse(text);
  // Exact canonical bytes reject duplicate fields, alternate number spellings and
  // accidental ambiguity before this JSON can be used as an authority grant.
  assert(text === canonicalJson(p));
  keys(p, ['schema', 'issued_at', 'expires_at', 'catalog', 'pins', 'readers', 'gateway_service', 'bridge', 'sources']);
  assert(p.schema === 1 && typeof p.catalog === 'string' && NAME.test(p.catalog)); expiry(p.issued_at); expiry(p.expires_at);
  assert(p.expires_at > p.issued_at && p.expires_at - p.issued_at <= 900);
  assert(p.pins && typeof p.pins === 'object' && !Array.isArray(p.pins));
  const pins = Object.keys(p.pins); list(pins, pin => OID.test(pin), 2); assert(pins.length > 0);
  for (const pin of pins) validateManifest(canonicalJson(p.pins[pin]));
  assert(p.sources && typeof p.sources === 'object' && !Array.isArray(p.sources));
  list(Object.keys(p.sources), name => NAME.test(name), 1); assert(Object.keys(p.sources).length === 1);
  for (const source of Object.values(p.sources)) {
    keys(source, ['key', 'sha256', 'authorized_threads']);
    list(source.authorized_threads, thread => typeof thread === 'string' && thread.length <= 255 && THREAD.test(thread), 8);
    assert(source.authorized_threads.length > 0);
    assert(typeof source.sha256 === 'string' && DIGEST.test(source.sha256));
    // Content-addressed immutable test objects only. Callers never choose R2 keys.
    assert(source.key === `native/${source.sha256}.bundle`);
  }
  for (const m of Object.values(p.pins)) assert(Object.hasOwn(p.sources, m.source) && p.sources[m.source].authorized_threads.includes(m.thread));
  const scopes = Object.values(p.pins).map(scope);
  list(p.readers, reader => {
    grant(reader, 'views', view => scopes.some(known => same(known, view)));
    return true;
  });
  assert(p.readers.length === 1);
  grant(p.gateway_service, 'pins', pin => typeof pin === 'string' && pins.includes(pin));
  keys(p.bridge, ['expires_at', 'pins', 'sources']); expiry(p.bridge.expires_at);
  list(p.bridge.pins, pin => typeof pin === 'string' && pins.includes(pin));
  list(p.bridge.sources, source => typeof source === 'string' && Object.hasOwn(p.sources, source));
  for (const g of [p.readers[0], p.gateway_service, p.bridge]) assert(g.expires_at > p.issued_at && g.expires_at <= p.expires_at);
  // Each service has a separate hop identity; none is an independent reader grant.
  assert(p.gateway_service.sha256 !== p.readers[0].sha256);
  assert(scopes.every(view => p.readers[0].views.some(granted => same(view, granted))));
  assert(same([...p.gateway_service.pins].sort(), pins.sort()));
  assert(same([...p.bridge.pins].sort(), pins));
  assert(same([...p.bridge.sources].sort(), Object.keys(p.sources).sort()));
  return p;
}
export async function credentialDigest(bearer) {
  assert(typeof bearer === 'string' && bearer.startsWith('Bearer ') && TOKEN.test(bearer.slice(7)));
  const bytes = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(bearer.slice(7)));
  return Array.from(new Uint8Array(bytes), v => v.toString(16).padStart(2, '0')).join('');
}
function equalDigest(a, b) {
  let difference = a.length ^ b.length;
  for (let i = 0; i < 64; i++) difference |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return difference === 0;
}
function active(p, grant, now) {
  return Number.isFinite(now) && now >= p.issued_at && now < p.expires_at && now < grant.expires_at;
}
export async function readerAllowed(p, bearer, pin, now) {
  if (!Object.hasOwn(p.pins, pin)) return false;
  const digest = await credentialDigest(bearer);
  const reader = p.readers.find(r => equalDigest(r.sha256, digest));
  return Boolean(reader && active(p, reader, now) && reader.views.some(v => same(v, scope(p.pins[pin]))));
}
export async function serviceAllowed(p, bearer, kind, target, now) {
  const digest = await credentialDigest(bearer);
  const grant = kind === 'gateway' ? p.gateway_service : null;
  if (!grant || !active(p, grant, now) || !equalDigest(grant.sha256, digest)) return false;
  return grant.pins.includes(target);
}
export const manifestScope = scope;
