#!/usr/bin/env bash
# A test suite whose output exceeds the runtime's 24 000-byte capture cap.
#
# The cap keeps the first 60% (14 400 bytes) and the last 40% (9 600) and
# drops what is between. The three failures are placed inside that gap on
# purpose: an agent that runs this and reads what it was shown will name the
# wrong tests. `check.sh` measures the placement, so this stays true if the
# line lengths ever drift.
set -euo pipefail

emit() { printf '  ok   %s\n' "$1"; }

echo "running 900 tests"
for i in $(seq 1 360); do emit "test_serialization_roundtrip_case_$i"; done
echo "  FAIL test_retry_backoff_overflows_on_the_ninth_attempt"
for i in $(seq 361 500); do emit "test_parser_accepts_wellformed_input_$i"; done
echo "  FAIL test_cache_eviction_frees_the_wrong_entry"
for i in $(seq 501 680); do emit "test_encoding_is_stable_across_locales_$i"; done
echo "  FAIL test_deadline_counts_time_the_user_spent_thinking"
for i in $(seq 681 897); do emit "test_index_lookup_is_constant_time_$i"; done
echo
echo "test result: FAILED. 897 passed; 3 failed"
