const { test, expect } = require("@playwright/test");

test("adds", () => {
  expect(1 + 1).toBe(2);
});

test("multiplies", () => {
  expect(2 * 3).toBe(6);
});
