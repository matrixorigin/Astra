import nextCoreWebVitals from "eslint-config-next/core-web-vitals";

const config = [
  {
    ignores: [".next/**", ".next-dev/**", "coverage/**", "node_modules/**"],
  },
  ...nextCoreWebVitals,
  {
    // Astra does not enable the React Compiler yet. Keep the correctness rules
    // for hooks, but do not make compiler eligibility a repository-wide lint
    // contract before the runtime and codebase adopt that execution model.
    rules: {
      "react-hooks/config": "off",
      "react-hooks/error-boundaries": "off",
      "react-hooks/gating": "off",
      "react-hooks/globals": "off",
      "react-hooks/immutability": "off",
      "react-hooks/incompatible-library": "off",
      "react-hooks/preserve-manual-memoization": "off",
      "react-hooks/purity": "off",
      "react-hooks/refs": "off",
      "react-hooks/set-state-in-effect": "off",
      "react-hooks/set-state-in-render": "off",
      "react-hooks/static-components": "off",
      "react-hooks/unsupported-syntax": "off",
      "react-hooks/use-memo": "off",
    },
  },
];

export default config;
