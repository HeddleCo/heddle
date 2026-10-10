// SPDX-License-Identifier: Apache-2.0
import { Container } from '@cloudflare/containers';
import { preparedContainerFetch } from './controller-runtime.mjs';
import { preparedBridgeFetch } from './composition.mjs';
export { ContainerProxy } from '@cloudflare/containers';

// This is a code-review gate, not a remotely configurable environment toggle.
export const ACTIVATION_REVIEWED = false;
const disabled = () => new Response('Cloudflare runtime preparation is disabled', { status: 503 });
export class NativeGatewayContainer extends Container {
  defaultPort = 8080;
  sleepAfter = '30s';
  enableInternet = false;
  interceptHttps = false;
  gatewayState = { busy: false };
  async fetch(request) {
    if (!ACTIVATION_REVIEWED) return disabled();
    return preparedContainerFetch(request, {
      id: this.ctx.id.toString(), running: this.ctx.container.running,
      startAndWaitForPorts: args => this.startAndWaitForPorts(args),
      containerFetch: (req, port) => this.containerFetch(req, port),
    }, this.env, this.gatewayState);
  }
}
NativeGatewayContainer.outboundByHost = {
  'gateway-bindings.internal': (request, env, ctx) => ACTIVATION_REVIEWED ? preparedBridgeFetch(request, env, ctx) : disabled(),
};
// No allowedHosts or catch-all handler: unlisted hosts cannot fall back to public
// internet. The virtual fixed-host bridge uses Cloudflare-managed encrypted HTTP.
