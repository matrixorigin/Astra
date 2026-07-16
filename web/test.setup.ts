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
    // Property is non-configurable (Node 25 WinterCG): cannot replace wholesale.
    // Patch missing methods on the existing object so tests can proceed.
    try {
      const ls = window.localStorage as unknown as Record<string, unknown>;
      if (typeof ls["clear"] !== "function")
        ls["clear"] = fakeStorage.clear.bind(fakeStorage);
      if (typeof ls["getItem"] !== "function")
        ls["getItem"] = fakeStorage.getItem.bind(fakeStorage);
      if (typeof ls["setItem"] !== "function")
        ls["setItem"] = fakeStorage.setItem.bind(fakeStorage);
      if (typeof ls["removeItem"] !== "function")
        ls["removeItem"] = fakeStorage.removeItem.bind(fakeStorage);
      if (typeof ls["key"] !== "function")
        ls["key"] = fakeStorage.key.bind(fakeStorage);
    } catch {
      // Ignore: if patching also fails, individual tests will surface the issue.
    }
  } else {
    Object.defineProperty(window, "localStorage", {
      value: fakeStorage,
      writable: true,
      configurable: true,
    });
  }
}
