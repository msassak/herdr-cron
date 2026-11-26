# Future plans

Notes for work after the [MVP](herdr-cron.md).

## Write a complete README

- **Installation:** Walk through installing Herdr Core and `hc`, setting up Rust with mise, installing `herdr-cron`, and verifying it is on `PATH`.
- **Getting started:** Create a crontab, preview the next run with `check`, start `serve`, attach to a job, and inspect its result with `history`.
- **How it works:** Explain what `herdr-cron` owns versus what Core owns, where state lives, how overlap and missed runs work, and what restart recovery can and cannot guarantee.
- **Development:** Cover mise, the build and lint commands, all three test tasks, and how the end-to-end test isolates Core.

## Decide how to run the supervisor

`herdr-cron serve` can already run outside `hc`; it calls `hc` to start and inspect jobs. Running the supervisor inside `hc` makes the supervisor itself attachable, not boot-persistent. Decide which launch method to recommend by default: foreground for setup, an `hc` terminal for manual use, or `launchd`/`systemd` for unattended startup. Check how each reaches the same Core session and where its logs go.

## Define crontab compatibility

Keep the current five-field format as the baseline. Document the exact supported syntax and decide which gaps are worth closing: environment assignments, `@daily`-style schedules, and `%` input handling. Decide whether features such as per-job working directories or stable names belong in crontab syntax or a separate config. Add compatibility tests for any new syntax.

## Explore a Herdr desktop mod

Start with a read-only view: show upcoming runs, live jobs, and recent results; let users attach to a running terminal. Decide what structured CLI output the mod needs. Keep the scheduler independent of the app so closing the app does not stop scheduling. Defer editing schedules, triggering jobs, or managing the supervisor until the read-only flow is useful.
