const { test, expect } = require("@playwright/test");

test("adds in the page", async ({ page }) => {
  await page.setContent("<output id=r></output><script>r.textContent = 1 + 2</script>");
  await expect(page.locator("#r")).toHaveText("3");
});

test("multiplies in the page", async ({ page }) => {
  await page.setContent("<output id=r></output><script>r.textContent = 2 * 3</script>");
  await expect(page.locator("#r")).toHaveText("6");
});
