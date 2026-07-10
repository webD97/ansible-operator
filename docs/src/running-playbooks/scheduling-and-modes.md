# Scheduling and execution modes

Two independent things decide *when* a plan runs and *what* runs:

- the **schedule** (and time zone) decides at which wall-clock times a run may fire;
- the **execution mode** plus **drift detection** decide which hosts actually execute when a run
  does fire.

## Schedule

`spec.schedule` is a standard **5-field cron** expression (`minute hour day-of-month month
day-of-week`). `spec.timeZone` is the IANA time zone it is evaluated in; if omitted, **UTC** is
assumed. A run fires inside a short time window around each scheduled tick, so an exact match to the
second is not required — but the granularity is minutes, not seconds.

```yaml
spec:
  schedule: "0 3 * * *"      # 03:00 every day
  timeZone: Europe/Berlin    # ...in Berlin local time (honours DST)
```

**Omitting `schedule`** means "eligible to run as soon as possible" rather than "never": the plan is
not gated on a clock and will run when its hosts are out of date. Use an explicit schedule when you
want runs pinned to a maintenance window.

The plan's `.status.nextRun` shows the next computed fire time, and the `Next run` printer column
surfaces it in `kubectl get playbookplan`.

## Execution modes

`spec.mode` is one of:

### `OneShot` (default)

Converge to a goal state and then stop. Only **out-of-date** hosts run; once every host has
succeeded on the current playbook/inputs, the plan settles into `Succeeded` (or `Failed` if some
host could not be brought current) and goes quiet — it does **not** keep re-running on the schedule.
It wakes again only when the inputs change (see drift detection below). Good for "make it so": apply
a configuration or a one-time migration and confirm every host got it.

### `Recurring`

Re-apply on **every** schedule tick. *All* hosts run each time, regardless of whether they ran
successfully last time, and the plan reschedules itself back to `Scheduled` for the next tick. Good
for periodic enforcement or inherently repeating work: nightly package upgrades, drift correction,
health tasks. A `Recurring` plan effectively always needs a `schedule`.

## Drift detection (the execution hash)

To decide which hosts are "out of date", the operator computes an **execution hash** over the
playbook text **plus the contents of every referenced Secret** (variables and files) —
order-insensitive, so reordering inputs does not count as a change. It deliberately excludes the
internally rendered workspace (whose content, e.g. proxy pod IPs, legitimately changes every run).

- Each host records the hash it **last succeeded on** (`.status.hostsStatus.<host>.lastAppliedHash`).
- A host whose last-applied hash equals the current hash is **current** and is skipped (in
  `OneShot`).
- When you edit the playbook, or change a referenced variables/files Secret, the hash changes: the
  plan resets to `Pending`, clears its retry bookkeeping, and every host becomes out of date again.

This is what makes `OneShot` idempotent and cheap: editing an unrelated field does not re-run
everything, but a real change to the playbook or its inputs does. The current hash is visible as
`.status.currentHash` and in the `Current hash` printer column.

## Retries and adoption

Within a single hash, if a run's Job needs to be retried the operator numbers successive Jobs
(`apply-<plan>-<id>-<n>`) rather than colliding on one name — the playbook and inputs are unchanged,
so the hash alone cannot distinguish attempts. You generally do not interact with this; it is why
you may see more than one Job object for the same logical run.

## Cleaning up finished Jobs

`spec.ttlSecondsAfterFinished` controls how long a finished run's Job and its pod linger before
Kubernetes' TTL controller reaps them (values below 60 seconds are raised to 60). Set it higher if
you want more time to inspect a finished pod, lower to reclaim resources sooner. The recap the
operator needs is captured from the pod's termination message at completion, so reaping the pod does
not lose your `.status` results.
