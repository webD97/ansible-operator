from __future__ import annotations

import json

from ansible.plugins.callback import CallbackBase

DOCUMENTATION = """
callback: ansible_operator_recap
type: notification
short_description: Emits a machine-readable per-host outcome summary for ansible-operator.
description:
  - Hooks the same playbook-stats event the default callback uses, without replacing it, so
    human-readable stdout (the PLAY RECAP) is unaffected.
  - At end of run, writes a compact JSON map to the container's termination-message file
    (/dev/termination-log). ansible-operator reads it back from the finished container's
    terminated state instead of scraping logs, since one Job can span many hosts and its own
    exit code no longer maps to any single host's result.
  - 'Format: {"<host>": [ok, changed, unreachable, failed, skipped, rescued, ignored], ...} —
    a fixed-order array per host, no spaces, to stay well under the kubelet''s message size cap.'
requirements:
  - Enabled via ANSIBLE_CALLBACKS_ENABLED (this callback sets CALLBACK_NEEDS_ENABLED).
"""

# Default terminationMessagePath; the kubelet surfaces this file's contents as the container's
# state.terminated.message once it exits.
TERMINATION_LOG_PATH = "/dev/termination-log"


class CallbackModule(CallbackBase):
    CALLBACK_VERSION = 2.0
    CALLBACK_TYPE = "notification"
    CALLBACK_NAME = "ansible_operator_recap"
    CALLBACK_NEEDS_ENABLED = True

    def v2_playbook_on_stats(self, stats):
        # Fixed wire order — must stay in lockstep with HostStats::from([u32; 7]) on the reader.
        recap = {}
        for host in stats.processed.keys():
            s = stats.summarize(host)
            recap[host] = [
                s.get("ok", 0),
                s.get("changed", 0),
                s.get("unreachable", 0),
                s.get("failures", 0),
                s.get("skipped", 0),
                s.get("rescued", 0),
                s.get("ignored", 0),
            ]

        try:
            with open(TERMINATION_LOG_PATH, "w") as f:
                f.write(json.dumps(recap, separators=(",", ":")))
        except OSError:
            # Best-effort: if the file can't be written, the operator sees an empty termination
            # message and treats every host as Unknown (same as a hard crash before this hook).
            pass
