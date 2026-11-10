# Changelog

## [Unreleased]

- `exec::Opts` has a new field `executor: Option<Arc<dyn StepExecutor>>`
  (`src/step_executor.rs`). When set, every step is handed to it as a
  `StepRequest` (key, argv, cwd, env, log, timeout, script pipe, cancel
  handle) instead of being started by local-ci, and the `StepExit` it returns
  (exit, run time, queue wait) is what the result reports; the contract an
  implementation keeps is in the trait's documentation. `exec::Exit` and
  `CancelHandle::track_pgid` / `untrack_pgid` are now public for this. With no
  executor nothing changes. Code that builds `Opts` by hand adds
  `executor: None`.
  This is a breaking change for external code: because the field is public,
  every struct literal of `exec::Opts` outside this crate must add
  `executor: None` to keep compiling.
- New `shard` module: a `RunnerAdapter` contract (`discover`, `partition`,
  `invoke`, `collect`, `merge`) and `partition_tests`, which divides a job's
  tests among shards (longest first, evened out by estimated load, with a
  stable name-hash fallback when there is no history), checks that every test
  is in exactly one shard, and says why a job cannot be split. Adapters for
  `cargo test`, Vitest and Jest.
- `FrozenPlan::with_shards` / `new_with_shards` turn a job into one task per
  shard on request; nothing is split by default. A split job's `ShardGroup`
  carries the runner, the listed tests and `fail_fast`, and
  `complete::aggregate_shard_group` puts the shard results together by job,
  shard and test. `NotSplit` has more reasons, and `ExecutionTask` a `shard`.
- The plan schema version is 5; a plan of version 4 is refused by name.

## [0.9.0] - 2026-11-10

- CLI breaking change: the tool that queues heavy steps, canirun, is now
  task-traffic-controller, and local-ci no longer uses the old name. It looks
  for `task-traffic-controller` on PATH and reads only that tool's output
  prefix (including the ledger lock timeout retry). `--no-canirun` is replaced
  by `--no-traffic-controller`; the old flag is removed. The `canirun` field of
  `exec::Opts` is `traffic_controller`.

## 0.8.0 - 2026-11-10

- CLI behaviour change: a run that has skipped work now ends `incomplete`,
  exits 3 and has the commit status `error`, where it used to be `success`
  and exit 0 (see the entry on completeness below; the spec intends this).
  `--json` and `result.json` have a new state `no_work` for a run whose changes
  trigger no workflow, which exits 0 and is still posted as `success`.
- `actions/upload-artifact@v4` and `actions/download-artifact@v4` run within a
  run instead of being skipped (`src/artifact.rs`, `src/artifact_io.rs`):
  `name`, `path` (lines, globs, exclusions), `if-no-files-found`, `overwrite`
  (a new version), `pattern`, `merge-multiple`. An upload fixes its files
  read-only in the run directory and writes a receipt (path, size, SHA-256,
  plan digest, attempt, job) before the step completes; a download needs the
  receipt only, verifies the hashes and then places the files, refusing
  absolute paths, `..` and symlinks. Cross-run options (`run-id`,
  `repository`, `github-token`, ...), another major version and a matrix upload
  whose name does not read every key of the matrix (or `strategy.job-index`;
  keys that `include` only adds to a variant are not asked for) or holds the
  whole matrix (`${{ matrix }}`, `toJSON(matrix)`) are errors of the jobs of their workflow, reported
  before anything of it runs and only for it: planning and the other
  workflows go on. Jobs a job filter leaves out are not looked at. A
  40-character commit SHA runs as v4, with one warning. `compression-level`
  (0 to 9) is accepted, recorded in the receipt and has no effect. A task's
  required artifacts (`verification.artifacts`) and needed ones (`artifacts`)
  are filled in from the plan (an `if:` that is only `always()`, `success()`,
  `!cancelled()` or `true` does not make an upload optional), `TaskResult.artifacts`
  from the receipts, and a run is not a success while a required one has none.
  Units that use them stay in the results cache: a kept result carries the
  artifacts its unit uploaded and restores them into the run (each file checked
  against its receipt, which gets the new plan digest and attempt, and
  `restored_from`; a mismatch is a cache miss), and a unit that downloads is
  reused only against the same upstream artifact digests. `strategy.job-index`
  and `strategy.job-total` are set in a matrix job's expressions.
  `execute_task` refuses the same workflows (its jobs are errors, nothing is
  started), a run's directory is never shared by two runs of one commit in the
  same second, and a failed restore leaves no artifact of a unit that runs again.
  Artifacts belong to a workflow run, as on GitHub: they are laid out per
  workflow file (`artifacts/<workflow>/<name>/v<N>/`), so two workflows may
  upload the same name and a job cannot download what another workflow
  uploaded; the results cache keeps and restores them per workflow too.
  `validate_run_result` takes what a task published from the receipts of the
  task's own jobs, as `execute_task` does. Library: `execute_plan` needs a new,
  unique `Workspace::run_dir` (`claim_run_dir` makes one) and refuses one that
  already has artifacts; `TaskResult`, `JobResult` and what they hold are
  `Deserialize`.
