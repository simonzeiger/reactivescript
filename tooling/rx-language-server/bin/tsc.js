#!/usr/bin/env node
// `rx-tsc`: TypeScript's own `tsc`, with `.rx` modules in the program (spec/13 TY-1a), for CI and the command line. It
// is Volar's tsc runner (the one `vue-tsc` uses) with the language server's `.rx` plugin, so the editor and the command
// line check the same projection. It takes tsc's arguments (`rx-tsc --noEmit`, `-p`, `--watch`) and reads the
// tsconfig.json's `include`, which must name the `.rx` files. Type errors are placed on the `.rx`.
//
// The crate's own diagnostics (parse errors, else the checker's, with their rules) are reported after TypeScript's, for
// every `.rx` module in the program: a module that does not parse has no projection, so TypeScript alone would pass it.
const path = require("node:path");
const { runTsc } = require("@volar/typescript/lib/quickstart/runTsc");
const { rxLanguagePlugin, analyses, guideUrl } = require("../src/index.js");

const rawArgs = process.argv.slice(2);
for (let i = process.argv.length - 1; i >= 2; i--) {
  if (process.argv[i] === "--compact") process.argv.splice(i, 1);
  else if (process.argv[i] === "--pretty=false") process.argv.splice(i, 1, "--pretty", "false");
  else if (process.argv[i] === "--pretty=true") process.argv.splice(i, 1, "--pretty", "true");
}
const args = process.argv.slice(2);
const pIdx = args.indexOf("--pretty");
const prettyFalse = pIdx >= 0 && args[pIdx + 1] === "false";
const pretty = !rawArgs.includes("--compact") && process.env.RX_COMPACT !== "1" && !prettyFalse && process.env.TERM !== "dumb"
  && (pIdx >= 0 || process.env.RX_PRETTY === "1" || !!process.stdout.isTTY);

const tsc = require.resolve("typescript/lib/tsc");

process.on("exit", code => {
  const watching = process.argv.includes("--watch") || process.argv.includes("-w");
  if (watching) return;
  let errors = 0;
  const codes = new Set();
  const blocks = [];
  for (const { file, result } of analyses()) {
    for (const d of result.diagnostics ?? []) {
      errors++;
      codes.add(d.code);
      const diagFile = d.file || file;
      const rel = path.relative(process.cwd(), diagFile) || diagFile;
      if (pretty && d.rendered) {
        blocks.push(d.rendered.replace(`--> ${d.file}:`, () => `--> ${rel}:`));
      } else {
        const where = d.range ? `${d.range.start.line}:${d.range.start.column + 1}` : "1:1";
        blocks.push(`${rel}:${where} - error ${d.code}: ${d.message} (${d.rule})`);
      }
    }
  }
  if (errors) {
    process.stdout.write(blocks.join(pretty ? "\n\n" : "\n") + "\n");
    if (pretty && process.env.RX_DOCS !== "0") {
      process.stdout.write(`\nFound ${errors} ReactiveScript error${errors === 1 ? "" : "s"}.\n`);
      for (const c of codes) process.stdout.write(`docs[${c}]: ${guideUrl(c)}\n`);
    }
    if (!code) process.exitCode = 1;
  }
});

runTsc(tsc, [".rx"], ts => [rxLanguagePlugin(ts)]);
