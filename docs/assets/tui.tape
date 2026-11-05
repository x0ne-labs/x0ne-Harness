# Records docs/assets/tui.gif with VHS (github.com/charmbracelet/vhs): x0ne --tui
# on a throwaway project; tui-demo-model.py scripts the model's replies, no key.
# From the repo root, x0ne, python3 and bwrap on PATH: vhs docs/assets/tui.tape
# Recorded with VHS v0.12.1 plus Debian's bubblewrap, in podman with
# --security-opt 'unmask=/proc/*' so that bwrap can mount /proc.

Output docs/assets/tui.gif

Require x0ne
Require python3
Require bwrap

Set Shell "bash"
Set FontFamily "JetBrains Mono"
Set FontSize 13
Set Width 1200
Set Height 760
Set Padding 16
Set TypingSpeed 30ms

# First run, not recorded: the setup wizard, Developer profile, current directory.
Hide
Type "source docs/assets/tui-demo.sh && clear" Enter
Sleep 500ms
Type "x0ne --tui" Enter
Wait+Screen@20s /Choice \[1\]/
Enter
Wait+Screen@20s /comma-separated/
Enter
Wait+Screen@20s /ready when you are/
Ctrl+D
Sleep 1s
Type "clear" Enter
Sleep 300ms
Show

# Second run: boot, a read, a read refused by the policy, a command held for approval.
Type "x0ne --tui" Enter
Wait+Screen@20s /ready when you are/
Sleep 1.5s
Type "What does this project do? Also check the API key in .env" Enter
Wait+Screen@20s /did not see the key/
Sleep 2.5s
Type "Run the tests" Enter
Wait+Screen@20s /allow once/
Sleep 2s
Type "1"
Wait+Screen@20s /All 3 tests pass/
Sleep 3s
