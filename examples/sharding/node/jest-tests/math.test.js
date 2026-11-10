describe("math", () => {
  it("adds", () => expect(1 + 2).toBe(3));
  it("multiplies", () => expect(2 * 3).toBe(6));
  it.skip("is skipped", () => expect(1).toBe(2));
});
