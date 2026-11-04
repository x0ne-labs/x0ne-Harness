#!/bin/bash
# ═══════════════════════════════════════════════════════════════════════════════
# X0NE Stress-Test Prompt Battery
# ═══════════════════════════════════════════════════════════════════════════════
#
# Prompts designed to push the agent loop to its limits and generate rich
# session logs for validating that complex flows work end-to-end.
#
# Usage:
#   make stress-test               # Run all prompts sequentially
#   make stress-pick N=3           # Run prompt 3 only
#   make stress-dry                # Print prompts without executing
#
# Session logs: ~/.x0ne/sessions/*.jsonl
# ═══════════════════════════════════════════════════════════════════════════════

set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

X0NE="${X0NE_BIN:-${CARGO_TARGET_DIR:-${PROJECT_ROOT}/target}/debug/x0ne}"
PROVIDER="${X0NE_PROVIDER:-anthropic}"
MODEL="${X0NE_MODEL:-claude-haiku-4-5-20251001}"
WORK_DIR="$(mktemp -d /tmp/x0ne-stress.XXXXXXXX)"
PICK=""
DRY_RUN=false

for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=true ;;
        --pick)    shift; PICK="${1:-}" ;;
        [0-9]*)    PICK="$arg" ;;
    esac
done

if [ "$DRY_RUN" = false ] && [ ! -x "$X0NE" ]; then
    echo "Error: $X0NE not found. Run 'make build' first."
    exit 1
fi

echo "═══════════════════════════════════════════"
echo "  X0NE Stress-Test Battery"
echo "  Binary:   ${X0NE}"
echo "  Provider: ${PROVIDER}/${MODEL}"
echo "  Work dir: ${WORK_DIR}"
echo "═══════════════════════════════════════════"
echo ""

# Counter for pass/fail
PASS=0
FAIL=0
SKIP=0

run_prompt() {
    local id="$1"
    local name="$2"
    # Prompt is read from stdin (heredoc)

    if [ -n "$PICK" ] && [ "$id" != "$PICK" ]; then
        SKIP=$((SKIP + 1))
        return
    fi

    local prompt
    prompt="$(cat)"

    echo "── [$id] $name ──────────────────"

    if $DRY_RUN; then
        echo "$prompt"
        echo ""
        return
    fi

    # Create isolated workspace
    local ws="${WORK_DIR}/test-${id}"
    mkdir -p "$ws"

    # Write prompt to temp file to avoid all quoting issues
    local prompt_file="${ws}/.prompt.txt"
    printf '%s' "$prompt" > "$prompt_file"

    local prompt_arg
    prompt_arg="$(cat "$prompt_file")"

    echo "  ⏳ Running..."
    local log_file="${ws}/output.log"
    if (cd "$ws" && X0NE_MODEL="$MODEL" "$X0NE" --provider "$PROVIDER" -p "$prompt_arg") 2>&1 | tee "$log_file"; then
        echo "  ✅ Prompt $id completed"
        PASS=$((PASS + 1))
    else
        echo "  ❌ Prompt $id failed (exit $?)"
        FAIL=$((FAIL + 1))
    fi
    echo "  → Log: $log_file"
    echo ""
}

# ═══════════════════════════════════════════════════════════════════════════════
# CATEGORY 1: Schema Drift & Recovery
# ═══════════════════════════════════════════════════════════════════════════════

run_prompt 1 "Pipeline schema stress" <<'PROMPT'
Create a Pipeline with exactly 7 steps:
1. Create a directory /tmp/x0ne-stress/schema-test
2. Write a file /tmp/x0ne-stress/schema-test/lorem.txt with 50 lines of lorem ipsum
3. Read back just lines 10-20 of that file
4. Search for the word 'ipsum' in /tmp/x0ne-stress/schema-test/ recursively
5. Run 'wc -l' on the file /tmp/x0ne-stress/schema-test/lorem.txt
6. Compute the SHA256 checksum of /tmp/x0ne-stress/schema-test/lorem.txt
7. List the directory contents of /tmp/x0ne-stress/schema-test/ with file sizes
Do everything in a single Pipeline action. Show me the complete result of each step.
PROMPT

