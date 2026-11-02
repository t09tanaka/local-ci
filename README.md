# local-ci

Run the GitHub Actions jobs your branch would trigger, on your machine, and get
a short report back.

```
local-ci run          # what a pull request of these changes would trigger
local-ci run --all    # every check a pull request can trigger
```

`local-ci` reads `.github/workflows/`, works out which workflows a pull request
from this branch would start (triggers, `branches`, `paths` / `paths-ignore`),
and runs their jobs in a throwaway `git worktree` of the commit. It prints one
line per job and, for a failure, the end of the failing step's log. Full logs
and a JSON result are kept on disk.

It is meant for the moment before you push, and for coding agents that need to
know "would CI pass?" without reading workflow files and logs themselves.

With [`examples/demo`](examples/demo) and a change that breaks one test shard:

```
$ local-ci run --no-status
local-ci 0.1.0 · 350924e vs main · 1 file changed · triggered by the changes · logs ~/Library/Caches/local-ci/runs/demo-2daacfc9/1793539365-350924e
setup  ok  1 step 0s
PASS   ci.yml › lint  0s
PASS   ci.yml › discover  0s
PASS   ci.yml › test (1)  0s
FAIL   ci.yml › test (2) › Unit tests (shard 2) (exit 1)  0s
PASS   ci.yml › integration  3s
SKIP   deploy (needs failed)

── ci.yml › test (2) › Unit tests (shard 2) (exit 1)
   expected 5, got 4
   log: ~/Library/Caches/local-ci/runs/demo-2daacfc9/1793539365-350924e/ci-test-2/03-unit-tests-shard-2.log

result: failure · pass 4 / fail 1 / error 0 / skip 1 · 3s
status: not posted (--no-status)
```

It is not a GitHub Actions runner. Jobs run directly on your machine (not in
Ubuntu containers), with the tools you have installed. What cannot be
reproduced locally is skipped and listed under `not covered` instead of being
reported as a pass.

## Install

```
cargo install local-ci
```

