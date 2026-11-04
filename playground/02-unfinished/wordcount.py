"""Exercise 02 — finish the program.

`report()` is specified by its docstring and by test_wordcount.py, and is not
written. Everything else works.

Run:  python3 -m pytest test_wordcount.py -q
"""

import re
from collections import Counter

WORD = re.compile(r"[a-zA-Z']+")


def tokenize(text: str) -> list[str]:
    """Lowercased words. Apostrophes are kept ("don't" is one word)."""
    return [w.lower() for w in WORD.findall(text)]


def count(text: str) -> Counter:
    """Word frequencies."""
    return Counter(tokenize(text))


def report(text: str, top: int = 3) -> str:
    """Render the `top` most frequent words, one per line, as "word: count".

    Ordered by count descending, then alphabetically for ties, so the output
    is stable. Fewer than `top` distinct words yields fewer lines. Empty text
    yields an empty string (no trailing newline in any case).

    Example:
        >>> report("a b a c b a", top=2)
        'a: 3\\nb: 2'
    """
    raise NotImplementedError("exercise 02: implement report()")