- Library: `source::capture_source` fixes the input of a run (staged and
  unstaged changes, untracked files, deletions, modes, symlinks, submodules,
  LFS payloads) in a private content-addressed store and describes it in a
  `SourceManifest` (schema 2). The synthetic commit holds Git's canonical
  blobs, so a clean repository has the tree of `HEAD`; a worker writes the raw
  bytes into a `--no-checkout` worktree (`source::add_worktree`). When the
  repository already reads the settings the manifest records (`core.autocrlf`
  / `eol` / `safecrlf`, `info/attributes`, the filters in use; compared in the
  new worktree itself, unset `autocrlf` / `eol` being their defaults), as the
  original does in a local run, no config is written; otherwise a worker
  repeats them in that worktree only, and the original is never written to
  (`OriginalConfigChanged` when its settings changed after the capture). It
  fails with a typed `SourceError` when it lacks a filter program
  (`FilterMissing`) or Git still sees a change (`WorkspaceNotClean`; also what
  an `info/attributes` / `.gitattributes` conflict ends in). A
  shallow clone and a SHA-256 repository are refused up front. Skip-worktree
  and assume-unchanged files are hashed from the disk, and attributes come from
  the captured `.gitattributes` files, not from the original's working tree.
  The first capture copies the history of `HEAD` into the private repository.
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
  The plan carries its own wire types (`wire`) instead of the
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
- Library: setup is per task. `derive_setup_recipe` gives each task the install
  steps it starts with, in order, with their directory, resolved shell (step,
  job, workflow) and combined env, the toolchain versions its jobs ask for, and
  the preparation that stays in its job with the reason (`setup` module). The
  plan no longer records an `execute` flag or an owning job for hoisted
  installs (wire schema version 3). `execute_setup` does a recipe and returns a
  `PreparedReceipt`; `execute_task` takes an optional receipt, skips the steps
  it names and runs every other one itself, so a task placed away from the
  host that did the installs misses nothing. A `TemplateKey` (source, steps,
  directory, shell, env, toolchain, OS, arch, profile, cache schema, run or
  persistent scope, freshness, secret digests) says when prepared work may be
  reused, and `TemplateStore` keeps finished templates read-only and hands out
  copies. git, cp, portool and the version probes now start with the context's
  environment too. A receipt is believed only in the workspace it was made in
  (`execute_setup` keeps an id in the workspace's own git dir, outside the working tree, recorded with the workspace's path; a copy made by any other means is another workspace and has no id until it is prepared itself;
  what local-ci derives itself is bound explicitly: a unit's copy gets an id of its own and the setup's receipts are rebound to it, and `PreparedTemplate::materialize` gives `dest` an id and returns the template's receipt bound to it),
  only while the top-level entries the setup made are still in the workspace,
  for the same commit and under the template key the task's recipe has with
  the `TemplateContext` given to `execute_task`; otherwise the task does its
  setup itself and `TaskResult::receipt_ignored` says why. A local run changes
  as follows: setup goes through canirun when it is installed; a step that
  writes `GITHUB_ENV`/`GITHUB_PATH`/`GITHUB_OUTPUT`/`GITHUB_STATE`, builds a
  Python venv (`pip install` included), has an env that depends on
  `needs`/`matrix`/`steps`, or runs with a shell local-ci cannot start
  (`defaults.run.shell: pwsh`) is no longer hoisted and runs in its job (the
  steps after it in that job stay too); an install that two jobs run in one
  directory with another env or shell is not hoisted either, so each job gets
  the result of its own install; setup uses the resolved shell and env
  (workflow, job, step) instead of the step's own only; a step that comes up
  again after others in a job runs again; `shell: bash` and no shell are the
  same shell. A job that differs in any `env:` value (job env included) no
  longer shares an install with the others, so its install is not hoisted: job
  env that differs without mattering (`DATABASE_URL`, `CI`) can be left out of
  the keys with `[setup] ignore-env = ["DATABASE_URL"]` in `.local-ci.toml`;
  the install still runs with the first job's value. `local-ci plan` lists the
  setup it could not work out instead of leaving it out.
- Job-level `success()`, `failure()`, `cancelled()` and `always()` and
  `needs` now follow GitHub. A job without a status function is skipped unless
  every job it needs succeeded; a skipped job does not count as a failure, so
  `if: failure()` after a job that was only skipped no longer runs (it ran
  before). `failure()` is true when a job it needs failed or any job above that
  did, also through jobs that were skipped for it. `cancelled()` is true when
  the run was cancelled, not when a job it needs was. A skipped job says why:
  `needs failed`, `needs cancelled`, `needs skipped` or `if: false`. Library:
  `JobOutcome` has `ancestor_failed`, `TaskInputs` has `run_cancelled` (both
  default to false when missing), and `TaskInputs::new` builds the inputs of a
  task from the outcomes finished so far.
- A result is checked against everything the plan expects before it counts as
  a success. `validate_run_result(&FrozenPlan, &RunResult)` lists the units,
  the job entries (a matrix job once per variant) and the steps of the whole
  plan (a unit reused from the cache counts as its result) and reports what is
  missing, there twice, not in the plan, or without a reason (a skip), and
  what the plan requires of a task's result (the outputs its jobs declare, the
  artifacts it must publish; `verify_task_result` does this for one
  `TaskResult`). `result.json` and `--json` have a `completeness` object, and
  `state` can now also be `no_work`, `incomplete` or, for a result that lacks
  or doubles something, `error`. Behaviour changes:
  - A run whose changes trigger no workflow has the state `no_work`, not
    `success` (it says `NO WORK`, and its commit status description starts
    with `no_work`). The exit code stays 0, and the commit status is still
    posted as `success`, so a required `local-ci` check can pass on a commit
    that triggers nothing, as before.
  - A run that skipped what local-ci cannot reproduce (a step that uses an
    action it does not know, a `secrets` step, an install of system
    packages, a push or deploy) has the state `incomplete` and exits 3 (it used to be a `success` with a
    "not covered" note and exit 0; exit 2 stays for `error`). Its commit
    status is `error`, never `success`, and its description names the first
    steps that were left out; `local-ci status` does the same. A workflow that
    was selected but cannot be run (a dependency cycle, a `needs` of a job that
    does not exist, a matrix with keys that print alike) makes the run
    `incomplete` too, with the reason under `completeness.incomplete`; a
    workflow the changes simply do not trigger does not. `checkout`, `cache` and the
    setup actions are not gaps.
    `upload-artifact` and `download-artifact` are not gaps either: they run.
    Only a form that `artifact::parse_uses` refuses (another major version, a
    sub-action such as `upload-artifact/merge`), or an option local-ci cannot
    run, is not run, and then as an `error` of the jobs of its workflow.
  - A task that cannot be run (not a step that fails: the task itself)
    counts as a failure for the jobs that need it. Before, those jobs saw a
    need that was not there.
