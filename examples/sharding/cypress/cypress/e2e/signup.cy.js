describe("signup", () => {
  it("fails on purpose when asked", () => {
    // CYPRESS_SHARD_EXAMPLE_FAIL=1 makes the spec fail
    expect(Cypress.env("SHARD_EXAMPLE_FAIL") ? "failed" : "passed").to.eq("passed");
  });
});
