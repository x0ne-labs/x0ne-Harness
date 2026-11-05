# Throwaway HOME, demo project and scripted model for docs/assets/tui.tape.
# Sourced by the tape; writes under /tmp/demo only.
DEMO_ROOT=/tmp/demo
DEMO_ASSETS=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
rm -rf "$DEMO_ROOT" && mkdir -p "$DEMO_ROOT/home/.x0ne" "$DEMO_ROOT/acme-api"
cat > "$DEMO_ROOT/acme-api/README.md" <<'MD'
# acme-api

A small payments API. `charge(amount_cents)` returns a receipt;
amounts must be positive. Run the tests with `python3 -m unittest`.
MD
cat > "$DEMO_ROOT/acme-api/app.py" <<'PY'
def charge(amount_cents):
    if amount_cents <= 0:
        raise ValueError("amount must be positive")
    return {"status": "ok", "amount": amount_cents}
PY
cat > "$DEMO_ROOT/acme-api/test_app.py" <<'PY'
import unittest
from app import charge


class ChargeTest(unittest.TestCase):
    def test_ok(self):
        self.assertEqual(charge(500)["status"], "ok")

    def test_amount(self):
        self.assertEqual(charge(500)["amount"], 500)

    def test_refuses_zero(self):
        with self.assertRaises(ValueError):
            charge(0)
PY
printf 'PAYMENTS_API_KEY=placeholder-for-the-demo\n' > "$DEMO_ROOT/acme-api/.env"
# api_key_env = "" sends no key (docs/install.md); the compat variable would
# still be read first, so it is cleared.
unset X0NE_OPENAI_COMPAT_KEY
cat > "$DEMO_ROOT/home/.x0ne/config.toml" <<'TOML'
[llm]
provider = "openai-compat"

[llm.openai_compat]
base_url = "http://127.0.0.1:9931"
model = "scripted-demo"
api_key_env = ""
TOML
python3 "$DEMO_ASSETS/tui-demo-model.py" 9931 "$DEMO_ROOT/model.log" &
export HOME="$DEMO_ROOT/home"
cd "$DEMO_ROOT/acme-api"
# The status bar shows the RAM and load of the machine it runs on: x0ne reads
# fixed ones here. The second /proc mount is what lets the runtime's own bwrap
# mount a /proc: the kernel refuses it when every /proc in sight has a file over it.
printf 'MemTotal: 16777216 kB\nMemAvailable: 12582912 kB\n' > "$DEMO_ROOT/meminfo"
printf '0.40 0.35 0.30 1/200 1000\n' > "$DEMO_ROOT/loadavg"
mkdir "$DEMO_ROOT/proc"
x0ne() {
  bwrap --dev-bind / / --proc /proc \
    --ro-bind "$DEMO_ROOT/meminfo" /proc/meminfo \
    --ro-bind "$DEMO_ROOT/loadavg" /proc/loadavg \
    --proc "$DEMO_ROOT/proc" -- "$(type -P x0ne)" "$@"
}
