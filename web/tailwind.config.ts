import type { Config } from 'tailwindcss';

const config: Config = {
  content: [
    './app/**/*.{js,ts,jsx,tsx,mdx}',
    './components/**/*.{js,ts,jsx,tsx,mdx}',
    './lib/**/*.{js,ts,jsx,tsx,mdx}',
  ],
  theme: {
    extend: {
      colors: {
        surface: '#0b1220',
        panel: '#111827',
        border: '#1f2937',
        accent: '#38bdf8',
        success: '#22c55e',
        warning: '#f59e0b',
      },
    },
  },
  plugins: [],
};

export default config;
