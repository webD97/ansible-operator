from __future__ import annotations

import json

from ansible.plugins.callback import CallbackBase

DOCUMENTATION = """
callback: ansible_operator_recap
type: notification
short_description: Emits a machine-readable per-host outcome summary for ansible-operator.
description:
  - Hooks the same playbook-stats event the default callback uses, without replacing it, so
    human-readable stdout is unaffected.
  - Prints one delimited JSON block at the end of the run. ansible-operator's reconciler parses
    it out of the Job pod's logs to derive per-host outcomes, since one Job can now span many
    hosts and its own exit code no longer maps to any single host's result.
requirements:
  - Enabled via ANSIBLE_CALLBACKS_ENABLED (this callback sets CALLBACK_NEEDS_ENABLED).
"""

MARKER_START = "===ANSIBLE-OPERATOR-RECAP-START==="
MARKER_END = "===ANSIBLE-OPERATOR-RECAP-END==="


class CallbackModule(CallbackBase):
    CALLBACK_VERSION = 2.0
    CALLBACK_TYPE = "notification"
    CALLBACK_NAME = "ansible_operator_recap"
    CALLBACK_NEEDS_ENABLED = True

    def v2_playbook_on_stats(self, stats):
        processed = {}

        for host in stats.processed.keys():
            summary = stats.summarize(host)
            processed[host] = {
                "ok": summary.get("ok", 0),
                "changed": summary.get("changed", 0),
                "unreachable": summary.get("unreachable", 0),
                "failed": summary.get("failures", 0),
                "skipped": summary.get("skipped", 0),
                "rescued": summary.get("rescued", 0),
                "ignored": summary.get("ignored", 0),
            }

        payload = {"processed": processed}

        self._display.display(MARKER_START)
        self._display.display(json.dumps(payload))
        self._display.display(MARKER_END)
