const { spawnSync } = require("node:child_process");
const out = spawnSync("bash", ["-lc", "source scripts/env.sh && printf '%s' \"$MERCURY_EVAL_TEST_ERROR\""], { encoding: "utf8" });
process.stderr.write((out.stdout || "FAIL tests/main.test.ts") + "\n");
process.exit(1);
