import "@testing-library/jest-dom/vitest";

// Node 25 ships a built-in globalThis.localStorage (WinterCG) whose properties
// are non-configurable. In vitest's jsdom environment it leaks onto `window`
// and shadows the spec-compliant jsdom Storage, breaking window.localStorage.clear().
// Replace window.localStorage wholesale when .clear is missing.
if (
  typeof window !== "undefined" &&
  typeof window.localStorage?.clear !== "function"
) {
  const store: Record<string, string> = {};
  const fakeStorage: Storage = {
    get length() {
      return Object.keys(store).length;
    },
    clear() {
      Object.keys(store).forEach((k) => delete store[k]);
    },
    getItem(key: string) {
      return Object.prototype.hasOwnProperty.call(store, key)
        ? store[key]
        : null;
    },
    setItem(key: string, value: string) {
      store[key] = String(value);
    },
    removeItem(key: string) {
      delete store[key];
    },
    key(index: number) {
      return Object.keys(store)[index] ?? null;
    },
  };
  const descriptor = Object.getOwnPropertyDescriptor(window, "localStorage");
  if (descriptor && !descriptor.configurable) {
    throw new Error(
      "test.setup.ts: window.localStorage is non-configurable in this environment. " +
        "The fakeStorage mock cannot be installed. " +
        "Check your Node/jsdom version or update the workaround in test.setup.ts."
    );
  }
  Object.defineProperty(window, "localStorage", {
    value: fakeStorage,
    writable: true,
    configurable: true,
  });
}
