'use client';

export default function GlobalError({
  error,
  reset,
}: {
  error: Error & { digest?: string };
  reset: () => void;
}) {
  return (
    <html lang="en">
      <body className="bg-slate-950 text-slate-100">
        <div className="flex min-h-screen flex-col items-center justify-center gap-4 p-4">
          <h1 className="text-4xl font-bold text-slate-700">Error</h1>
          <p className="max-w-md text-center text-sm text-slate-400">
            {error.message || 'An unexpected error occurred.'}
          </p>
          <button
            type="button"
            onClick={reset}
            className="mt-4 rounded-xl bg-sky-600 px-5 py-2.5 text-sm font-medium text-white hover:bg-sky-500"
          >
            Try again
          </button>
        </div>
      </body>
    </html>
  );
}
