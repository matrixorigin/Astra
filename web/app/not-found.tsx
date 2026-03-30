import Link from 'next/link';

export default function NotFound() {
  return (
    <div className="flex min-h-screen flex-col items-center justify-center gap-4 bg-slate-950 p-4">
      <h1 className="text-6xl font-bold text-slate-700">404</h1>
      <p className="text-lg text-slate-400">Page not found</p>
      <p className="max-w-sm text-center text-sm text-slate-500">
        The page you are looking for does not exist or has been moved.
      </p>
      <Link
        href="/overview"
        className="mt-4 rounded-xl bg-sky-600 px-5 py-2.5 text-sm font-medium text-white hover:bg-sky-500"
      >
        Go to overview
      </Link>
    </div>
  );
}
