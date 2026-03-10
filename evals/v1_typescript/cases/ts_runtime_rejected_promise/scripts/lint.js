const { spawnSync } = require("node:child_process");
const out = spawnSync("bash", ["-lc", "source scripts/env.sh && printf '%s' \"$MERCURY_EVAL_LINT_ERROR\""], { encoding: "utf8" });
process.stderr.write((out.stdout || "src/index.ts:1:1  error  Unexpected any") + "\n");
process.exit(1);
