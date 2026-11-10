// Lists the tests of a Playwright suite, and what a listing cannot show: which
// projects depend on another (the tests of a project listed alone differ from
// the same listing without its dependencies), which spec files declare serial
// tests, and what the configuration file says about dependencies, teardown and
// parallelism. Prints the Playwright report with a `localCi` key added.
// Arguments: the command (a JSON array) and the arguments to keep (a JSON array).
const { spawnSync } = require("child_process");
const fs = require("fs");
const path = require("path");
const [command, args] = process.argv.slice(1).map((a) => JSON.parse(a));

const run = (extra) =>
  spawnSync(command[0], [...command.slice(1), "--list", "--reporter=json", ...extra], {
    encoding: "utf8",
    maxBuffer: 1 << 30,
  });
const parse = (r) => {
  if (r.status !== 0) return null;
  try {
    return JSON.parse(r.stdout);
  } catch (e) {
    return null;
  }
};
const walk = (suite, file, out) => {
  const own = suite.file || file;
  for (const spec of suite.specs || [])
    for (const test of spec.tests || [])
      out.push([test.projectName, own, spec.title, spec.line].join("\u0001"));
  for (const child of suite.suites || []) walk(child, own, out);
};
const keys = (report) => {
  const out = [];
  for (const s of report.suites || []) walk(s, s.file, out);
  return out.sort().join("\n");
};

const first = run(args);
process.stderr.write(first.stderr || "");
const report = parse(first);
if (!report) {
  process.stdout.write(first.stdout || "");
  process.exit(first.status || 1);
}

// the projects to look at: those the job names, or all of them
const named = [];
const rest = [];
for (let i = 0; i < args.length; i++) {
  if (args[i] === "--project") named.push(args[++i]);
  else if (args[i].startsWith("--project=")) named.push(args[i].slice("--project=".length));
  else rest.push(args[i]);
}
const config = report.config || {};
const projects = named.length > 0 ? named : (config.projects || []).map((p) => p.name).filter(Boolean);
const dependencies = [];
for (const p of projects) {
  const withDeps = parse(run([...rest, "--project", p]));
  const without = parse(run([...rest, "--project", p, "--no-deps"]));
  if (!withDeps || !without || keys(withDeps) !== keys(without)) dependencies.push(p);
}

const mentions = [];
try {
  const text = fs.readFileSync(config.configFile, "utf8");
  for (const w of ["dependencies", "teardown", "globalSetup", "globalTeardown"])
    if (new RegExp("\\b" + w + "\\b").test(text)) mentions.push(w);
  if (/fullyParallel\s*:\s*false/.test(text)) mentions.push("fullyParallel:false");
} catch (e) {
  // no configuration file
}

const serial = [];
for (const file of new Set((report.suites || []).map((s) => s.file))) {
  try {
    const text = fs.readFileSync(path.join(config.rootDir, file), "utf8");
    if (/describe\s*\.\s*serial\b|mode\s*:\s*['"`]serial['"`]/.test(text)) serial.push(file);
  } catch (e) {
    serial.push(file); // a file that cannot be read is not known to be parallel
  }
}

let version = null;
for (const name of ["@playwright/test", "playwright"]) {
  try {
    version = require(require.resolve(name + "/package.json", { paths: [process.cwd()] })).version;
    break;
  } catch (e) {
    // try the next package
  }
}

report.localCi = { version, dependencies, config_mentions: mentions, serial_files: serial };
process.stdout.write(JSON.stringify(report));
