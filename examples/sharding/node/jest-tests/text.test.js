it("upper-cases", () => expect("a".toUpperCase()).toBe("A"));
it("fails on purpose when asked", () => {
  if (process.env.SHARD_EXAMPLE_FAIL) throw new Error("asked to fail");
});
