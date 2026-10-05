#!/usr/bin/env bash
# Crane walkthrough: builds a throwaway Git repository with a small Python payment service and
# runs every workflow against it, printing what Crane answers. Agent tool calls are simulated by
# sending the same hook JSON Claude Code sends, so no agent is needed.
#
#   bash examples/walkthrough.sh            # all workflows
#   bash examples/walkthrough.sh 4          # only workflow 4 (earlier setup still runs)
#
# Needs: git, python (for the Slack signature in workflow 9 and the test commands), and a built
# crane (cargo build). Set CRANE=/path/to/crane to use another binary.

set -u
HERE="$(cd "$(dirname "$0")/.." && pwd)"
CRANE="${CRANE:-$HERE/target/debug/crane}"
[ -x "$CRANE" ] || CRANE="$CRANE.exe"
ONLY="${1:-all}"
PLAY="$(mktemp -d)"
unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT CODEX_SANDBOX CODEX_SANDBOX_NETWORK_DISABLED CRANE_AGENT
cd "$PLAY"
ROOT="$(pwd -W 2>/dev/null || pwd)"   # a native path on Windows, so hook payloads resolve
PY="$(command -v python3 || command -v python)"

crane() { "$CRANE" "$@"; }
step() { echo; echo "=== $* ==="; }
want() { [ "$ONLY" = all ] || [ "$ONLY" = "$1" ]; }
# Send one Claude hook event: hook EVENT SESSION JSON_FIELDS
hook() {
  printf '{"session_id":"%s",%s}' "$2" "$3" | crane agent hook --event "$1" --profile claude
  echo "  -> exit $?"
}
write_payload() { printf '"tool_name":"Write","tool_input":{"file_path":"%s/%s","content":"%s"}' "$ROOT" "$1" "$2"; }
edit_payload() { printf '"tool_name":"Edit","tool_input":{"file_path":"%s/%s","old_string":"%s","new_string":"%s"}' "$ROOT" "$1" "$2" "$3"; }

SERVICE=pay/service.py
ORIGINAL='class PaymentService:
    def charge(self, amount):
        return self.fee(amount) + amount

    def refund(self, amount):
        return -amount

    def fee(self, amount):
        return amount // 10
'

step "Setup: repository, checkpoint, connection"
mkdir -p pay tests docs
printf '%s' "$ORIGINAL" > $SERVICE
printf 'import sys, os\nsys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))\nfrom pay.service import PaymentService\n\ndef test_charge():\n    assert PaymentService().charge(100) == 110\n\nif __name__ == "__main__":\n    test_charge()\n' > tests/test_service.py
echo "notes" > docs/notes.md
git init -q -b main && git config user.email lead@example.com && git config user.name Lead && git config core.autocrlf false
git add . && git commit -qm baseline
crane init && crane checkpoint --name baseline && crane connect
printf '{"commands": {"python": ["%s", "-c", "import runpy, sys\\nfor path in sys.argv[1:]:\\n    runpy.run_path(path, run_name=\\"__main__\\")\\n", "{files}"]}}' "$PY" > .crane/testing.json

if want 1; then
step "1. Contracts: preserve and target, checked against the checkpoint"
crane protect --function PaymentService.charge --policy core
crane target --function PaymentService.refund --policy task change_type logical_bn
crane check; echo "  -> exit $? (refund not changed yet)"
sed -i 's/return -amount/return -abs(amount)/' $SERVICE
crane check; echo "  -> exit $? (target satisfied, charge untouched)"
sed -i 's/return self.fee(amount) + amount/return amount/' $SERVICE
crane check; echo "  -> exit $? (preserved charge changed)"
printf '%s' "$ORIGINAL" > $SERVICE
rm .crane/policies/task.crane
fi

if want 2; then
step "2. Discovery, zones, packs, and a reviewed policy proposal"
crane discover | head -15
printf 'zone payments {\n    criticality critical;\n    autonomy assisted;\n    select subsystem pay;\n}\n' > .crane/zones/org.zone
crane zones
crane packs show payments
crane packs show testing | head -12
crane packs propose payments --name payments_pack
ls .crane/policies   # still only core: a pack never activates anything
DIGEST=$(crane policy show payments_pack --json | sed -n 's/.*"policy_digest": "sha256:\([0-9a-f]\{12\}\).*/\1/p' | head -1)
crane policy approve payments_pack --approver lead --confirm "$DIGEST"
ls .crane/policies
rm -f .crane/policies/payments_pack.crane   # keep the later workflows simple
fi

if want 3; then
step "3. Task planning: a task becomes a reviewable contract"
printf '{"task_format": 1, "task_id": "PAY-1", "title": "Reject negative refunds", "description": "Make `PaymentService.refund` reject negative amounts without changing `PaymentService.charge`.", "acceptance_criteria": ["refund never returns a positive number"], "repositories": ["acme/shop"]}' > .crane/tasks/PAY-1.json
crane task plan PAY-1 | head -25
printf '{"task_format": 1, "task_id": "VAGUE-1", "title": "Improve things", "description": "Make it better.", "acceptance_criteria": [], "repositories": ["acme/shop"]}' > .crane/tasks/VAGUE-1.json
crane task plan VAGUE-1 | head -8
fi

