// SPDX-License-Identifier: Apache-2.0
import { Container } from '@cloudflare/containers';
import { hostedContainerFetch } from './hosted-container-runtime.mjs';
// Separate code-review gates. Neither can be enabled by a request or environment setting.
export const HOSTED_CONTAINER_ACTIVATION_REVIEWED = false;
export const HOSTED_NATIVE_NETWORK_REVIEWED = false;
export class HostedNativeGateway extends Container {
  defaultPort = 8080;
  sleepAfter = '30s';
  enableInternet = false;
  interceptHttps = false;
  gatewayState = { busy: false };
  fetch(request) {
    return hostedContainerFetch(request, {
      id: this.ctx.id.toString(), running: this.ctx.container.running,
      startAndWaitForPorts: args => this.startAndWaitForPorts(args),
      containerFetch: (req, port) => this.containerFetch(req, port),
    }, this.env, this.gatewayState, {
      activation: HOSTED_CONTAINER_ACTIVATION_REVIEWED, nativeNetwork: HOSTED_NATIVE_NETWORK_REVIEWED,
    });
  }
}
// There is deliberately no misleading allowedHosts policy here. Once reviewed enableInternet
// is true for Iroh, arbitrary QUIC does not pass through HTTP outbound interception. The native
// process checks pinned endpoint/descriptor and exact configured Artifacts HTTPS destination.
