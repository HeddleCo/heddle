// SPDX-License-Identifier: Apache-2.0
import test from 'node:test';
import assert from 'node:assert/strict';
import { nativeServiceSecret } from './native-service.mjs';
test('native service configuration is an explicit full Bearer header with the Rust byte grammar', () => {
  for (const size of [32,64,256]) assert.equal(nativeServiceSecret('Bearer ' + 'S'.repeat(size)), 'S'.repeat(size));
  for (const value of [undefined, '', 'S'.repeat(64), 'bearer ' + 'S'.repeat(64), 'Bearer ' + 'S'.repeat(31),
    'Bearer ' + 'S'.repeat(257), 'Bearer ' + 'é'.repeat(32), 'Bearer ' + 'S'.repeat(32) + ' ',
    'Bearer ' + 'S'.repeat(32) + '\t', 'Bearer ' + 'S'.repeat(32) + '\n']) assert.throws(() => nativeServiceSecret(value));
});
