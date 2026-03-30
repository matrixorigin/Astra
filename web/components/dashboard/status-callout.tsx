export function StatusCallout({
  title,
  message,
  tone = 'info',
}: {
  title: string;
  message: string;
  tone?: 'info' | 'warning';
}) {
  const toneClasses =
    tone === 'warning'
      ? 'border-amber-400/30 bg-amber-400/10 text-amber-100'
      : 'border-sky-400/30 bg-sky-400/10 text-sky-100';

  return (
    <div className={`rounded-2xl border p-4 ${toneClasses}`}>
      <p className="font-medium">{title}</p>
      <p className="mt-2 text-sm leading-6 opacity-90">{message}</p>
    </div>
  );
}
