# CLI trace export

Heddle can export command traces to an OpenTelemetry collector. The default
CLI build does not include the OTLP exporter. Build it explicitly with:

```sh
cargo build -p heddle-cli --features telemetry
```

In a telemetry build, export remains off at runtime unless a trace endpoint is
configured. Without one, Heddle does not construct a tracer provider, exporter,
or telemetry network client.

Then opt in for one shell with the collector's OTLP/HTTP base endpoint:

```sh
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318
heddle status
```

`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` takes precedence when both variables are
set and, per the OTLP convention for signal-specific endpoints, should include
the `/v1/traces` path. A persistent user configuration uses the same complete
trace URL:

```toml
[logging]
otel_traces_endpoint = "http://127.0.0.1:4318/v1/traces"
```

For the pilot observability stack under `workspace/obs`, set the endpoint to
`http://<collector-host>:4318`; use `127.0.0.1` when Heddle runs on the same
host as the collector.

Each traced invocation emits a `heddle.command` span with the static command
name, success/error status, numeric exit code, and duration. Existing
`HEDDLE_PROFILE` timings are represented as `heddle.phase` child spans where
the command already measures them. Heddle does not attach paths, argv, object
IDs, remote URLs, environment values, or filenames. Hosted request context
propagates W3C `traceparent` and `tracestate`; baggage is not collected or sent.

Trace export is batched on a background worker. Process exit attempts a
best-effort flush bounded to 250 milliseconds, so an unavailable collector
cannot hold command completion indefinitely.

The client-side spans and export are useful independently. End-to-end traces
also require Weft to extract the propagated context from
`CallContext.trace.traceparent` and `CallContext.trace.tracestate` in the Iroh
request opening prelude and use it as the parent of its RPC lifecycle span.
