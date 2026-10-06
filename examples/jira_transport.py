#!/usr/bin/env python3
"""Jira transport for Crane's task completion dispatcher.

Crane makes no network calls itself. When it completes a Jira issue after a verified merge, it
runs the command configured in .crane/delivery.json:

    "trackers": {"jira": {
        "done_transition": "31",
        "base_url": "https://acme.atlassian.net",
        "transport": ["python3", "examples/jira_transport.py"]
    }}

For each call it writes {"method", "url", "path", "body"} to this program's stdin and reads
{"status", "body"} from its stdout. A non-zero exit means Jira could not be reached; Crane keeps
the completion event and retries later with backoff.

Credentials come from the environment: JIRA_EMAIL and JIRA_API_TOKEN (Jira Cloud basic auth).
"""

import base64
import json
import os
import sys
import urllib.error
import urllib.request


def main():
    request = json.load(sys.stdin)
    token = base64.b64encode(
        f"{os.environ['JIRA_EMAIL']}:{os.environ['JIRA_API_TOKEN']}".encode()
    ).decode()
    body = request.get("body")
    call = urllib.request.Request(
        request["url"],
        data=None if body is None else json.dumps(body).encode(),
        method=request["method"],
        headers={
            "Authorization": f"Basic {token}",
            "Accept": "application/json",
            "Content-Type": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(call, timeout=30) as response:
            status, text = response.status, response.read().decode()
    except urllib.error.HTTPError as error:
        status, text = error.code, error.read().decode()
    except (urllib.error.URLError, TimeoutError) as error:
        sys.stderr.write(f"Jira unreachable: {error}\n")
        return 7
    try:
        parsed = json.loads(text) if text else None
    except json.JSONDecodeError:
        parsed = {"text": text}
    print(json.dumps({"status": status, "body": parsed}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
