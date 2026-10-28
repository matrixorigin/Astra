import { redirect } from "next/navigation";
import { headers } from "next/headers";
import type { ReactNode } from "react";
import { AppShell } from "@/components/app/app-shell";
import { getCurrentUser } from "@/lib/auth/actions";

export default async function WorkspaceLayout({
  children,
}: {
  children: ReactNode;
}) {
  if (!(await getCurrentUser())) {
    const path = (await headers()).get("x-astra-request-path") ?? "/overview";
    redirect(`/login?${new URLSearchParams({ next: path })}`);
  }

  return <AppShell>{children}</AppShell>;
}
