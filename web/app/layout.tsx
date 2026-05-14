import type { Metadata } from 'next';
import './globals.css';

export const metadata: Metadata = {
  title: 'astra web',
  description: 'Astra web agent workspace.',
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