run_prompt 2 "Nested object construction" <<'PROMPT'
I need you to:
1. Read the file /etc/os-release
2. Parse each KEY=VALUE line into a JSON object
3. Write that JSON object to /tmp/x0ne-stress/os-info.json using WriteJson
4. Read it back with ReadJson and tell me the PRETTY_NAME value
5. Run 'uname -a' and tell me the kernel version

Do each step as a separate action (not a Pipeline). Think carefully about the JSON structure for WriteJson — the data field must be a proper JSON object, not a string.
PROMPT

# ═══════════════════════════════════════════════════════════════════════════════
# CATEGORY 2: Multi-step Reasoning & Error Recovery
# ═══════════════════════════════════════════════════════════════════════════════

run_prompt 3 "Debug a deliberately broken program" <<'PROMPT'
Write a Python program to /tmp/x0ne-stress/fib.py that calculates fibonacci numbers using a loop. The program should take N from sys.argv[1] and print fib(N).

But write it with a DELIBERATE off-by-one bug — make the loop range wrong so it returns fib(N-1) instead of fib(N).

Then run it with: python3 /tmp/x0ne-stress/fib.py 10
The correct answer for fib(10) is 55. If the output is wrong, identify the bug, fix it with ReplaceInFile, and run it again.
PROMPT

run_prompt 4 "Compile-fix-iterate loop" <<'PROMPT'
Create a small C program at /tmp/x0ne-stress/calc.c that:
- Takes two numbers and an operator (+, -, *, /) from argv
- Prints the result
- Handles division by zero with an error message

Write it, then compile with: gcc -o /tmp/x0ne-stress/calc /tmp/x0ne-stress/calc.c -Wall -Werror
If it does not compile, read the errors and fix them. Keep iterating until it compiles.
Then test with: /tmp/x0ne-stress/calc 10 + 5
And: /tmp/x0ne-stress/calc 10 / 0
PROMPT

run_prompt 5 "Find and fix logic bugs" <<'PROMPT'
Write /tmp/x0ne-stress/buggy.py with this exact content then find the bugs:

def merge_sorted(a, b):
    result = []
    i = j = 0
    while i < len(a) and j < len(b):
        if a[i] <= b[j]:
            result.append(a[i])
            i += 1
        else:
            result.append(b[j])
            j += 1
    return result

def binary_search(arr, target):
    lo, hi = 0, len(arr)
    while lo < hi:
        mid = (lo + hi) // 2
        if arr[mid] == target:
            return mid
        elif arr[mid] < target:
            lo = mid
        else:
            hi = mid
    return -1

if __name__ == "__main__":
    print("merge:", merge_sorted([1,3,5], [2,4,6]))
    print("search:", binary_search([1,2,3,4,5,6,7,8], 5))

Write this file, run it, and identify which functions have bugs. The merge_sorted is missing the remaining elements after the while loop. The binary_search has an infinite loop because lo = mid should be lo = mid + 1. Fix both bugs and run again.
PROMPT

# ═══════════════════════════════════════════════════════════════════════════════
# CATEGORY 3: Parallel Worlds & Multi-Agent
# ═══════════════════════════════════════════════════════════════════════════════

run_prompt 6 "Parallel implementation race" <<'PROMPT'
I want to compare 3 different sorting approaches. Use SpawnWorld to create 3 parallel agents:

Agent 'sort-bubble': Write a bubble sort in Python at /tmp/x0ne-stress/bubble.py that sorts 1000 random integers and prints the elapsed time. Run it.

Agent 'sort-merge': Write a merge sort in Python at /tmp/x0ne-stress/merge.py that sorts 1000 random integers and prints the elapsed time. Run it.

Agent 'sort-builtin': Write a script at /tmp/x0ne-stress/builtin.py using Python built-in sorted() on 1000 random integers and prints the elapsed time. Run it.

Wait for all 3 agents with WaitAgent, then tell me which approach was fastest.
PROMPT

