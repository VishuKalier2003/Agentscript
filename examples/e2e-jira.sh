#!/usr/bin/env bash
# End-to-end run on a test codebase: a Jira ticket becomes a reviewed task contract, an agent
# session does the work under zones, contracts, autonomy, and budget, Crane validates it, opens a
# pull request, collects a signed Slack approval and a human approval, merges, makes the merge the
# new trusted checkpoint, and only then closes the Jira issue.
#
#   cargo build && bash examples/e2e-jira.sh
#
# The agent is simulated with the exact hook JSON Claude Code sends (to drive a real Claude Code
# instead, see the note at step 5). Jira and Slack are local: the webhook is replayed from a file,
# outbound calls land in .crane/runtime/delivery/outbox, and the Slack click is signed like Slack
# signs it. Needs git and python.

set -u
HERE="$(cd "$(dirname "$0")/.." && pwd)"
CRANE="${CRANE:-$HERE/target/debug/crane}"
[ -x "$CRANE" ] || CRANE="$CRANE.exe"
FIXTURES="$(cd "$HERE/tests/fixtures/orchestration" && (pwd -W 2>/dev/null || pwd))"
PLAY="$(mktemp -d)"
JIRA="$(cd "$(mktemp -d)" && (pwd -W 2>/dev/null || pwd))"   # the Jira stand-in, outside the repository
unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT CODEX_SANDBOX CODEX_SANDBOX_NETWORK_DISABLED CRANE_AGENT
export MSYS_NO_PATHCONV=1                     # keep Git Bash from rewriting /api/... arguments
export CRANE_SLACK_SECRET="e2e-slack-signing-secret"
cd "$PLAY"
ROOT="$(pwd -W 2>/dev/null || pwd)"
PY=python; python3 -c "print(1)" >/dev/null 2>&1 && PY=python3

crane() { "$CRANE" "$@"; }
step() { echo; echo "=================== $* ==================="; }
note() { echo "  # $*"; }
# Send a Claude Code hook event for the task's session; the body is built by Python from a dict
hook() {
  "$PY" -c "import json, sys; body = json.loads(sys.argv[2]); body['session_id'] = 'task-PAY-1821-v1'; print(json.dumps(body))" "$1" "$2" \
    | crane agent hook --event "$1" --profile claude
  echo "  -> hook exit $?"
}
state() { crane task status PAY-1821 --json | "$PY" -c "import json, sys; print('  task state:', json.load(sys.stdin)[0]['state'])"; }

step "1. The test codebase (acme/shop): a Python payment service with tests"
mkdir -p services/payments/pay tests docs
cat > services/payments/pay/service.py <<'EOF'
class PaymentService:
    def charge(self, amount):
        return self.fee(amount) + amount

    def refund(self, amount):
        return -amount

    def fee(self, amount):
        return amount // 10
EOF
cat > tests/test_service.py <<'EOF'
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "services", "payments"))
from pay.service import PaymentService


def test_charge():
    assert PaymentService().charge(100) == 110


if __name__ == "__main__":
    test_charge()
EOF
echo "# Shop" > README.md
printf '/services/payments/ @acme/payments\n' > CODEOWNERS
git init -q -b main && git config user.email lead@acme.example && git config user.name "Release Lead" && git config core.autocrlf false
git remote add origin https://github.com/acme/shop.git
git add . && git commit -qm "acme/shop baseline"
git log --oneline

step "2. Onboarding: init, trusted checkpoint, one-time connection, semantic inventory"
crane repo connect          # init + trusted checkpoint + discovery, recorded once
crane repo status
crane discover | sed -n 1,12p

step "3. Zones: Society recommends, a human reviews and approves"
crane zones recommend
crane zones recommendations
crane zones review payments | sed -n 1,16p
for zone in payments tests; do
  CONFIRM=$(crane zones review $zone --json | "$PY" -c "import json, sys; print(json.load(sys.stdin)['digest'][7:19])")
  crane zones approve $zone --approver security-lead --confirm "$CONFIRM" | head -1
