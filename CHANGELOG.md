# Changelog

## Unreleased

- Library: planning and executing are separate. `build_frozen_plan` returns a
  `FrozenPlan` (serde, schema version and sha256 digest, no host paths) and
  `execute_plan` runs it in a given `Workspace`; `run` composes them with the
  results cache, the worktree and the saved result. The process environment is
  read only by the binary: the library takes an `ExecContext` (env map, cancel
  handle). Process and container tracking for cancelling moved from
  process-global statics into the context's `CancelHandle`, so concurrent runs
  do not stop each other. No change to the command line.
- Library: a plan is cut into a job graph (`needs` as edges; a matrix as one
  node per variant plus the node later jobs depend on) and tasks that can be
  placed on a host: a job, or a matrix variant, each in a checkout of its own,
  except jobs that write the same files, which stay one task with the reason
  recorded. `execute_task` runs one task from the results and outputs of the
  jobs it needs (`TaskInputs`) and returns a `TaskResult`; a matrix job's
  outputs are merged in matrix order, not in the order the variants finish.
  The plan carries its own wire types (`wire`, schema version 2) instead of the
  engine's, and records the disabled-workflow list it was made with;
  `build_frozen_plan_with` takes that list so planning needs no network.
  `ExecContext` is no longer `Clone` (`fresh_attempt` makes a second one), and
  the docker, compose and python commands run with the context's environment.
  A workflow with a dependency cycle, a `needs` of a job that does not exist or
  a matrix with two keys that print alike is left out of the plan and listed
  under "not triggered" with the reason, as GitHub does not start it; the other
  workflows are planned and run. A job cancelled while it ran counts as
  `cancelled` for the jobs that need it (`cancelled()`, `needs.<id>.result`),
  and a matrix is `failure` if any entry failed, else `cancelled` if any was.
  No other change to the command line.

## 0.7.0

- Each job that ran has a `time` in `result.json` and `--json`: how long its
  steps ran (`work_secs`) and how long it waited for the repository lock, a
  container slot, canirun and a `--parallel` place (`wait_lock_secs`,
  `wait_slot_secs`, `wait_queue_secs`, `wait_parallel_secs`). The report names
  the waits of a job that waited a second or more. The step's own time
  (`secs`) is as before. With canirun, a `bash` or `sh` step's script now
  begins by noting the moment it got its turn (on its own first line, so the
  command canirun sees and the line numbers are unchanged); a step that prints
  nothing no longer counts as still waiting. Steps of other shells still
  count as waiting until their first line of output.

## 0.6.2

- Units' copies clone `node_modules` again, as up to 0.5.1. Linking to
  setup's packages (0.6.0) is now opt-in with `link-node-modules = true` in
  `.local-ci.toml`. Tools that follow a link to where the package really is
  and then refuse a path outside the project broke with it: after
  TypeScript emitting declarations (0.6.1), a Next.js build with Turbopack
  did not find `next` outside its workspace root.

## 0.6.1

- A project whose `tsconfig*.json` (or one it extends by a relative path)
  sets `declaration` or `composite` gets a copy of its `node_modules` again
  instead of links. TypeScript follows a link to where the package really
  is, outside the unit's copy, and a declaration that would have to name
  that path failed with TS2742 ("cannot be named without a reference to
  ...") even under `tsc --noEmit`.

## 0.6.0

- A unit whose jobs passed in the last 24 hours is not run again when
  nothing it depends on has changed: the files its workflow's `paths` (or all
  but its `paths-ignore`) cover, every file at the repository root, the
  workflows and local actions, the merge base and branch, the versions of
  Node and of the tools its steps name, and local-ci itself. This trusts
  `paths` to name what the workflow depends on. The report marks such jobs
  `cached (abc1234, 2h ago)` and keeps what they did not cover; when every
  unit is reused, nothing is checked out or installed. Units running
  `npm audit` or other advisory checks, also from a `package.json` script,
  always run, as do workflows and jobs listed in the new `no-cache` key of
  `.local-ci.toml`. `--no-cache` runs everything.
- In a unit's copy of the worktree, each `node_modules` is a directory of
  links to what is in setup's instead of a clone of it. A clone is still one
  new file per file, so a run of twenty units made and then deleted twenty
  copies of every project's dependencies (several hundred thousand files
  each time in a mid-sized monorepo), and fseventsd checked each deletion.
  Now a copy holds one link per package. Before a step that may write into
  `node_modules` (an install, `npm rebuild`, `prisma generate`, `yarn`,
  `pnpm`, `patch-package`, also from a `package.json` script or an npm
  command inside one), the copy gets its own. A `node_modules` linking out of
  itself (workspaces, `file:` dependencies), `.dart_tool` and everything else
  is cloned as before.
- Service containers left by a run that was killed are removed by the next
  run. One holding a service's declared port made every later run's
  services fail to start.
- `--job` works out the setup again for the jobs it keeps, instead of
  filtering it: an install could stay marked not to run while its twin that
  did was dropped.

## 0.5.1

- The copies of finished units are deleted one at a time, across runs, at the
  lowest CPU priority (`nice -n 19`; the turns are taken with `lockf` on
  macOS). Many units ended at once and a dozen `rm -rf` of copies holding
  `node_modules` ran side by side, taking several cores, and as many again in
  fseventsd and the kernel, from the jobs still running.

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
