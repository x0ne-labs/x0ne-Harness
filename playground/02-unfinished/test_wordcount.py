"""Tests for exercise 02.

Runs with plain `python3 test_wordcount.py` — no pytest, nothing to install.
Exit code 0 means everything passes.
"""

import sys
import traceback

from wordcount import count, report, tokenize


def test_tokenize_lowercases_and_keeps_apostrophes():
    assert tokenize("Don't STOP") == ["don't", "stop"]


def test_count_is_frequency():
    assert count("a b a")["a"] == 2


def test_report_orders_by_count_desc():
    assert report("a b a c b a", top=2) == "a: 3\nb: 2"


def test_report_breaks_ties_alphabetically():
    # b and c both appear twice; alphabetical order makes the output stable.
    assert report("b b c c a", top=2) == "b: 2\nc: 2"


def test_report_returns_fewer_lines_than_asked_when_it_must():
    assert report("solo", top=5) == "solo: 1"


def test_report_on_empty_text_is_empty_string():
    assert report("", top=3) == ""


def test_report_has_no_trailing_newline():
    assert not report("a b", top=2).endswith("\n")


def main() -> int:
    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_")]
    failed = 0
    for t in tests:
        try:
            t()
            print(f"  ok   {t.__name__}")
        except Exception:
            failed += 1
            print(f"  FAIL {t.__name__}")
            traceback.print_exc()
    print(f"\n{len(tests) - failed} passed, {failed} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
