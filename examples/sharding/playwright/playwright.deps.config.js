// @ts-check
// A project that depends on a setup project. `--list --project desktop` shows the
// setup's test as well, and `--no-deps` does not, which is how local-ci sees that
// a shard running a test of `desktop` would run the setup too.
const { defineConfig } = require("@playwright/test");

module.exports = defineConfig({
  testDir: "tests",
  projects: [
    { name: "setup", testMatch: /.*\.setup\.js/ },
    { name: "desktop", dependencies: ["setup"] },
  ],
});
