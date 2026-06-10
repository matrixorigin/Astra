import { mergeTextDelta, splitThinkingTags } from "@/lib/api/stream-text";

describe("mergeTextDelta", () => {
  it("ignores empty, duplicate, and replayed suffix deltas", () => {
    expect(mergeTextDelta("Hello", "")).toBe("Hello");
    expect(mergeTextDelta("Hello", "Hello")).toBe("Hello");
    expect(mergeTextDelta("Hello World", " World")).toBe("Hello World");
  });

  it("accepts full-text replacement deltas and appends normal incremental deltas", () => {
    expect(mergeTextDelta("Hello", "Hello World")).toBe("Hello World");
    expect(mergeTextDelta("Hello", " World")).toBe("Hello World");
  });
});

describe("splitThinkingTags", () => {
  it("splits visible text and reasoning from thinking tags", () => {
    expect(splitThinkingTags("<thinking>hidden</thinking>visible")).toEqual({
      visibleText: "visible",
      reasoning: "hidden",
      hasThinking: true,
      reasoningOpen: false,
    });
  });

  it("handles orphan close tags as reasoning", () => {
    expect(splitThinkingTags("hidden</thinking>visible")).toEqual({
      visibleText: "visible",
      reasoning: "hidden",
      hasThinking: true,
      reasoningOpen: false,
    });
  });

  it("marks unterminated thinking tags as open reasoning", () => {
    expect(splitThinkingTags("visible <think>still thinking")).toEqual({
      visibleText: "visible",
      reasoning: "still thinking",
      hasThinking: true,
      reasoningOpen: true,
    });
  });
});
