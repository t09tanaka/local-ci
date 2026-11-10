const { test, expect } = require("@playwright/test");

test("has a viewport of its project", async ({ page }) => {
  await page.setContent("<p>hi</p>");
  expect(page.viewportSize().width).toBeGreaterThan(300);
});

test.skip("is skipped", async () => {});
