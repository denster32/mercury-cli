#!/usr/bin/env bash
set -euo pipefail
export MERCURY_EVAL_CHECK_ERROR="src/index.ts(7,2): error TS1005: '}' expected."
export MERCURY_EVAL_TEST_ERROR="FAIL tests/main.test.ts\n  Parse fixture should not run tests"
export MERCURY_EVAL_LINT_ERROR="src/index.ts:1:1  error  Parse fixture lint placeholder"