Or download a binary from the [releases](https://github.com/t09tanaka/local-ci/releases).

## Commands

| Command | What it does |
|---|---|
| `local-ci plan` | Show the workflows, units and setup steps that would run. Runs nothing. |
| `local-ci run` | Run them and report. Exit code 0 = success, 1 = failure, 2 = error (could not check everything), 130 = interrupted. |
| `local-ci status` | Record the last run's result as the `local-ci` commit status (for a commit you have pushed since). |

Common options (`plan` and `run`):

| Option | |
|---|---|
| `--base <ref>` | Compare against this ref. Default: origin's default branch. |
| `--sha <rev>` | Test this commit. Default: `HEAD`. Uncommitted changes are never tested. |
| `--all` | All checks: ignore `paths` / `paths-ignore`, and run workflows that are disabled on GitHub. `branches` and `types` still apply, so a workflow that only runs on pushes to `main` stays out. |
| `--include-disabled` | Also run workflows that are disabled on GitHub, with `paths` / `paths-ignore` still applied. `--all` implies it. |
| `-w, --workflow <file or name>` | Only these workflows. |
| `-j, --job <id>` | Only these jobs, plus the jobs they `need`. |
| `-C <dir>` | Repository directory. |

`run` also takes `--parallel <n>`, `--no-canirun`, `--no-status`, `--keep`
(keep the worktree), `--tail <lines>`, `--job-timeout <minutes>` and `--json`.

## How a run works

1. **Worktree.** The commit is checked out with `git worktree add --detach`
   under the cache directory. Your working tree is never touched, so steps such
   as `git add -A`, `cp .env.example .env` or generators that rewrite tracked
   files are harmless. The worktree is removed afterwards.
2. **Setup, once.** In CI every job runs `npm ci` on a fresh VM. Locally all
   jobs share one checkout, and `npm ci` deletes `node_modules` before it
   installs, so parallel jobs would break each other. Install steps
   (`npm ci`, `pip install`, `flutter pub get`, `npx prisma generate`, ...)
   and steps that several jobs repeat word for word at their start are run
   once, before any job, and skipped in the jobs. Installs that happen later
   anyway (a matrix job, a script that calls `npm ci`) go through an `npm`
   wrapper on `PATH`: a directory is installed once per run and lockfile, and
   later `npm ci` / `npm install` there are no-ops.
3. **Units in parallel.** Jobs linked by `needs:` (or writing the same build
   output) form a unit and run in order. Units run in parallel. Units that
   start services or containers, or look like integration / e2e tests, are
   *exclusive*: they hold a per-repository lock, so they never overlap, in this
   run or in another `local-ci` run of the same repository.
4. **Report.** One line per job, the tail of each failing step, where the logs
   are, and the overall state: `success`, `failure` or `error`.
5. **Commit status.** The state is posted as the `local-ci` commit status with
   `gh` when the commit is on GitHub. Before you push it is not; run
   `local-ci status` after pushing. `--no-status` skips this.

## What is supported

- Triggers `pull_request` / `pull_request_target` (with `types`, `branches`,
  `paths`, `paths-ignore`) and `push`. A run pretends to be a non-draft pull
  request into the base branch. `workflow_dispatch`, `schedule` and the rest
  are reported as not triggered, and so are workflows disabled on GitHub (read
  with `gh` when it is available) unless `--include-disabled` or `--all` is given.
- `run:` steps with `bash` (GitHub's `bash --noprofile --norc -eo pipefail`),
  `sh`, `python` or a custom `shell: ... {0}`; `working-directory`, `env`,
  `if:`, `continue-on-error`, `timeout-minutes`.
- Expressions: contexts `github`, `env`, `matrix`, `steps`, `needs`, `job`,
  `runner`, `strategy`, `inputs`, `vars`; operators; `success()`, `failure()`,
  `always()`, `cancelled()`, `contains`, `startsWith`, `endsWith`, `format`,
  `join`, `toJSON`, `fromJSON`, `hashFiles` (always empty). Anything else is an
  error, never a silent `false`.
- `strategy.matrix` with `include` / `exclude`, matrices computed from
  `needs.*.outputs`, `fail-fast`.
- `$GITHUB_OUTPUT`, `$GITHUB_ENV`, `$GITHUB_PATH`, `$GITHUB_STEP_SUMMARY`; job
  `outputs`.
- `services:` with Docker. Each container gets a free host port, and
  `localhost:<port>` / `127.0.0.1:<port>` in the job's `env` and `run:`
  scripts (and `*PORT*` variables holding the port) are rewritten to it, so a
  database you already run on 5432 is not in the way. `--health-cmd` is waited
  for.
- `uses:` — `actions/checkout`, caches and artifacts are skipped; `setup-node`,
  `setup-python`, `setup-go`, `setup-java`, `setup-ruby`, `flutter-action`,
  `setup-dart` and `rust-toolchain` check that the tool is installed and warn
  when its version differs. Other actions are skipped and listed under
  `not covered`.

Not covered: steps that read `secrets.*` or `github.token`, steps that push,
publish or deploy (`git push`, `gh release`, `npm publish`, `aws`, `kubectl`,
`terraform apply`, ...), steps that install system packages (`sudo`,
`apt-get`, `brew install`, ...), `container:` jobs and reusable workflows. A
`git push` from inside a script is pointed at a URL that does not exist. `pip install` goes into a virtualenv of the run and `npm install -g`
into a prefix of the run, so neither changes your machine.

## Working with other tools

- **[canirun](https://github.com/t09tanaka/canirun)**: when it is installed,
  every `run:` step outside setup is wrapped in `canirun exec`, so heavy steps
  wait for CPU instead of piling up. Timeouts start when the step gets its turn.
- Ports used by a workflow's own `docker compose` steps are not rewritten;
  exclusive units take the per-repository lock instead.

## Files

The cache directory is `~/Library/Caches/local-ci` on macOS and
`$XDG_CACHE_HOME/local-ci` (or `~/.cache/local-ci`) elsewhere; set
`LOCAL_CI_HOME` to move it. It holds the worktrees while they run, the last 20
runs of each repository (`runs/<repo>/<time>-<sha>/` with one log per step and
`result.json`), and the locks.

## License

MIT
