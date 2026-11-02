# Changelog

## Unreleased

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
