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
| `local-ci run` | Run them and report. Exit code 0 = success (or `no_work`: the changes trigger no workflow), 1 = failure, 2 = error (could not run, or the result lacks something the plan expects), 3 = `incomplete` (what ran passed, but steps or workflows that local-ci cannot reproduce were left out, so it is not a pass; the commit status is `error`), 130 = interrupted. |
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
# Always run these, even when a recent result covers them
no-cache = ["e2e.yml", "api-ci.yml:contract"]

[setup]
# Env names that do not change what an install does (see below)
ignore-env = ["DATABASE_URL", "CI"]
```

| Key | |
|---|---|
| `workflows-dirs` | Directories to read workflows from, relative to the repository root. They replace `.github/workflows`; `--workflows-dir` still adds to them. Each one must hold a `.yml` / `.yaml` file at the commit. The workflows in the first one are named by their file name in the output (`ci.yml › test`), the others by their path. |
| `no-cache` | Workflows (`ci.yml`) or jobs (`ci.yml:e2e`) whose results are never reused (see *Reused results*). |
| `link-node-modules` | `true` to give units' copies links to setup's packages instead of a clone of each `node_modules`. Off by default. |
| `[setup]` `ignore-env` | `env:` names left out of the keys of setup steps. Off (empty) by default. |

**`[setup] ignore-env`.** An install that two jobs run in one directory with a
different `env:` (workflow, job or step) is not shared: each job runs its own,
so that one never sees the result of the other's. That includes variables the
install does not read, such as a `DATABASE_URL` that only the tests use. List
those names and jobs that differ only in them share one install, which runs
with the value of the first job that has it. A name that does change the
install (`NODE_ENV`) must not be listed.

**`link-node-modules`.** Even a copy-on-write clone makes, and later deletes,
one file per file, and dependencies are most of the files: in a monorepo of
twenty units that is millions of files per run, each deletion also checked by
fseventsd. With the key on, a copy's `node_modules` is a directory of links
to the packages setup installed. A step that may write into one (an install,
`npm rebuild`, `prisma generate`, `yarn`, `pnpm`, `patch-package`, or an npm
command from inside a script) first gets a copy of its own; so does a project
whose `node_modules` links out of itself (workspaces, `file:` dependencies)
or whose `tsconfig` emits declarations. It is off by default because a tool
that follows a link to where the package really is, outside the unit's copy,
may refuse it: TypeScript emitting declarations fails with TS2742 (handled
as said), and a Next.js build with Turbopack does not find `next` outside
its workspace root (not handled: do not turn the key on for a repository
that builds with Turbopack). Run everything once with `--no-cache` after
turning it on.

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

0. **Reused results.** A unit whose jobs all passed (or were skipped) in the
   last 24 hours is not run again when nothing it depends on has changed: the
   files its workflow's `paths` cover (all files but its `paths-ignore`, or
   every file when it has neither), every file at the repository root
   (lockfiles, shared configuration), everything under `.github/` and the
   workflow directories, the merge base and the branch, the versions of Node
   and of the tools its steps name (npm, yarn, pnpm, dart, flutter, rustc,
   python3, go), and local-ci itself. This trusts `paths` to name what the
   workflow depends on, as GitHub does when a pull request changes nothing it
   covers: a change in a shared directory the workflow does not list does
   not run it again. The report shows such jobs as
   `PASS … cached (abc1234, 2h ago)`, still lists what they did not cover,
   and when every unit is reused nothing is checked out or installed. Units
   running `npm audit` or other advisory checks (also from a `package.json`
   script) are always run, since their result changes without the code; so
   are those listed in `no-cache`. `--no-cache` runs everything (and keeps
   the new results).
1. **Worktree.** The commit is checked out with `git worktree add --detach`
   under the cache directory. Your working tree is never touched, so steps such
   as `git add -A`, `cp .env.example .env` or generators that rewrite tracked
   files are harmless. The worktree is removed afterwards.
2. **Setup, once.** In CI every job runs `npm ci` on a fresh VM. Locally that
   would install the same dependencies once per job. Install steps
   (`npm ci`, `flutter pub get`, `npx prisma generate`, ...)
   and steps that several jobs repeat word for word at their start are run
   once, before any job, and skipped in the jobs. A step whose result cannot be
   carried to another job stays in its job: one that holds an expression,
   an `if:` or `needs`/`matrix`-dependent env, one that needs the job's
   services, one that writes `GITHUB_ENV`/`GITHUB_PATH`/step outputs, and
   Python venv / `pip install` steps (the venv lives at an absolute path).
   `local-ci plan` lists them with the reason. Installs that happen later
   anyway (a matrix job, a script that calls `npm ci`) go through an `npm`
   wrapper on `PATH`: a directory is installed once per run and lockfile, and
   later `npm ci` / `npm install` there are no-ops.
3. **Units in parallel.** Jobs linked by `needs:` (or writing the same build
   output) form a unit and run in order. Units run in parallel, each in its
   own copy of the worktree as it is after setup, as each CI job has its own
   checkout: a job that edits files for a while (a negative control, a
   formatter, mutation testing) is not seen by the jobs beside it. The copy is
   a copy-on-write clone (`clonefile(2)` on APFS, `cp --reflink=auto` elsewhere).
   With `link-node-modules = true` in `.local-ci.toml`, each `node_modules`
   holds links to the packages in setup's instead (see below). With canirun,
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
   are, and the overall state: `success`, `failure`, `error`, `incomplete` (parts that cannot be reproduced here were skipped, or a workflow could not be run at all; `--json` lists them under `completeness.incomplete` and the exit code is 3) or `no_work` (nothing is triggered). `success` is only for a run whose result has everything the plan expects. A job that
   waited a second or more for something says what it waited for and how long
   it really ran: `PASS  ci.yml › e2e  3m00s (ran 2m20s · waited slot 5m00s, queue 40s)`.
   The job's `time` in `result.json` and in `--json` has the same split, in
   seconds: `work_secs` (the job running: its services' start and its steps), `wait_lock_secs` (the
   per-repository lock), `wait_slot_secs` (one of the `--containers` slots),
   `wait_queue_secs` (canirun letting a step start, summed over the steps) and
   `wait_parallel_secs` (one of the `--parallel` places). The job's `secs`
   still counts from its first step and so includes `wait_queue_secs`; the
   other waits come before it. The waits of a unit before its jobs start go to
   its first job. Jobs that did not run, and reused results, have no `time`. Steps of a shell
other than bash or sh that print nothing count as waiting for canirun until
they end.
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
- `uses:` — `actions/upload-artifact@v4` and `actions/download-artifact@v4`
  run for real within one run (next section); `actions/checkout` and caches
  are skipped; `setup-node`,
  `setup-python`, `setup-go`, `setup-java`, `setup-ruby`, `flutter-action`,
  `setup-dart` and `rust-toolchain` check that the tool is installed and warn
  when its version differs. Other actions are skipped and listed under
  `not covered`.

### Artifacts (`upload-artifact@v4`, `download-artifact@v4`)

Within one run, an upload copies its files into the run's own area
(`<run dir>/artifacts/<name>/v<N>/`), read-only, and writes a receipt (name,
each file's relative path, size and SHA-256, the plan digest, the attempt and
the job) before the step completes. A download reads the receipt, checks every
file against it and only then places the files, so a job that `needs` the
uploading job gets exactly what was uploaded. A run is not a `success` while an
artifact its plan requires (a literal name, a step that always runs, and
`if-no-files-found: error`) has no receipt. Jobs that use these actions are
never taken from the results cache.

| | Supported | Not supported (an error before the run starts) |
|---|---|---|
| Action | `actions/upload-artifact@v4`, `actions/download-artifact@v4` | other major versions, sub-actions such as `upload-artifact/merge` |
| `name` | upload: a name without `/ \ : " < > \| * ?`, default `artifact`; download: one artifact by name | |
| `path` (upload) | several lines, `*` `?` `**` globs, `!` exclusions, a directory takes what is under it; stored relative to the common ancestor of the lines; hidden files only with `include-hidden-files: true` | an absolute path, `..`, a backslash |
| Multiple files | yes (sorted, each with its own size and SHA-256) | symlinks are not followed or uploaded |
| `if-no-files-found` | `warn` (default: the step passes with a warning), `error` (the step fails), `ignore` | other values |
| `overwrite` | a second upload of a name is a conflict by default; `overwrite: true` records a new version, and a download takes the newest | |
| Matrix | the name must contain the variant (`name: report-${{ matrix.os }}`) | a name that is the same in every variant |
| `path` (download) | a directory under the workspace; the default is the workspace itself | an absolute path, `..`, a path through a symlink |
| `pattern`, `merge-multiple` | yes: without `merge-multiple` each artifact goes to `<path>/<name>/`; two artifacts writing one file is an error | |
| `retention-days` | 1 to 90, recorded in the receipt (local runs are kept until pruned) | |
| Across runs, GitHub's side | | `run-id`, `repository`, `github-token`, `artifact-ids`, `skip-decompress`, `compression-level` and any other option |

