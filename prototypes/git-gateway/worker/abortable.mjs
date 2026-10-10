// SPDX-License-Identifier: Apache-2.0
// Some RPC/test implementations ignore AbortSignal. Stop waiting anyway, and
// release any resource returned after cancellation instead of abandoning it.
export function abortable(promise, signal, disposeLate = () => {}) {
  if (!signal) return Promise.resolve(promise);
  return new Promise((resolve, reject) => {
    let settled = false, aborted = false;
    const cleanup = () => signal.removeEventListener('abort', onAbort);
    const onAbort = () => {
      if (settled) return;
      settled = true; aborted = true; cleanup(); reject(signal.reason);
    };
    signal.addEventListener('abort', onAbort, { once: true });
    if (signal.aborted) onAbort();
    Promise.resolve(promise).then(value => {
      if (aborted) {
        try { disposeLate(value); } catch { /* Best-effort release after abort. */ }
        return;
      }
      settled = true; cleanup(); resolve(value);
    }, error => {
      if (settled) return;
      settled = true; cleanup(); reject(error);
    });
  });
}