done
crane zones | sed -n 1,12p

step "4. Contracts: pack recommendations, then a permanent policy made in the control plane and approved by a human"
crane packs show payments | sed -n 1,14p
note "the pack only recommends; we create the policy we want through the dashboard API (the same call the visual editor makes)"
crane dashboard api POST /api/contracts --body '{"draft": {"name": "payments_core", "checkpoint": "baseline", "rules": [{"rule": "preserve", "kind": "function", "target": "PaymentService.charge"}, {"rule": "preserve", "kind": "function", "target": "PaymentService.fee"}]}, "by": "payments-lead"}'
DIGEST=$(crane policy show payments_core --json | "$PY" -c "import json, sys; print(json.load(sys.stdin)['policy_digest'][7:19])")
crane policy approve payments_core --approver security-lead --confirm "$DIGEST"
note "organization configuration: tests Crane runs, delivery checks and merge policy, Slack, Jira mapping"
cat > .crane/testing.json <<EOF
{"commands": {"python": ["$PY", "-c", "import runpy, sys\nfor path in sys.argv[1:]:\n    runpy.run_path(path, run_name='__main__')\n", "{files}"]}}
EOF
cat > .crane/delivery.json <<EOF
{
  "checks": [{"name": "compile", "kind": "lint", "command": ["$PY", "-m", "py_compile", "services/payments/pay/service.py"]}],
  "merge_policy": {"rules": [{"name": "payments-critical", "min_criticality": "critical", "approvals": 2, "approvers": ["payments-lead", "security-lead"]}]},
  "slack": {"notify": ["#payments-delivery"], "signing_secret_env": "CRANE_SLACK_SECRET", "users": {"U0PAYLEAD": "payments-lead", "U0SECLEAD": "security-lead"}, "pr_url_template": "https://github.com/acme/shop/pull/{number}"},
  "trackers": {"jira": {"done_transition": "31", "base_url": "https://acme.atlassian.net", "transport": ["$PY", "$JIRA/jira.py", "$JIRA/jira-state.json"]}}
}
EOF
# A local stand-in for Jira (examples/jira_transport.py is the real transport)
echo '{"status": "In Review", "comments": [], "transitions": []}' > "$JIRA/jira-state.json"
cat > "$JIRA/jira.py" <<'PYEOF'
import json, sys
state = json.load(open(sys.argv[1])); request = json.load(sys.stdin)
def answer(status, body=None):
    json.dump(state, open(sys.argv[1], "w")); print(json.dumps({"status": status, "body": body}))
if request["method"] == "GET" and request["path"].endswith("/comment"): answer(200, {"comments": state["comments"]})
elif request["method"] == "GET": answer(200, {"fields": {"status": {"name": state["status"], "statusCategory": {"key": "done" if state["status"] == "Done" else "indeterminate"}}}})
elif request["path"].endswith("/comment"): state["comments"].append(request["body"]); answer(201, {"id": str(len(state["comments"]))})
else: state["transitions"].append(request["body"]["transition"]["id"]); state["status"] = "Done"; answer(204)
PYEOF
mkdir -p .crane/sources
sed 's/"agent_profile": "generic"/"agent_profile": "claude"/' "$FIXTURES/config.json" > .crane/sources/config.json
git add .crane/zones .crane/policies .crane/testing.json .crane/delivery.json .crane/sources/config.json
git commit -qm "Crane configuration: zones, payments_core, testing, delivery, Jira mapping"
echo "  committed: $(git log -1 --format=%s)"

step "5. Jira: PAY-1821 'Reject negative refunds' is created and assigned to Crane Bot"
note "this is the webhook body Jira posts; 'crane task serve' accepts the same on POST /webhooks/jira"
crane task ingest --source jira "$FIXTURES/jira/01-created-PAY-1821.json"
state
note "the task was normalized and compiled into a bound task contract that waits for review:"
crane task contract show PAY-1821 | sed -n 1,30p