A download does not wait for the uploading job to finish: it needs the receipt
only, so it must run after the job that uploads (`needs:`); a download with no
receipt is an error that says so. Moving artifacts to and from another host is
not part of local-ci.

A step whose script holds `github.token` is run with bash, sh or python (the
default shell is bash); the script is handed over a pipe and never written to a
file. With another shell (`shell: pwsh {0}`, `shellcheck {0} && bash {0}`) such
a step is not run and the run is an `error`. The value is replaced by `***` in
logs, labels and notes; a changed form of it (base64, say) is not hidden.

Not covered: steps that read a secret other than `github.token` (which gets
this machine's `gh auth token`, never written to a file of the run), steps that push,
publish or deploy (`git push`, `gh release`, `npm publish`, `aws`, `kubectl`,
`terraform apply`, ...), steps that install system packages (`sudo`,
`apt-get`, `brew install`, ...), `container:` jobs and reusable workflows. A
`git push` from inside a script is pointed at a URL that does not exist. `pip install` goes into a virtualenv of the run and `npm install -g`
into a prefix of the run, so neither changes your machine.

## Working with other tools

- **[canirun](https://github.com/t09tanaka/canirun)**: when it is installed,
  every `run:` step, setup included, is wrapped in `canirun exec`, so heavy steps
  wait for CPU instead of piling up. Timeouts start when the step gets its turn.
- **[portool](https://github.com/t09tanaka/portool)**: in a repository with a
  `.portool.toml`, each exclusive job without `services:` asks portool for a
  port block of its own
  worktree copy and gets `.env.portool` (`TEST_PORT=...`) in its environment, so
  a script that falls back to a fixed port (`TEST_PORT ?? 4000`) uses one no
  other worktree has. The workflow's own `env` still wins. The block is given
  back when the copy is removed. `--no-portool` turns this off.

## Capturing a working directory (library)

`source::capture_source` fixes the input of a run as the working directory is
right now (staged and unstaged changes, untracked files, deletions, modes,
symlinks, submodules, LFS payloads) in a private store, and describes it in a
`SourceManifest`. The repository itself is only read.

- The first capture of a repository copies the history of `HEAD` into a
  private bare repository under the store, so a rewrite or `git gc` of the
  original cannot break an export later; the next captures fetch only what is
  new.
- The manifest records the settings that turned the captured bytes into Git's
  blobs (`core.autocrlf`, `core.eol`, `core.safecrlf`, the attribute rules that
  are not in the tree, and the `filter.<name>.*` definitions in use). When the
  repository a run happens in already reads the same values (a run in the
  original itself always does) nothing is written. A worker whose settings
  differ repeats them in its worktree only (`extensions.worktreeConfig` is the
  one switch written to its shared config, and only in a clone that was not
  captured from); if the original's own settings changed after the capture the
  run stops with `SourceError::OriginalConfigChanged` instead of writing to
  it. Either way `git status` agrees with the commit. One known limit: Git
  reads `info/attributes` before `.gitattributes`, but a worker repeats those
  rules where Git reads them last, so when they disagree about a path the
  result is `SourceError::WorkspaceNotClean`, never silently different
  content. A worker without the program of a filter the capture ran gets
  `SourceError::FilterMissing` and can be left out; a workspace that Git still
  sees as changed is `SourceError::WorkspaceNotClean`.
- Not supported yet: shallow clones (`git fetch --unshallow` first) and
  repositories that use SHA-256 object names. Both are refused with an
  explicit error.

## Files

The cache directory is `~/Library/Caches/local-ci` on macOS and
`$XDG_CACHE_HOME/local-ci` (or `~/.cache/local-ci`) elsewhere; set
`LOCAL_CI_HOME` to move it. It holds the worktrees while they run
(`wt/<sha>-<pid>`, one `wt/<sha>-u<n>-<pid>` per unit and one `wt/<sha>-c<n>-<pid>` per matrix entry run at once), the last 20
runs of each repository (`runs/<repo>/<time>-<sha>/` with one log per step and
`result.json`), the results that may be reused (`results/<repo>/`, a day at
most), and the locks.

## Development

```sh
cargo test
./scripts/coverage-gate.sh   # 100% line coverage on the pure modules
```

The pure decision modules (`src/matrix.rs`, `src/filter.rs`, `src/source/gate.rs`,
`src/dag.rs`, `src/frozen.rs`) are gated on 100% line coverage over the whole test suite.
All of them but `src/dag.rs` are also gated on zero surviving mutants. The coverage gate needs `cargo-llvm-cov` and the `llvm-tools-preview`
component. If `cargo llvm-cov` cannot find the LLVM tools, point it at them:

```sh
export LLVM_COV=~/.rustup/toolchains/stable-aarch64-apple-darwin/lib/rustlib/aarch64-apple-darwin/bin/llvm-cov
export LLVM_PROFDATA=~/.rustup/toolchains/stable-aarch64-apple-darwin/lib/rustlib/aarch64-apple-darwin/bin/llvm-profdata
```

Mutation testing uses [cargo-schemata](https://crates.io/crates/cargo-schemata), which reruns the
tests once per mutant. Each module is checked against the test targets that pin it (the exact
commands are in `.github/workflows/ci.yml`):

```sh
cargo schemata --file src/matrix.rs --file src/filter.rs --file src/source/gate.rs -- --lib --test job_dag --test source_capture
cargo schemata --file src/frozen.rs -- --lib --test job_dag --test frozen_plan
cargo schemata --file src/artifact.rs -- --lib artifact::
```

`cargo schemata` copies the sources into `target/schemata/` and points `TMPDIR` there, which is
inside the repository. Tests that need "not a git repository" therefore set
`GIT_CEILING_DIRECTORIES`.

`cargo schemata --file` exits 0 when the glob matches nothing, so CI first runs
`scripts/mutation-targets-check.sh <file>...` (built on `--list`) and fails if any target file
produces no mutants, for example after a rename.

## License

MIT
