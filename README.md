# local-ci

Run the GitHub Actions jobs your branch would trigger, on your machine, and get
a short report back.

```
local-ci run          # what a pull request of these changes would trigger
local-ci run --all    # every check a pull request can trigger
```

`local-ci` reads `.github/workflows/` (or the directories
[`.local-ci.toml`](#local-citoml) names), works out which workflows a pull request
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
| `--workflows-dir <dir>` | Also read the workflows in this directory (relative to the repository root, repeatable), besides `.github/workflows` or the directories `.local-ci.toml` names. GitHub never runs them, so this is the place for checks you only want before pushing. They are picked by the same triggers and `paths`, read from the commit like the rest, and named by their path in the output (`ci/local/mutation.yml › ...`). |
| `-w, --workflow <file or name>` | Only these workflows. |
| `-j, --job <id>` | Only these jobs, plus the jobs they `need`. |
| `-C <dir>` | Repository directory. |

`run` also takes `--parallel <n>`, `--containers <n>`, `--no-canirun`, `--no-portool`, `--no-status`, `--keep`
(keep the worktrees), `--tail <lines>`, `--job-timeout <minutes>` and `--json`.

## `.local-ci.toml`

An optional file at the repository root. Like the workflows, it is read from
the commit under test, so commit it before it takes effect.

```toml
# Read workflows from here instead of .github/workflows
workflows-dirs = [".ci/workflows"]
```

| Key | |
|---|---|
| `workflows-dirs` | Directories to read workflows from, relative to the repository root. They replace `.github/workflows`; `--workflows-dir` still adds to them. Each one must hold a `.yml` / `.yaml` file at the commit. The workflows in the first one are named by their file name in the output (`ci.yml › test`), the others by their path. |

Without the file, local-ci reads `.github/workflows` as before. An absolute
path, a path outside the repository (`..`), an empty list and an unknown key
are errors.

**CI with local-ci only.** To keep GitHub Actions from running your checks and
run them with local-ci alone, move the workflows out of `.github/workflows`
(the only place GitHub looks) and point local-ci at their new place:

```
mkdir -p .ci && git mv .github/workflows .ci/workflows
printf 'workflows-dirs = [".ci/workflows"]\n' > .local-ci.toml
git add .local-ci.toml && git commit -m "Run CI with local-ci"
local-ci run
```

The triggers, `branches` and `paths` of the moved files apply as before, and
`local-ci status` still records the result as the `local-ci` commit status.
Workflows outside `.github/workflows` are never seen as disabled on GitHub.

## How a run works

1. **Worktree.** The commit is checked out with `git worktree add --detach`
   under the cache directory. Your working tree is never touched, so steps such
   as `git add -A`, `cp .env.example .env` or generators that rewrite tracked
   files are harmless. The worktree is removed afterwards.
2. **Setup, once.** In CI every job runs `npm ci` on a fresh VM. Locally that
   would install the same dependencies once per job. Install steps
   (`npm ci`, `pip install`, `flutter pub get`, `npx prisma generate`, ...)
   and steps that several jobs repeat word for word at their start are run
   once, before any job, and skipped in the jobs. Installs that happen later
   anyway (a matrix job, a script that calls `npm ci`) go through an `npm`
   wrapper on `PATH`: a directory is installed once per run and lockfile, and
   later `npm ci` / `npm install` there are no-ops.
3. **Units in parallel.** Jobs linked by `needs:` (or writing the same build
   output) form a unit and run in order. Units run in parallel, each in its
   own copy of the worktree as it is after setup, as each CI job has its own
   checkout: a job that edits files for a while (a negative control, a
   formatter, mutation testing) is not seen by the jobs beside it. The copy is
   a copy-on-write clone (`clonefile(2)` on APFS, `cp --reflink=auto` elsewhere),
   except that each `node_modules` holds links to the packages in setup's: even
   a clone makes and later deletes one file per file, and dependencies are
   most of the files. A
   step that may write into `node_modules` (an install, `npm rebuild`,
   `prisma generate`, `yarn`, `pnpm`, `patch-package`, or an npm command from
   inside a script) first gets a copy of its own. With canirun,
   the entries of a matrix run at once too (up to `strategy.max-parallel`),
   each in its own copy. Jobs that start services or containers, run
   Playwright or Cypress, or look like integration / e2e tests, are
   *exclusive*. Their fixed ports and Compose projects are rewritten (see
   below). With portool and a `.portool.toml`, exclusive jobs without
   `services:` overlap freely, except a job that pins something rewriting
   cannot reach (a Compose file with `container_name:` or a literal host port,
   `docker run --name`): it takes a per-repository lock. Units with a
   `services:` job, and every exclusive unit without portool, hold that lock
   and never overlap, in this run or in another `local-ci` run of the same
   repository: a port their code picks for itself (a test server's) is not
   theirs alone. Either way, at most
   `--containers` exclusive jobs (default 2) run at once on the machine, across
   all `local-ci` runs, so databases do not pile up while their tests wait
   for CPU.
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
  database you already run on 5432 is not in the way. A host port the workflow
  never names that way is left as declared, since then the code itself
  defaults to it. `--health-cmd` is waited for.
- Fixed ports and Compose projects in exclusive jobs (below). A port the job
  sets itself — a number in a `*_PORT` variable of its `env`, or
  `TEST_DB_PORT=15432 docker compose ...` in a script — is given a free port,
  and every place the job uses it is rewritten the same way. `docker compose -p
  <name>` and `COMPOSE_PROJECT_NAME=<name>` get a suffix of the run, so two runs
  never share a project and the clean-up never stops one you started yourself.
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
- **[portool](https://github.com/t09tanaka/portool)**: in a repository with a
  `.portool.toml`, each exclusive job without `services:` asks portool for a
  port block of its own
  worktree copy and gets `.env.portool` (`TEST_PORT=...`) in its environment, so
  a script that falls back to a fixed port (`TEST_PORT ?? 4000`) uses one no
  other worktree has. The workflow's own `env` still wins. The block is given
  back when the copy is removed. `--no-portool` turns this off.

## Files

The cache directory is `~/Library/Caches/local-ci` on macOS and
`$XDG_CACHE_HOME/local-ci` (or `~/.cache/local-ci`) elsewhere; set
`LOCAL_CI_HOME` to move it. It holds the worktrees while they run
(`wt/<sha>-<pid>`, one `wt/<sha>-u<n>-<pid>` per unit and one `wt/<sha>-c<n>-<pid>` per matrix entry run at once), the last 20
runs of each repository (`runs/<repo>/<time>-<sha>/` with one log per step and
`result.json`), and the locks.

## License

MIT