step "6. A human reviews and approves the task contract; the agent session starts"
DIGEST=$(crane task contract show PAY-1821 --json | "$PY" -c "import json, sys; print(json.load(sys.stdin)['digest'][7:19])")
crane task contract approve PAY-1821 --approver payments-lead --confirm "$DIGEST" | head -3
crane policy status | sed -n 1,10p
crane task sync PAY-1821 > /dev/null
state
note "what the agent is told at session start (Claude Code shows this as context):"
hook session-start '{"source": "startup", "model": "claude-opus"}' | sed -n '/ACTIVE CONTRACT/,/zones:/p'
note "to use a real Claude Code instead of the simulated calls below: run 'crane agent install --profile claude'"
note "here, then start Claude Code and give it the ticket; Crane enforces the same way."

step "7. The agent works: a forbidden change, the real change (needs approval), a new test"
note "7a. it tries to 'simplify' charge, which payments_core preserves:"
hook pre-tool-use '{"tool_name": "Edit", "tool_input": {"file_path": "'"$ROOT"'/services/payments/pay/service.py", "old_string": "return self.fee(amount) + amount", "new_string": "return amount"}}'
FIXED='class PaymentService:
    def charge(self, amount):
        return self.fee(amount) + amount

    def refund(self, amount):
        if amount < 0:
            raise ValueError("refund amount must not be negative")
        return -amount

    def fee(self, amount):
        return amount // 10
'
WRITE=$("$PY" -c "import json, sys; print(json.dumps({'tool_name': 'Write', 'tool_input': {'file_path': sys.argv[1], 'content': sys.argv[2]}}))" "$ROOT/services/payments/pay/service.py" "$FIXED")
note "7b. it changes refund; payments is a critical zone, so Claude Code asks the human:"
hook pre-tool-use "$WRITE"
note "the human clicks Allow in Claude Code; the edit runs and Crane verifies its actual effect:"
printf '%s' "$FIXED" > services/payments/pay/service.py
hook post-tool-use "$WRITE"
TEST='import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "services", "payments"))
from pay.service import PaymentService


def test_negative_refund_is_rejected():
    try:
        PaymentService().refund(-5)
    except ValueError:
        return
    raise AssertionError("a negative refund was accepted")


if __name__ == "__main__":
    test_negative_refund_is_rejected()
'
TESTWRITE=$("$PY" -c "import json, sys; print(json.dumps({'tool_name': 'Write', 'tool_input': {'file_path': sys.argv[1], 'content': sys.argv[2]}}))" "$ROOT/tests/test_refund.py" "$TEST")
note "7c. it adds a test (marked agent-authored; it can never satisfy the contract by itself):"
hook pre-tool-use "$TESTWRITE"
printf '%s' "$TEST" > tests/test_refund.py
hook post-tool-use "$TESTWRITE" > /dev/null
note "7d. the agent stops; Crane validates the whole session:"
hook stop '{"stop_hook_active": false}'
crane task sync PAY-1821 > /dev/null
state
crane autonomy status claude-task-PAY-1821-v1 | sed -n 1,4p

step "8. Delivery: final contract tests, repository tests, checks, pull request, Slack"
crane deliver run claude-task-PAY-1821-v1
state
note "the pull request body (contract summary + attestation):"
sed -n 1,30p .crane/runtime/delivery/claude-task-PAY-1821-v1/pull_request.md

