# Changelog

## 0.5.0

- `.local-ci.toml` at the repository root, read from the commit under test like
  the workflows. `workflows-dirs = [".ci/workflows"]` reads the workflows from
  those directories instead of `.github/workflows`, so a repository can move
  its workflows where GitHub Actions does not look and run its checks with
  local-ci alone. `--workflows-dir` still adds to them. The workflows of the
  first directory are named by their file name in the output, as those of
  `.github/workflows` were. An absolute path, a path outside the repository,
  an empty list, a directory with no `.yml` / `.yaml` file at the commit, and
  an unknown key are errors. Without the file nothing changes.

## 0.4.1

- Worktree copies are registered one at a time per repository, across runs.
  When many units made their copies at the same moment, `git worktree add`
  read another copy's half-written entry and the unit ended in `error`
  ("failed to read .git/worktrees/.../commondir").

## 0.4.0

- A `services:` host port that the workflow never names (no `localhost:<port>`,
  no `*_PORT` value, no `${{ job.services... }}`) is bound as declared instead
  of moved to a free port. The code reaching the service by that port as its
  default could not find it before.
- Exclusive jobs no longer bind the fixed ports a workflow writes down. A number
  in a `*_PORT` variable of the job's `env`, or `TEST_DB_PORT=15432` in a
  script, is given a free port and rewritten everywhere the job uses it.
  `KEY_PORT=<port>` in a script is now rewritten for `services:` ports too.
- `docker compose -p <name>` and `COMPOSE_PROJECT_NAME=<name>` (in a script or
  in `env:`) in exclusive jobs get a suffix of the run, so the clean-up no
  longer stops a project of the same name that you started yourself. Only a
  `-p` before the subcommand counts, so `docker compose exec db mkdir -p tmp`
  is left alone, and every `docker compose` command of a line is seen, not just
  the first.
- With portool and a `.portool.toml`, exclusive jobs without `services:` get a
  port block of their own copy (`.env.portool` in their environment) and give
  it back afterwards; the block portool's hook takes for the run's worktree is
  given back too. Jobs with `services:` get none: their database is the
  service, and a script that honours portool's ports would look for its own.
  `--no-portool` turns it off.
- With canirun, the entries of a matrix run at once, each in its own copy of
  the workspace, instead of one after another, up to `strategy.max-parallel`.
  With `fail-fast`, once an entry fails or errors the others skip their
  remaining steps (`if: always()` steps still run) and are reported as skipped.
  Entries whose names come out the same get ` #<n>`, so their logs stay apart.
- With portool and a `.portool.toml`, exclusive jobs without `services:` no
  longer take the per-repository lock, so integration and e2e jobs that start
  their own containers run beside each other and beside another run of the
  same repository. Units with a `services:` job keep the lock. A job that still pins something
  rewriting cannot reach takes the lock: a Compose file with `container_name:`
  or a literal host port, a Compose file named by a variable, or
  `docker run --name` / `-p <host port>:`.
- `--containers <n>` (default 2): exclusive jobs that may run at once on the
  machine, across all runs, so their databases do not pile up while the tests
  wait for CPU.
- Jobs whose scripts run Playwright, Cypress or `test:e2e` / `test:int` are
  exclusive even when their name does not say so.
- A step whose `canirun` call failed on its ledger lock timeout, before the
  command started, is tried again (up to three times).

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
