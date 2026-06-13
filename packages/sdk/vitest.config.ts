import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    globals: true,
    environment: "node",
    include: ["src/**/__tests__/**/*.test.ts"],
    coverage: {
      provider: "v8",
      include: ["src/**/*.ts"],
      exclude: ["src/**/__tests__/**", "src/index.ts", "src/react.ts"],
      reporter: ["text", "text-summary", "html"],
      thresholds: {
        statements: 45,
        branches: 32,
        functions: 40,
        lines: 45,
      },
    },
  },
});
