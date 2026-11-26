# herdr-cron

Run scheduled commands in attachable Herdr Core terminals. `herdr-cron` is a standalone supervisor; it does not modify `hc` or your system crontab.

## Get started

Requires macOS or Linux, `hc` on `PATH`, and a Rust toolchain and C compiler to build (SQLite is bundled). Tested with `hc` 0.1.22.

```sh
mise install
mise exec -- cargo install --locked --path .
```

Write a crontab, for example `~/.config/herdr-cron/crontab`:

```cron
# Run a backup on weekdays at 08:30
30 8 * * MON-FRI cd "$HOME/Work/my-repo" && ./backup
*/15 * * * * "$HOME/.local/bin/check-services"
```

```sh
herdr-cron check ~/.config/herdr-cron/crontab
herdr-cron serve ~/.config/herdr-cron/crontab
herdr-cron history --limit 10
```

To keep the supervisor in its own attachable terminal:

```sh
hc run --bg --no-parent --name herdr-cron -- \
  herdr-cron serve "$HOME/.config/herdr-cron/crontab"
```

`serve` stays in the foreground and reads the file once. Restart it to apply changes. Jobs already started survive a supervisor exit. Use `launchd` or `systemd` if the supervisor must start after a reboot; `hc --keep` does not rerun programs.

## Crontab rules

- Five time fields followed by a `/bin/sh -c` command. Blank lines and full-line comments are ignored. Lists, ranges, steps, month and weekday names, and Sunday as `0` or `7` are supported. When both day fields are restricted, either may match.
- Timezone comes from `--timezone`, then `TZ` (an IANA name, such as `America/New_York`), then the system timezone. A skipped local time never fires. A repeated local time can fire twice, once per UTC instant, subject to overlap policy.
- No special `@` schedules, crontab environment assignments, or unescaped `%` stdin syntax. Use `\%` for a literal `%`. Identical job lines are rejected. A job's schedule fields and command define its identity, independent of line order. Editing either creates a new job identity.
- Jobs start in `$HOME` unless `serve --cwd` is set, and get Herdr Core’s environment. Keep credentials out of commands: the command text and terminal output are stored in local history.

## Runs and recovery

- Different jobs may run concurrently. A job is skipped if its previous terminal is still live. Missed minutes are not replayed. If Core cannot be reached, dispatch pauses until it can inspect existing runs again.
- State lives in `$XDG_STATE_HOME/herdr-cron` or `~/.local/state/herdr-cron`. Use `--state-dir` for an independent schedule and keep it pointed at the same Core session when restarting. Only one supervisor can serve a state directory at a time.
- The supervisor records each launch before asking Core to start the job. On restart, it matches labelled terminals to the ledger. A launch that may have happened but cannot be confirmed is marked `unknown`, never reissued.
- `history` shows the terminal ID, exit result and output tail (up to 1,000 scrollback lines plus the screen, capped at 64 KiB). It is not a raw stdout/stderr log. The supervisor removes finished terminals only after saving the result. Run history is not automatically pruned.

## Tests

```sh
mise exec -- cargo test --locked
mise exec -- cargo test --locked real_core_recovers_and_collects_two_runs -- --ignored
```

The second test creates and stops its own Core instance with a private socket and scratch state.
