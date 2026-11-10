// @ts-check
// A setup project with a teardown project: the teardown runs after the tests
// of the projects that depend on the setup, in every shard that runs any.
const { defineConfig } = require("@playwright/test");

module.exports = defineConfig({
  testDir: "tests",
  projects: [
    { name: "setup", testMatch: /.*\.setup\.js/, teardown: "cleanup" },
    { name: "cleanup", testMatch: /.*\.teardown\.js/ },
    { name: "desktop", dependencies: ["setup"] },
  ],
});
