import { describe, expect, it } from "vitest";
import { safeNextPath } from "@/lib/auth/next-path";

describe("sign-in return path", () => {
  it("preserves a local path and query", () => {
    expect(safeNextPath("/now?source=review")).toBe("/now?source=review");
    expect(safeNextPath("/now?source=hello%5Cworld")).toBe("/now?source=hello%5Cworld");
    expect(safeNextPath("/now?source=line%0Abreak")).toBe("/now?source=line%0Abreak");
  });

  it.each([null, "https://example.com", "//example.com", "/%2fexample.com", "/%5cexample.com", "/%00", "/%xx", "/a/..//example.com", "/a/%2e%2e//example.com", "/a/../%2fexample.com"])(
    "rejects unsafe return path %s",
    (value) => expect(safeNextPath(value)).toBe("/overview"),
  );
});
