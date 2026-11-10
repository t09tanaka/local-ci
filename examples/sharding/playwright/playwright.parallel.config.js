// @ts-check
// A fully parallel suite: a test is a unit of its own, except in a file that
// declares serial tests (`ordered.spec.js`), which stays whole.
const { defineConfig } = require("@playwright/test");

module.exports = defineConfig({
  testDir: "parallel-tests",
  fullyParallel: true,
  projects: [{ name: "unit" }],
});
