# `@heddleco/grpc`

Generated TypeScript protobuf and Connect client descriptors for Heddle's
hosted gRPC API.

This package is versioned with the Rust `heddle-grpc` crate. For example,
`@heddleco/grpc@0.7.1` is generated from `heddle-grpc@0.7.1`.

## Usage

```ts
import { createClient } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-web";
import { Sync } from "@heddleco/grpc";

const transport = createConnectTransport({ baseUrl: "https://api.example.com" });
const client = createClient(Sync.RepoSyncService, transport);
```

The root module exports each generated proto file as a namespace to avoid
cross-file symbol collisions:

```ts
import { Auth, Hosted, Sync } from "@heddleco/grpc";
```

Generated files are also available as subpath exports:

```ts
import { RepoSyncService } from "@heddleco/grpc/gen/heddle/v1/sync_pb";
```

## Regenerating

```sh
npm install
npm run generate
npm run typecheck
```

The generator reads the canonical proto tree from
`crates/grpc/proto/heddle/v1` and writes `src/gen/`.
