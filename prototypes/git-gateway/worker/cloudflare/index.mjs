// SPDX-License-Identifier: Apache-2.0
import { ACTIVATION_REVIEWED } from './controller.mjs';
import { preparedEdgeFetch } from './composition.mjs';
export { NativeGatewayContainer, ContainerProxy } from './controller.mjs';
export default {
  async fetch(request, env) {
    if (!ACTIVATION_REVIEWED) return new Response('Cloudflare runtime preparation is disabled', {
      status: 503, headers: { 'cache-control': 'no-store' },
    });
    return preparedEdgeFetch(request, env);
  },
};
