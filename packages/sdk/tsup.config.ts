import { defineConfig } from 'tsup';

export default defineConfig([
  {
    entry: { index: 'src/index.ts' },
    format: ['esm', 'cjs'],
    dts: true,
    clean: true,
    sourcemap: true,
    splitting: false,
  },
  {
    entry: { react: 'src/react.ts' },
    format: ['esm', 'cjs'],
    dts: true,
    sourcemap: true,
    splitting: false,
    external: ['react'],
  },
]);
