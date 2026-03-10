#!/usr/bin/env bash
set -euo pipefail
export MERCURY_EVAL_TEST_ERROR="FAIL tests/math.test.ts\n  AssertionError: expected 3 to equal 4"
