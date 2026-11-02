# Changelog

## Unreleased

- Exclusive jobs no longer bind the fixed ports a workflow writes down. A number
  in a `*_PORT` variable of the job's `env`, or `TEST_DB_PORT=15432` in a
  script, is given a free port and rewritten everywhere the job uses it.
  `KEY_PORT=<port>` in a script is now rewritten for `services:` ports too.
- `docker compose -p <name>` and `COMPOSE_PROJECT_NAME=<name>` in exclusive
  jobs get a suffix of the run, so the clean-up no longer stops a project of
  the same name that you started yourself.
- With portool and a `.portool.toml`, exclusive jobs get a port block of their
  own copy (`.env.portool` in their environment) and give it back afterwards;
  the block portool's hook takes for the run's worktree is given back too.
  `--no-portool` turns it off.
- With canirun, the entries of a matrix run at once, each in its own copy of
  the workspace, instead of one after another. `fail-fast` skips the entries
  that have not started yet; running ones finish.
- With portool and a `.portool.toml`, exclusive units no longer take the
  per-repository lock, so integration and e2e jobs run beside each other and
  beside another run of the same repository.
- `--containers <n>` (default 2): exclusive jobs that may run at once on the
  machine, across all runs, so their databases do not pile up while the tests
  wait for CPU.

## 0.3.0

- `--workflows-dir <dir>` also reads the workflows in another directory of the
  repository, for checks that should only run locally. They are named by their
  path in the output, so a file name used in both places stays distinct.
- Each unit runs in its own copy of the worktree, cloned after setup, instead
  of all units sharing one. A job that edits tracked files for a while (a
  negative control, a formatter, mutation testing) made the jobs running beside
  it fail; `--parallel 1` was the only way around it.
- The `npm` wrapper keys its "installed once" marks by the directory relative to
  the workspace, so the copies keep the installs done in setup.

## 0.2.0

- `--include-disabled` runs workflows that are disabled on GitHub while keeping
  the `paths` / `paths-ignore` filter (`--all` already did both).

## 0.1.0

First release.

- `local-ci plan` / `local-ci run` / `local-ci status`.
- Picks workflows the way GitHub does for a pull request (`types`, `branches`,
  `paths`, `paths-ignore`, workflows disabled on GitHub), or all of them with
  `--all`.
- Runs in a throwaway worktree of the commit; install steps run once; units of
  jobs run in parallel, exclusive ones under a per-repository lock.
- Expressions, matrices (including ones computed from `needs`), file commands,
  job outputs, `services:` on free host ports, `docker compose` projects
  isolated and cleaned up.
- Wraps steps in `canirun exec` when canirun is installed.
- Short text report, JSON result, logs per step, `local-ci` commit status.
