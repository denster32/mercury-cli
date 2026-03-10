#!/usr/bin/env bash
set -euo pipefail
export MERCURY_EVAL_LINT_ERROR="src/index.ts:5:7  error  'debugValue' is assigned a value but never used  @typescript-eslint/no-unused-vars"
