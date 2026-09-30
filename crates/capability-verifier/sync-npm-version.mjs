// wasm-pack resolves Cargo workspace inheritance into the generated manifest.
import { readFileSync, writeFileSync } from "node:fs";

const generated = JSON.parse(readFileSync(new URL("./npm/dist/package.json", import.meta.url)));
const manifest = new URL("./npm/package.json", import.meta.url);
const packaged = JSON.parse(readFileSync(manifest));
packaged.version = generated.version;
writeFileSync(manifest, `${JSON.stringify(packaged, null, 2)}\n`);
console.log(`npm binding version: ${packaged.version}`);

// npm reads publish metadata before prepack; changing the file cannot refresh it.
if (process.argv.includes("--check-pack-version") &&
    process.env.npm_package_version !== generated.version) {
  throw new Error(
    `npm cached version ${process.env.npm_package_version}; Cargo version is ${generated.version}. ` +
    "The manifest is now synchronized; rerun npm pack or npm publish.",
  );
}
