#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${CRANE_BIN:-$ROOT/target/debug/crane}"
if [[ ! -x "$BIN" ]]; then
  echo "Crane binary is not executable: $BIN" >&2
  exit 1
fi
DEMO="$(mktemp -d)"
trap 'rm -rf "$DEMO"' EXIT
cd "$DEMO"
"$BIN" --version | grep -Eq '^crane [0-9]+\.[0-9]+\.[0-9]+$'
git init -q
git config user.email crane@example.com
git config user.name "Crane Demo"
"$BIN" init
cat > GatewayService.java <<'JAVA'
class GatewayService {
    public void call() {
        System.out.println("payment");
    }
}
JAVA
git add GatewayService.java
git commit -qm "trusted baseline"
"$BIN" status >/tmp/crane_status.txt
grep -q "Crane status" /tmp/crane_status.txt
"$BIN" checkpoint --name baseline
"$BIN" protect --function GatewayService.call --policy payment_gateway
"$BIN" check >/tmp/crane_pass.txt
grep -q "Crane check: PASS" /tmp/crane_pass.txt
python3 - <<'PY'
from pathlib import Path
p=Path("GatewayService.java")
p.write_text('''class GatewayService {\n    public void call() {\n        System.out.println("modified");\n    }\n}\n''')
PY
if "$BIN" check >/tmp/crane_fail.txt 2>&1; then
  echo "expected check to fail"
  exit 1
fi
grep -q "FAIL payment_gateway" /tmp/crane_fail.txt
grep -q "Crane check: FAIL" /tmp/crane_fail.txt
echo "Smoke test passed."
