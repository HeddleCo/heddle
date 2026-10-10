// SPDX-License-Identifier: Apache-2.0
// Complete configured HTTP header; the Rust listener stores SHA-256 of its token only.
export function nativeServiceSecret(value) {
  const match = typeof value === 'string' && /^Bearer ([!-~]{32,256})$/.exec(value);
  if (!match) throw new Error('Native service requires Bearer plus a 32..256 byte ASCII token');
  return match[1];
}
