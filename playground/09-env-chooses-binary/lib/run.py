"""Prints the greeting. Imports its sibling `greet` from the same directory.

Python puts the directory of the script being run at the front of `sys.path`,
so invoking this file by its path (`python3 lib/run.py`) makes `greet`
importable with no environment tweaking at all.
"""

from greet import message

print(message())