if want 4; then
step "4. Agent session: allowed, denied, approval-required, and caught-after-the-fact"
crane agent session start --profile claude --session s1 --autonomy delegated | tail -12
hook pre-tool-use s1 "$(write_payload docs/notes.md updated)"                       # routine file: allowed
hook pre-tool-use s1 "$(edit_payload $SERVICE 'return self.fee(amount) + amount' 'return amount')"   # preserved: denied
hook pre-tool-use s1 "$(edit_payload $SERVICE 'return -amount' 'return -abs(amount)')"              # critical zone: ask
sed -i 's/return self.fee(amount) + amount/return amount * 2/' $SERVICE                # a shell script edits charge
hook post-tool-use s1 '"tool_name":"Bash","tool_input":{"command":"python rewrite.py"}'
crane autonomy status claude-s1 | head -6
printf '%s' "$ORIGINAL" > $SERVICE
crane agent session verify claude-s1 --level full > /dev/null                          # verified repair
crane autonomy status claude-s1 | sed -n 3,4p
fi

if want 5; then
step "5. Autonomy: human promotion, agent self-escalation, recovery"
crane agent session start --profile claude --session s2 --autonomy assisted > /dev/null
crane autonomy promote claude-s2 --to delegated --reason "good record"
crane autonomy promote claude-s2 --to autonomous; echo "  -> exit $? (only one step at a time... from delegated it is allowed)"
CLAUDECODE=1 crane autonomy promote claude-s2 --to autonomous; echo "  -> exit $? (agent tried it)"
crane autonomy status claude-s2 | sed -n 1,10p
crane agent session resume claude-s2
crane autonomy refill claude-s2 --amount 20 --reason "reviewed" --approver lead --expires 1h
crane autonomy history claude-s2
fi

if want 6; then
step "6. Risk budget: spend, exhaust, refill"
printf '{"max": {"delegated": 6}}' > .crane/budget.json
crane agent session start --profile claude --session s3 > /dev/null
for round in 1 2 3; do
  hook pre-tool-use s3 "$(write_payload docs/notes.md "v$round")"
  echo "v$round" > docs/notes.md
  hook post-tool-use s3 "$(write_payload docs/notes.md "v$round")" > /dev/null
done
crane autonomy budget claude-s3 | head -8
crane autonomy refill claude-s3 --amount 10 --reason "next step approved" --approver lead --expires 2h
rm .crane/budget.json
git checkout -q docs/notes.md
fi

if want 7; then
step "7. Finalization: mandatory contract tests and the attestation"
crane agent session start --profile claude --session s4 --autonomy autonomous > /dev/null
hook pre-tool-use s4 "$(write_payload docs/notes.md 'release notes')"
echo "release notes" > docs/notes.md
hook post-tool-use s4 "$(write_payload docs/notes.md 'release notes')" > /dev/null
hook stop s4 '"stop_hook_active":false' > /dev/null
crane agent session finalize claude-s4
crane test-contract --session claude-s4
fi

if want 8; then
step "8. Evidence: inspect, export, reconstruct, detect tampering"
crane session inspect claude-s4 | head -20
crane session export claude-s4 --json > "$PLAY/export.json"
crane session inspect --export "$PLAY/export.json" | tail -2
sed 's/"decision": "allow"/"decision": "deny"/' "$PLAY/export.json" > "$PLAY/forged.json"
crane session inspect --export "$PLAY/forged.json" | sed -n 5p; echo "  -> exit $?"
crane session export claude-s4 --otlp | head -c 200; echo
fi

if want 9; then
step "9. Delivery: branch, checks, pull request, approvals, merge"
git add .crane/zones .crane/policies .crane/testing.json && git commit -qm "crane configuration"
crane deliver run claude-s4
crane deliver approve claude-s4 --approver lead --reason "looks good" | tail -2
crane deliver merge claude-s4 | head -4
git log --oneline -3
cat .crane/checkpoints/trusted_claude_s4.json
ls .crane/runtime/delivery/outbox
fi

if want 10; then
step "10. Control plane: API, simulator"
crane dashboard api GET /api/screens
crane dashboard api POST /api/contracts/preview --body '{"name":"pay_refund","checkpoint":"baseline","rules":[{"rule":"preserve","kind":"function","target":"PaymentService.refund"}]}'
crane dashboard api POST /api/simulate --body '{"agentscript":"policy shadow {\n    checkpoint baseline;\n    preserve --function PaymentService.refund;\n}\n"}' | grep -A6 false_positive_analysis
echo "Open the dashboard yourself with: (cd $PLAY && $CRANE dashboard)"
fi

echo
echo "Playground left at $PLAY (delete it when done)."
