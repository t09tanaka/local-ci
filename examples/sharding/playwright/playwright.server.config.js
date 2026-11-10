// @ts-check
// A suite whose configuration starts a server on a fixed port: every shard
// would start it again on the same port, so local-ci does not split it.
const base = require("./playwright.config.js");

module.exports = {
  ...base,
  webServer: { command: "node -e \"setInterval(() => {}, 1000)\"", port: 4321 },
};
