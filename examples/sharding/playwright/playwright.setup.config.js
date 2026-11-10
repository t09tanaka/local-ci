// @ts-check
// A suite with a global setup (a database seed, say): every shard would run it.
const base = require("./playwright.config.js");

module.exports = { ...base, globalSetup: require.resolve("./global-setup.js") };
