// @ts-check
const { defineConfig } = require("@playwright/test");

module.exports = defineConfig({
  testDir: "tests",
  reporter: "list",
  // one browser at two sizes: each test is one result per project
  projects: [
    { name: "desktop", use: { browserName: "chromium", viewport: { width: 1280, height: 720 } } },
    { name: "mobile", use: { browserName: "chromium", viewport: { width: 390, height: 844 } } },
  ],
});
