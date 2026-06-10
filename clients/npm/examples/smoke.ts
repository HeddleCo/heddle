/**
 * Runnable example: drive a real heddle binary and print a typed payload.
 *
 *   HEDDLE_BIN=./path/to/heddle node dist/examples/smoke.js
 *   # or, with heddle on PATH:
 *   npm run example
 */
import { Heddle, HeddleError } from "../src/index.js";

async function main(): Promise<void> {
  const heddle = new Heddle({
    binaryPath: process.env["HEDDLE_BIN"] ?? "heddle",
    repoPath: process.argv[2],
  });

  try {
    // `status` is read-only; its payload is `StatusSchema`-typed.
    const status = await heddle.status();
    console.log("status output_kind:", status.output_kind);
    console.log(JSON.stringify(status, null, 2));
  } catch (err) {
    if (err instanceof HeddleError) {
      console.error(
        `heddle ${err.verb} failed (exit ${err.exitCode}, code=${err.code}, retryable=${err.retryable})`,
      );
      console.error(err.message);
      process.exitCode = err.exitCode;
      return;
    }
    throw err;
  }
}

void main();
