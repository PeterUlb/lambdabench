// Scenario "hello": return a constant.
// Baseline isolating Node runtime startup + handler dispatch, no I/O, no SDK.
// The returned constant matches the other languages' hello output, so only
// runtime overhead differs.
// Node 24 requires async handlers (callback style was removed).

export const handler = async () => {
  return { message: "hello", scenario: "hello" };
};
