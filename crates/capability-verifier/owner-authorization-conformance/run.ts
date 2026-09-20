import { execFileSync } from "node:child_process";
import { isDeepStrictEqual } from "node:util";
import {
  mkdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

type FixtureKind = "purge" | "transfer" | "keyring";

type CorpusCase = {
  id: string;
  fixture_kind: FixtureKind;
  fixture_json: string;
};

type Outcome = {
  id: string;
  ok?: unknown;
  error?: string;
};

type JsonRecord = Record<string, unknown>;

const DEFAULT_SEED = 0x248c0de;
const FUZZ_CASE_COUNT = Number(process.env.OWNER_AUTH_FUZZ_CASE_COUNT ?? "24");
const seed = Number(process.env.OWNER_AUTH_CASE_SEED ?? DEFAULT_SEED);
const nativeVerifier = process.env.OWNER_AUTH_NATIVE_VERIFIER;
const scratch = process.env.OWNER_AUTH_SCRATCH;
const sourceDirectory = path.dirname(fileURLToPath(import.meta.url));
const repositoryRoot = path.resolve(sourceDirectory, "..");
const bindingRoot = process.env.OWNER_AUTH_BINDING_ROOT ??
  path.join(repositoryRoot, "npm", "dist");

if (!nativeVerifier || !scratch) {
  throw new Error(
    "OWNER_AUTH_NATIVE_VERIFIER and OWNER_AUTH_SCRATCH are required",
  );
}
if (!Number.isSafeInteger(seed) || !Number.isSafeInteger(FUZZ_CASE_COUNT) ||
    FUZZ_CASE_COUNT < 0) {
  throw new Error("seed and fuzz case count must be non-negative safe integers");
}

class Random {
  private state: number;

  constructor(state: number) {
    this.state = state;
  }

  next(): number {
    let value = (this.state += 0x6d2b79f5);
    value = Math.imul(value ^ (value >>> 15), value | 1);
    value ^= value + Math.imul(value ^ (value >>> 7), value | 61);
    return ((value ^ (value >>> 14)) >>> 0) / 0x1_0000_0000;
  }

  int(ceiling: number): number {
    return Math.floor(this.next() * ceiling);
  }
}

const fixtureDefinitions: Array<{
  kind: FixtureKind;
  filename: string;
}> = [
  { kind: "purge", filename: "v2.json" },
  { kind: "transfer", filename: "transfer-v2.json" },
  { kind: "keyring", filename: "keyring-v2.json" },
];

function cloneJson(value: unknown): unknown {
  return JSON.parse(JSON.stringify(value));
}

function asRecord(value: unknown, label: string): JsonRecord {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(`${label} is not a JSON object`);
  }
  return value as JsonRecord;
}

function hexTargets(value: unknown): Array<{ owner: JsonRecord; key: string }> {
  const targets: Array<{ owner: JsonRecord; key: string }> = [];
  if (Array.isArray(value)) {
    for (const item of value) targets.push(...hexTargets(item));
    return targets;
  }
  if (typeof value !== "object" || value === null) return targets;
  const record = value as JsonRecord;
  for (const [key, child] of Object.entries(record)) {
    if (key.endsWith("_hex") && typeof child === "string" && child.length >= 2) {
      targets.push({ owner: record, key });
    } else {
      targets.push(...hexTargets(child));
    }
  }
  return targets;
}

function mutateHex(value: string, operation: number, random: Random): string {
  const evenLength = value.length - (value.length % 2);
  switch (operation % 6) {
    case 0: {
      const index = random.int(value.length);
      const replacement = value[index] === "0" ? "1" : "0";
      return `${value.slice(0, index)}${replacement}${value.slice(index + 1)}`;
    }
    case 1:
      return value.slice(0, Math.max(0, evenLength - 2));
    case 2:
      return `${value}00`;
    case 3: {
      const index = random.int(value.length);
      return `${value.slice(0, index)}g${value.slice(index + 1)}`;
    }
    case 4:
      return `${value.slice(2)}${value.slice(0, 2)}`;
    default:
      return value.toUpperCase();
  }
}

function mutatedFixture(
  fixture: unknown,
  mutation: number,
  random: Random,
): unknown {
  const copy = asRecord(cloneJson(fixture), "fixture");
  const cases = copy.cases;
  if (!Array.isArray(cases) || cases.length === 0) {
    throw new Error("fixture has no cases");
  }
  const selected = cases[random.int(cases.length)];
  const targets = hexTargets(selected);
  if (targets.length === 0) throw new Error("fixture case has no hex evidence");
  const target = targets[random.int(targets.length)];
  const current = target.owner[target.key];
  if (typeof current !== "string") throw new Error("hex target is not a string");
  target.owner[target.key] = mutateHex(current, mutation, random);
  return copy;
}

const random = new Random(seed);
const corpusCases: CorpusCase[] = [];
for (const definition of fixtureDefinitions) {
  const fixturePath = path.join(
    repositoryRoot,
    "conformance",
    "fixtures",
    definition.filename,
  );
  const source = JSON.parse(readFileSync(fixturePath, "utf8"));
  corpusCases.push({
    id: `base-${definition.kind}`,
    fixture_kind: definition.kind,
    fixture_json: JSON.stringify(source),
  });
  for (let mutation = 0; mutation < FUZZ_CASE_COUNT; mutation += 1) {
    corpusCases.push({
      id: `${definition.kind}-seed-${seed}-mutation-${mutation}`,
      fixture_kind: definition.kind,
      fixture_json: JSON.stringify(mutatedFixture(source, mutation, random)),
    });
  }
}

