/** @type {import('jest').Config} */
export default {
  preset: 'ts-jest',
  testEnvironment: 'node',
  roots: ['<rootDir>/src'],
  testMatch: ['**/__tests__/**/*.test.ts'],
  moduleFileExtensions: ['ts', 'tsx', 'js', 'json'],
  collectCoverageFrom: [
    'src/**/*.ts',
    '!src/**/__tests__/**',
    '!src/index.ts',
    '!src/react.ts',
  ],
  coverageReporters: ['text', 'text-summary', 'html'],
  coverageThreshold: {
    global: {
      statements: 45,
      branches: 32,
      functions: 40,
      lines: 45,
    },
  },
};