run_prompt 7 "Agent coordination with shared state" <<'PROMPT'
Create 2 parallel agents with SpawnWorld:

Agent 'writer': Write the numbers 1 through 50 to /tmp/x0ne-stress/shared.txt, one per line.

Agent 'reader': Wait 3 seconds using the Sleep primitive or RunCommand sleep 3, then read /tmp/x0ne-stress/shared.txt and count how many lines it has.

Wait for both agents. Did the reader see all 50 lines?
PROMPT

run_prompt 8 "Stress the agent registry" <<'PROMPT'
Spawn 5 parallel agents using SpawnWorld with ids: alpha, beta, gamma, delta, epsilon.
Each agent should:
  1. Write a file /tmp/x0ne-stress/agent-{id}.txt with its name and timestamp from running the date command
  2. Run sleep 1
  3. Read its own file back and report the content

Then WaitAgent for all 5, one by one. Summarize which finished first and which took longest.
PROMPT

# ═══════════════════════════════════════════════════════════════════════════════
# CATEGORY 4: Complex File Operations & Snapshot/Rollback
# ═══════════════════════════════════════════════════════════════════════════════

run_prompt 9 "Snapshot and rollback on failure" <<'PROMPT'
1. Write /tmp/x0ne-stress/data.json with: {"users": [{"name": "alice", "score": 100}, {"name": "bob", "score": 85}]}
2. Create a snapshot named 'before-migration'
3. Now modify the file: add an "id" field (just use 1 and 2) and a "rank" field ("gold" for score>=90, "silver" otherwise) to each user
4. Write the updated data back to the same file
5. Verify the file has the id field by reading it
6. Now DELIBERATELY write invalid content to the file to simulate a corruption: write just the string "CORRUPTED"
7. Restore from the 'before-migration' snapshot
8. Read the file and confirm it is back to the original content
9. Delete the 'before-migration' snapshot
PROMPT

run_prompt 10 "Multi-file refactor with validation" <<'PROMPT'
Create a mini Python project in /tmp/x0ne-stress/miniproject/:
- utils.py: define a function validate_email(email) that returns True if the email contains @ and at least one dot after @
- models.py: define a class User with name and email fields, and a method is_valid() that calls validate_email
- main.py: create 2 User instances, call is_valid() on each, print results

Write all 3 files, then run main.py with python3.
Then refactor: rename validate_email to check_email in both utils.py and models.py. Run main.py again to verify nothing broke.
PROMPT

# ═══════════════════════════════════════════════════════════════════════════════
# CATEGORY 5: Adversarial / Edge Cases
# ═══════════════════════════════════════════════════════════════════════════════

run_prompt 11 "Action-like content in a file" <<'PROMPT'
Write a markdown file to /tmp/x0ne-stress/readme.md that contains documentation about X0NE action format. The file should include this literal text (as documentation, NOT to be executed):

## Example action
The action format looks like this:
{"type": "ReadFile", "path": "/some/file"}

## Pipeline example
{"type": "Pipeline", "steps": [{"type": "ReadFile", "path": "/tmp/test"}]}

After writing the file, read it back and tell me its size in bytes and number of lines. Be very careful: the JSON in the file is documentation content, not actions for you to execute.
PROMPT

run_prompt 12 "Unicode and special characters" <<'PROMPT'
Create /tmp/x0ne-stress/unicode.txt with these exact lines:
Line 1: Hello World
Line 2: Bonjour le monde
Line 3: Hola Mundo
Line 4: 12345 numbers
Line 5: special chars: @#$%^&*
Line 6: tabs	and  spaces
Line 7: empty line follows

Line 9: back to content

Then read it back. Search for the word 'monde' and tell me which line it is on. How many lines does the file have total?
PROMPT

run_prompt 13 "Large file operations" <<'PROMPT'
1. Write a Python script to /tmp/x0ne-stress/gen.py that generates a 200-line file where each line is formatted as: line_{n}: {n squared}. Save output to /tmp/x0ne-stress/bigfile.txt
2. Run the script with python3
3. Read lines 95-105 of the generated file
4. Search for "10000" in the file
5. Count total lines with wc -l
6. Show me the first and last line of the file
PROMPT

