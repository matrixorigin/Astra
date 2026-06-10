import {
  mergeTextDelta,
  splitThinkingTags,
  stripDsmlToolCallBlocks,
} from "@/lib/api/stream-text";

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

  it("keeps DSML tool-call envelopes out of visible assistant text", () => {
    const dsml =
      "<\uFF5C\uFF5CDSML\uFF5C\uFF5Ctool_calls> " +
      "<\uFF5C\uFF5CDSML\uFF5C\uFF5Cinvoke name=\"agent\">" +
      "<\uFF5C\uFF5CDSML\uFF5C\uFF5Cparameter name=\"action\">get_result" +
      "</\uFF5C\uFF5CDSML\uFF5C\uFF5Cparameter>" +
      "</\uFF5C\uFF5CDSML\uFF5C\uFF5Cinvoke> " +
      "</\uFF5C\uFF5CDSML\uFF5C\uFF5Ctool_calls>";

    expect(splitThinkingTags(`Done.\n\n${dsml}`).visibleText).toBe("Done.");
  });
});

describe("stripDsmlToolCallBlocks", () => {
  it("hides incomplete DSML tool-call blocks while streaming", () => {
    const partial =
      "Waiting.\n\n<\uFF5C\uFF5CDSML\uFF5C\uFF5Ctool_calls>" +
      "<\uFF5C\uFF5CDSML\uFF5C\uFF5Cinvoke name=\"agent\">";

    expect(stripDsmlToolCallBlocks(partial)).toBe("Waiting.");
  });
});
