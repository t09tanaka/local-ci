// Runs Cypress through its module API and prints one line a shard's report is
// read from. Arguments: the `cypress run` arguments to keep (a JSON array) and
// the spec files to run (a JSON array, empty for all of them).
const cypress = require("cypress");
const [keep, specs] = process.argv.slice(1).map((a) => JSON.parse(a));

(async () => {
  const options = await cypress.cli.parseRunArguments(["cypress", "run", ...keep]);
  if (specs.length > 0) options.spec = specs.join(",");
  const result = await cypress.run(options);
  const failed = result.status === "failed";
  const runs = failed
    ? []
    : result.runs.map((r) => ({
        spec: r.spec.relative,
        tests: r.stats.tests,
        passes: r.stats.passes,
        failures: r.stats.failures,
        pending: r.stats.pending,
        skipped: r.stats.skipped,
        error: Boolean(r.error),
      }));
  const report = {
    failed,
    message: failed ? String(result.message) : null,
    total_failed: failed ? 0 : result.totalFailed,
    runs,
  };
  console.log("\nLOCAL_CI_CYPRESS " + JSON.stringify(report));
  // the module API resolves whatever the tests did; the exit code is ours
  process.exit(failed || result.totalFailed > 0 ? 1 : 0);
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