- A setup receipt holds the content digest of what the setup made. The
  artifacts of `PreparedReceipt` were a digest of names and sizes; they are now
  the tree digest (`tree::tree_digest`: path, kind, executable bit and content
  hash of every entry, symlinks by their link value, in a fixed order). A task
  skips its installs only when the tree is still what the setup made: a file
  gone or changed in `node_modules` makes it ignore the receipt and install
  again (`receipt_ignored` says so). Receipts made before this have a digest
  that never matches and are ignored. Library: `PreparedReceipt` has `notes`.
- When jobs that share one install have other values of an `ignore-env`
  variable, the install ran with the values of the first. The setup step's
  note, the run's warnings and the receipt's `notes` now say which job's
  value it ran with and which other jobs differ. Only the name and the jobs
  are shown, not a value or a fingerprint of one (a short value can be guessed
  from a fingerprint).
- What is left out is decided from the commands a `run:` script runs, not from
  words in it. The script is cut into simple commands (at `;`, `&&`, `||`,
  `|`, `&` and newlines; comments and heredoc bodies are dropped; `bash -c`
  scripts are read too), the command is found past `sudo`, `env`, `command`
  and the like, and its subcommand decides. `git push --dry-run`,
  `echo using aws sdk`, `grep x vercel.json`, `kubectl kustomize`,
  `gh pr list`, `aws sts get-caller-identity` and `sudo -n true` are run;
  `git push`, `npm publish`, `gh pr merge|create|comment|edit`,
  `gh api -X POST`, `aws s3 cp|sync|rm` to a bucket, `aws ... deploy`,
  `kubectl apply|delete|create|replace|patch|rollout`, `vercel`/`netlify`/
  `wrangler` deploys, `docker push` / `image push` / `compose push` /
  `buildx build --push`, `curl`/`wget` with a writing method or a body, the
  same commands behind `npx`, `bunx`, `pnpm dlx|exec`, `yarn dlx|exec` and
  `npm exec`, a `publish` of npm/pnpm/yarn/bun wherever it stands among the
  options, and installs of system packages (`sudo apt-get install`, `brew
  install`) are left out. A `brew install` of formulae that are on the PATH
  already is run.
