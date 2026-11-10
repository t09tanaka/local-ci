// @ts-check
// A project that depends on a setup project. The listing does not show the
// dependency, so local-ci splits it; the setup then runs in every shard that
// has a test of `desktop`, and the merge reports that as a problem.
const { defineConfig } = require("@playwright/test");

module.exports = defineConfig({
  testDir: "tests",
  projects: [
    { name: "setup", testMatch: /.*\.setup\.js/ },
    { name: "desktop", dependencies: ["setup"] },
  ],
});