mkdirSync(scratch, { recursive: true });
const corpusPath = path.join(scratch, `owner-authorization-corpus-${seed}.json`);
writeFileSync(corpusPath, JSON.stringify({ seed, cases: corpusCases }));

const nativeOutcomes = JSON.parse(
  execFileSync(nativeVerifier, [corpusPath], { encoding: "utf8" }),
) as Outcome[];
const wasm = await import(pathToFileURL(
  path.join(bindingRoot, "capability_verifier.js"),
).href);
const wasmBytes = readFileSync(
  path.join(bindingRoot, "capability_verifier_bg.wasm"),
);
wasm.initSync({ module: wasmBytes });
const packageMetadata = JSON.parse(
  readFileSync(path.join(repositoryRoot, "npm", "package.json"), "utf8"),
);
if (wasm.verifierVersion() !== packageMetadata.version) {
  throw new Error(
    `binding version ${wasm.verifierVersion()} != package version ${packageMetadata.version}`,
  );
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  return String(error);
}

function evaluateWasm(testCase: CorpusCase): Outcome {
  const runner = testCase.fixture_kind === "purge"
    ? wasm.runPurgeFixture
    : testCase.fixture_kind === "transfer"
    ? wasm.runTransferFixture
    : wasm.runKeyringFixture;
  try {
    return { id: testCase.id, ok: JSON.parse(runner(testCase.fixture_json)) };
  } catch (error) {
    return { id: testCase.id, error: errorMessage(error) };
  }
}

const wasmOutcomes = corpusCases.map(evaluateWasm);
if (process.env.OWNER_AUTH_FORCE_DIVERGENCE === "1") {
  wasmOutcomes[0] = {
    id: wasmOutcomes[0].id,
    error: "injected differential self-test mismatch",
  };
}

const nativeById = new Map(nativeOutcomes.map((outcome) => [outcome.id, outcome]));
const divergences: string[] = [];

const purgeFixtureCase = corpusCases.find((testCase) => testCase.id === "base-purge");
if (!purgeFixtureCase) throw new Error("base purge fixture is missing");
const purgeFixture = asRecord(
  JSON.parse(purgeFixtureCase.fixture_json),
  "purge fixture",
);
const maxTtl = purgeFixture.max_capability_ttl_seconds;
if (typeof maxTtl !== "number" || !Array.isArray(purgeFixture.cases)) {
  throw new Error("purge fixture limits or cases are invalid");
}
for (const value of purgeFixture.cases) {
  const testCase = asRecord(value, "purge fixture case");
  const bytes = (field: string): Uint8Array => {
    const hex = testCase[field];
    if (typeof hex !== "string") throw new Error(`${field} is not hex`);
    return new Uint8Array(Buffer.from(hex, "hex"));
  };
  const pathSegments = testCase.spool_path_segments;
  const now = testCase.now_unix_seconds;
  if (!Array.isArray(pathSegments) ||
      !pathSegments.every((segment) => typeof segment === "string") ||
      typeof now !== "number") {
    throw new Error("purge fixture path or time is invalid");
  }
  const actual = JSON.parse(wasm.verifyPurgeAuthorization(
    bytes("authorization_hex"),
    bytes("operation_body_hex"),
    bytes("payload_hex"),
    bytes("owner_genesis_hex"),
    bytes("current_owner_state_hash_hex"),
    bytes("spool_uuid_hex"),
    pathSegments,
    BigInt(now),
    BigInt(maxTtl),
  ));
  if (!isDeepStrictEqual(actual, testCase.expected)) {
    divergences.push(
      `direct-binding-${String(testCase.name)}: expected=${JSON.stringify(testCase.expected)} actual=${JSON.stringify(actual)}`,
    );
  }
}

for (const wasmOutcome of wasmOutcomes) {
  const nativeOutcome = nativeById.get(wasmOutcome.id);
  if (!nativeOutcome || !isDeepStrictEqual(nativeOutcome, wasmOutcome)) {
    divergences.push(
      `${wasmOutcome.id}: native=${JSON.stringify(nativeOutcome)} wasm=${JSON.stringify(wasmOutcome)}`,
    );
  }
}
if (nativeOutcomes.length !== corpusCases.length) {
  divergences.push(
    `native result count ${nativeOutcomes.length} != corpus count ${corpusCases.length}`,
  );
}

for (const definition of fixtureDefinitions) {
  const base = nativeById.get(`base-${definition.kind}`);
  if (!Array.isArray(base?.ok) ||
      !base.ok.every((outcome) => asRecord(outcome, "base outcome").matches === true)) {
    divergences.push(`base-${definition.kind}: checked-in fixture did not match expectations`);
  }
}

if (divergences.length > 0) {
  console.error(
    `OWNER_AUTH_DIFFERENTIAL_DIVERGENCE=DETECTED seed=${seed} count=${divergences.length}`,
  );
  throw new Error(divergences.join("\n"));
}

console.log(
  `OWNER_AUTH_DIFFERENTIAL=PASS seed=${seed} fuzz_cases_per_fixture=${FUZZ_CASE_COUNT} corpus_cases=${corpusCases.length}`,
);
