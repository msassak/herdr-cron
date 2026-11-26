# herdr-cron plan

**Status:** MVP implemented.

Build a standalone cron-like supervisor that uses `hc` to launch scheduled programs in herdr-core terminals and collect their results. **No changes to `hc` or herdr-core.** The supervisor owns scheduling and run history; core owns terminals, processes, and attachment.

## MVP

- `herdr-cron check <crontab>` validates the file, reports line-numbered errors, and prints upcoming fire times.
- `herdr-cron serve <crontab>` loads the schedule, dispatches due jobs, and collects results.
- `herdr-cron history` lists recent runs with their scheduled time, outcome, exit code when available, and a bounded output tail.

Use common user crontab syntax: five time fields followed by a shell command, with blank lines and comments. Use a parser that implements the traditional day-of-month/day-of-week matching rule. Run each command through `/bin/sh -c` so pipes, redirects, and `&&` work. Report unsupported cron features such as `@reboot` and unescaped `%` rather than silently interpreting them differently.

```cron
# Weekday repository sync at 08:30
30 8 * * 1-5 cd "$HOME/Work/my-repo" && ./scripts/sync

# Health check every 15 minutes
*/15 * * * * "$HOME/.local/bin/check-services"
```

## Scheduling policy

- Evaluate schedules in the selected timezone, defaulting to local time. Use the UTC scheduled instant in occurrence IDs. Test and document the rules for skipped and repeated local times at daylight-saving transitions.
- **No catch-up after downtime.** Schedule future occurrences and log the downtime gap; do not fire a backlog.
- **Skip overlapping runs of the same job** and record the skip. This is an intentional difference from traditional cron. Different jobs may run concurrently.
- Do not automatically retry failed or ambiguous runs. Give each job a stable ID so unrelated edits or reordered lines do not change its identity.

## Run lifecycle

1. **Record before launching.** Keep a small SQLite ledger with one row per `(job ID, scheduled UTC instant)`. Persist a `launching` row before calling `hc run --bg --json`. Add scheduler and occurrence labels at launch and save the returned terminal address. Labels belong to the **hc terminal**, not the underlying OS process. Use `--outlive-parent` so accepted jobs can survive a scheduler restart, and `--no-notify` so completion notes are not typed into the scheduler's terminal.
2. **Collect after exit.** Use `hc events` as a wake-up signal, then confirm the terminal has finished and read its recorded exit code. A coding agent's `done` status is not process exit. Persist timestamps, outcome, and a bounded `hc read` tail, indicating possible truncation. This is a terminal transcript, not byte-exact stdout and stderr. Persist the result before `hc rm`; don't use `--rm` at launch. Only clean up terminals owned by this scheduler.
3. **Reconcile after restart.** Use `hc ls --all --label …` to adopt running or finished terminals whose occurrence labels match the ledger, including launches that succeeded before their addresses were recorded. If a `launching` row has no match and the launch outcome cannot be proved, record `unknown` rather than relaunching it. SQLite and `hc run` do not share a transaction; preventing duplicate side effects may cost a missed occurrence. Reconcile again if the event stream reconnects.

## Running the supervisor

It can run directly in a shell or in its own attachable hc terminal:

```sh
hc run --bg --no-parent --name herdr-cron -- \
  herdr-cron serve "$HOME/.config/herdr-cron/crontab"
```

Running under `hc` does **not** make the scheduler boot-persistent. `--keep` restores a shell, not the original command. If unattended startup after reboot is needed, use `launchd` or `systemd` to supervise `hc-cron`.

## First milestone

Use an isolated hc session and scratch state. Schedule one short job that emits output and exits, and another long enough to attach to. Stop and restart only the scheduler. Verify that it adopts the running terminal, collects both results, and does not launch either occurrence twice. Then cover a nonzero exit, overlap, and a crash after launch but before the terminal address is saved.

**Defer:** Remote targets, retries, catch-up policies, hot reload, byte-exact log capture, and a desktop mod.
