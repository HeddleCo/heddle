// Build only from the locked Rust graph; wasm-bindgen is pinned in Cargo.toml.
import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const root = fileURLToPath(new URL(".", import.meta.url));
function run(command, args, capture = false) {
  const result = spawnSync(command, args, { cwd: root, encoding: "utf8", stdio: capture ? "pipe" : "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} failed (${result.status}): ${result.stderr ?? ""}`);
  return result.stdout;
}
if (run("wasm-bindgen", ["--version"], true).trim() !== "wasm-bindgen 0.2.127") {
  throw new Error("Install wasm-bindgen-cli 0.2.127 with cargo install --locked.");
}
const metadata = JSON.parse(run("cargo", ["metadata", "--locked", "--no-deps", "--format-version", "1"], true));
const crate = metadata.packages.find(p => p.name === "heddleco-capability-verifier");
const dist = path.join(root, "npm", "dist");
mkdirSync(dist, { recursive: true });
run("cargo", ["build", "--locked", "--release", "-p", crate.name, "--target", "wasm32-unknown-unknown", "--lib"]);
run("wasm-bindgen", [path.join(metadata.target_directory, "wasm32-unknown-unknown", "release", "heddleco_capability_verifier.wasm"), "--target", "web", "--out-dir", dist, "--out-name", "capability_verifier"]);
for (const license of ["LICENSE-APACHE", "LICENSE-MIT"]) copyFileSync(path.join(root, license), path.join(dist, license));
writeFileSync(path.join(dist, "package.json"), `${JSON.stringify({ name: crate.name, version: crate.version, type: "module" }, null, 2)}\n`);
await import("./sync-npm-version.mjs");