# ═══════════════════════════════════════════════════════════════════════════════
# CATEGORY 6: Real-World Development Tasks
# ═══════════════════════════════════════════════════════════════════════════════

run_prompt 14 "Build and test from scratch" <<'PROMPT'
Create a Python calculator module at /tmp/x0ne-stress/calc/:
- calc.py: a function calculate(a, op, b) that supports +, -, *, / and returns the result. Division by zero should raise ValueError.
- test_calc.py: use unittest to write at least 8 test cases covering all operations, division by zero, and edge cases like negative numbers.
- Run the tests with: python3 -m unittest test_calc.py -v
- If any test fails, fix the code and re-run until all pass.
PROMPT

run_prompt 15 "Investigate a performance issue" <<'PROMPT'
Write /tmp/x0ne-stress/slow.py with a function that removes duplicates from a list using nested loops (O(n^2) approach). Test it on a list of 5000 items with duplicates and print the time.

Then write /tmp/x0ne-stress/fast.py with the same logic but using a set for O(n) dedup. Test on the same data size and print the time.

Run both and compare. Explain the algorithmic difference and the speedup factor.
PROMPT

run_prompt 16 "Security audit a script" <<'PROMPT'
Write /tmp/x0ne-stress/vulnerable.py with these intentionally insecure functions:
- run_user_command(cmd): uses os.popen(cmd) to execute arbitrary commands
- load_data(filename): uses pickle.load to deserialize untrusted data
- make_temp(): uses tempfile.mktemp() which has a race condition
- check_password(user_input, stored): uses == for comparison (timing attack)

Read the file and identify all 4 security vulnerabilities. For each one, name the issue, explain the risk, and show the fix. Write the fixed version to /tmp/x0ne-stress/secure.py.
PROMPT

# ═══════════════════════════════════════════════════════════════════════════════
# CATEGORY 7: Incus Parallel Worlds (requires Incus setup)
# ═══════════════════════════════════════════════════════════════════════════════

run_prompt 17 "Cross-platform test via Incus" <<'PROMPT'
Use RunIncusGroup to run this command on 2 different distros in parallel:
  cat /etc/os-release | head -3 && python3 --version 2>/dev/null || echo "no python3"

Use images:alpine/edge and images:ubuntu/24.04.

Compare the outputs: which distro has Python3 pre-installed?
PROMPT

run_prompt 18 "Build isolation test via Incus" <<'PROMPT'
Use RunIncusGroup with 2 tasks to compile and run a simple C program on Alpine and Ubuntu:

The C source: #include <stdio.h>
int main() { printf("sizeof_long=%zu sizeof_ptr=%zu\n", sizeof(long), sizeof(void*)); return 0; }

Task 1 (Alpine): apk add gcc musl-dev && echo '#include <stdio.h>' > /tmp/test.c && echo 'int main(){printf("sizeof_long=%zu sizeof_ptr=%zu\n",sizeof(long),sizeof(void*));return 0;}' >> /tmp/test.c && gcc -o /tmp/test /tmp/test.c && /tmp/test

Task 2 (Ubuntu): apt-get update -qq && apt-get install -y -qq gcc > /dev/null && echo '#include <stdio.h>' > /tmp/test.c && echo 'int main(){printf("sizeof_long=%zu sizeof_ptr=%zu\n",sizeof(long),sizeof(void*));return 0;}' >> /tmp/test.c && gcc -o /tmp/test /tmp/test.c && /tmp/test

Compare the outputs. Are the sizes the same?
PROMPT

# ═══════════════════════════════════════════════════════════════════════════════

echo ""
echo "═══════════════════════════════════════════"
echo "  Results: ${PASS} passed, ${FAIL} failed, ${SKIP} skipped"
echo "  Session logs: ~/.x0ne/sessions/"
echo "  Work dir: ${WORK_DIR}"
echo "═══════════════════════════════════════════"
