#!/usr/bin/env node
// Emits its own pid as the first line, then a line every 25ms forever.
// SIGTERM-able. Used to prove early generator close kills the child.
console.log(String(process.pid));
setInterval(() => console.log("tick"), 25);