- A step that reads `github.token` or `secrets.GITHUB_TOKEN` is run with this
  machine's token (`gh auth token`, asked once, only when a step needs it);
  without a token it is skipped as before (`incomplete`). As on GitHub, the
  value is the result of the expression, put into the script text (and into
  `env`, `if:` and `with:`) wherever the expression stands, so no reading of
  the shell's quoting is involved. The value is never written to a file of the
  run: a step whose script or env holds it (also through a step output or
  `GITHUB_ENV`) is handed its script over a pipe, file descriptor 3, and the
  shell is started with `/dev/fd/3` (`bash --noprofile --norc -eo pipefail
  /dev/fd/3`, `python3 /dev/fd/3`), so neither a file nor the argument list
  shows it. bash and sh read such a script one byte at a time, so the lines not
  run yet stay in the pipe, and fd 3 is inherited by everything the script
  starts: the script therefore begins with `exec 3<&-;` (on its first line, so
  line numbers do not move), which the shell does not mind, as it reads the
  script through a descriptor of its own; a child that reads fd 3 gets EBADF.
  Python parses the whole script before it runs a line, so nothing is left in
  the pipe. A custom shell (`shell: bash {0}`, `shellcheck {0} && bash {0}`) may
  open the script twice and cannot be told to close fd 3, so a step of one
  whose script holds the token is not run: it is an `error` that says to use
  bash, sh or python (a custom shell is still run when only its `env` holds the
  token). On macOS the pipe is a FIFO opened with `O_CLOEXEC`, whose name is
  removed at once, not `pipe()` plus a later `fcntl` (two calls there), so no
  command started by another thread can inherit an end; elsewhere it is
  `std::io::pipe()` (one `pipe2(O_CLOEXEC)` call), as a FIFO would make
  `/dev/fd/3` block on Linux once the writer has finished. Its writer thread ends with the
  step even if a process that outlived the step holds the read end. canirun
  passes the descriptor on to its child, so such a step is still queued by
  canirun. The notes and errors of `uses:` steps and of `if:` are
  hidden the same way. A value changed into another form (base64, say) is not
  hidden. Other steps are
  written to a file as before (mode 0600). The value is replaced by `***` in
  step logs and labels. A later step of the job gets a step output's real
  value (in memory only); a job output that holds the token is not passed on
  to the jobs that need it, as on GitHub: it is recorded as empty (the name
  stays, since a plan expects the job to set it) and the run's warnings name
  it. The files behind `GITHUB_ENV`, `GITHUB_OUTPUT`,
  `GITHUB_PATH` and `GITHUB_STEP_SUMMARY` are private (directory 0700, files
  0600) and removed when the step ends. Other secrets are still never
  supplied.
- A receipt is believed without hashing the tree again in the workspace the
  run's own setup made, and in a unit's copy of it (checked once, when the copy
  is made, before any task wrote in it), so a job that writes next to the
  install (`node_modules/.cache`) does not make the next job install again. A
  receipt that comes from elsewhere (`execute_task`) is checked right before it
  is used, and `PreparedTemplate::materialize` checks the copy it made against
  the receipt (an altered template is an error). The tree digest takes file
  names and link targets as bytes, so names that are not UTF-8 are told apart.
- A task that cannot be run fails every entry of a matrix job (with the
  reason), not one entry in its place, so the result no longer also lists the
  other entries as missing.
- Library, breaking: `report::assemble` no longer takes the `no_work` flag and
  takes the whole plan; `report::warnings` and `report::description` have new
  signatures (`description` takes the list of what was left out, not a count);
  `UnitResult` has `unit` and `outcomes`; `RunResult` has `completeness`;
  `PreparedReceipt` has `notes`; `SkippedWorkflow` has `unrunnable`;
  `TaskResult` has `artifacts`; `status()` returns the state a commit status
  can have (`success`, `failure` or `error`), not the run's state; `Ctx` of
  `expr` has `with_token`; `ExecContext::github_token`. The
  exit code of an `incomplete` run is 3.

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
