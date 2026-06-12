#!/usr/bin/env node
// Prints the argv it was invoked with — lets tests assert SpawnExecutor's
// real argv construction without building the heddle binary.
console.log(JSON.stringify(process.argv.slice(2)));
