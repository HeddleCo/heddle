#!/usr/bin/env node

const size = Number(process.argv.at(-1) ?? 0);
process.stdout.write("x".repeat(size));
