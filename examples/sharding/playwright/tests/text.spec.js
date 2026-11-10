const { test, expect } = require("@playwright/test");

test.describe("text", () => {
  test("upper-cases", async ({ page }) => {
    await page.setContent("<output id=r></output><script>r.textContent = 'a'.toUpperCase()</script>");
    await expect(page.locator("#r")).toHaveText("A");
  });

  test("fails on purpose when asked", async ({ page }) => {
    await page.setContent("<output id=r>ok</output>");
    if (process.env.SHARD_EXAMPLE_FAIL) throw new Error("asked to fail");
  });
});
