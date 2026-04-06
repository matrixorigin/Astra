import type { Metadata } from 'next';
import './globals.css';

export const metadata: Metadata = {
  title: 'astra web',
  description: 'Frontend platform for agent, session, run, and event observability.',
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html lang="en">
      <body className="bg-slate-950 text-slate-100">{children}</body>
    </html>
  );
}
