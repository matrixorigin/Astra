"use client";

import { ErrorRecovery } from "@/components/app/error-recovery";

export default function GlobalError() {
  return (
    <html lang="en">
      <body>
        <ErrorRecovery />
      </body>
    </html>
  );
}