step "9. Approvals: payments-lead clicks Approve in Slack, security-lead approves from the CLI"
SLACK_MESSAGE=$(ls .crane/runtime/delivery/outbox/*-slack.json | head -1)
VALUE=$("$PY" -c "import json, sys; message = json.load(open(sys.argv[1])); print([b for b in message['request']['blocks'][1]['elements'] if b['action_id'] == 'approve'][0]['value'])" "$SLACK_MESSAGE")
echo "  Slack buttons: $("$PY" -c "import json, sys; message = json.load(open(sys.argv[1])); print(', '.join(b['text']['text'] for b in message['request']['blocks'][1]['elements']))" "$SLACK_MESSAGE")"
"$PY" - "$VALUE" "$CRANE_SLACK_SECRET" > "$PLAY/slack.sh" <<'EOF'
import hashlib, hmac, json, sys, time, urllib.parse
value, secret = sys.argv[1], sys.argv[2]
payload = {"type": "block_actions", "user": {"id": "U0PAYLEAD"}, "actions": [{"action_id": "approve", "value": value}]}
body = "payload=" + urllib.parse.quote(json.dumps(payload), safe="")
timestamp = str(int(time.time()))
signature = "v0=" + hmac.new(secret.encode(), f"v0:{timestamp}:{body}".encode(), hashlib.sha256).hexdigest()
open("slack-request.txt", "w").write(body)
print(f"TIMESTAMP={timestamp} SIGNATURE={signature}")
EOF
eval "$(cat "$PLAY/slack.sh")"
note "the click as Slack would POST it to /slack/actions, signed with the signing secret:"
crane deliver slack-action --body "$ROOT/slack-request.txt" --timestamp "$TIMESTAMP" --signature "$SIGNATURE" | grep -E "merge rule|waiting|ELIGIBLE"
note "a forged click is refused:"
crane deliver slack-action --body "$ROOT/slack-request.txt" --timestamp "$TIMESTAMP" --signature "v0=forged"; echo "  -> exit $?"
crane deliver approve claude-task-PAY-1821-v1 --approver security-lead --reason "reviewed the refund guard" | grep -E "merge rule|ELIGIBLE"

step "10. Merge, new trusted state, task completion, Jira closed"
crane deliver merge claude-task-PAY-1821-v1 --by release-lead | grep -E "MERGED|journal"
git log --oneline --graph -6
echo "  trusted checkpoint: $(cat .crane/checkpoints/trusted_claude_task_PAY_1821_v1.json | tr -d '\n ')"
state
crane task status PAY-1821 --json | "$PY" -c "import json, sys; print('  lifecycle:', ' -> '.join(entry['to'] for entry in json.load(sys.stdin)[0]['history']))"
note "queued for Jira only after the verified merge, as one idempotent completion event:"
for message in .crane/runtime/delivery/outbox/*-jira.json; do
  "$PY" -c "import json, sys; m = json.load(open(sys.argv[1])); print('   ', m['request']['method'], m['request']['path'], '(' + m['request']['completion_event'] + ')')" "$message"
done
crane task completions list
note "the dispatcher completes the Jira issue (status check, comment, transition, check); sending again changes nothing:"
crane task completions send
crane task completions send
"$PY" -c "import json, sys; s = json.load(open(sys.argv[1])); print('  Jira PAY-1821:', s['status'], '-', len(s['comments']), 'comment,', len(s['transitions']), 'transition')" "$JIRA/jira-state.json"
state
crane task status PAY-1821 --json | "$PY" -c "import json, sys; print('  lifecycle:', ' -> '.join(entry['to'] for entry in json.load(sys.stdin)[0]['history']))"
note "the task contract is retired; the permanent policy stays:"
ls .crane/policies .crane/retired

note "the golden path for the task, one stage and every layer:"
crane flow status PAY-1821 | sed -n 1,4p

step "11. Evidence: the session's attestation and trail, from the CLI and the control plane"
crane session inspect claude-task-PAY-1821-v1 | sed -n '/TIMELINE/,$p'
crane dashboard api GET /api/attestations | "$PY" -c "import json, sys; [print('  attestation', a['session'], a['decision'], 'merged as', a['merged']) for a in json.load(sys.stdin)['attestations']]"
echo
echo "Done. Explore it yourself: cd \"$ROOT\" && \"$CRANE\" dashboard"
