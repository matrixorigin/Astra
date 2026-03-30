import type { NextConfig } from 'next';

// Ensure server-side fetch bypasses HTTP proxy for local backend
if (!process.env.no_proxy?.includes('localhost')) {
  const current = process.env.no_proxy ?? process.env.NO_PROXY ?? '';
  const extras = 'localhost,127.0.0.1';
  const merged = current ? `${current},${extras}` : extras;
  process.env.no_proxy = merged;
  process.env.NO_PROXY = merged;
}

const nextConfig: NextConfig = {
  reactStrictMode: true,
  // Proxy backend API calls through Next.js to avoid CORS issues
  async rewrites() {
    const backendUrl = process.env.MO_AGENT_API_URL ?? 'http://localhost:8000';
    return [
      {
        source: '/api/backend/:path*',
        destination: `${backendUrl}/:path*`,
      },
    ];
  },
};

export default nextConfig;
