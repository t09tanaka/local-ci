const { test, expect } = require("@playwright/test");

test.describe.serial("a sequence", () => {
  let state = 0;
  test("starts", () => {
    state = 1;
    expect(state).toBe(1);
  });
  test("continues from the first", () => {
    expect(state).toBe(1);
  });
});
