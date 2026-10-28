import { redirect } from "next/navigation";
import type { ReactNode } from "react";
import { AppShell } from "@/components/app/app-shell";
import { getCurrentUser } from "@/lib/auth/actions";

export default async function WorkspaceLayout({
  children,
}: {
  children: ReactNode;
}) {
  if (!(await getCurrentUser())) redirect("/login");

  return <AppShell>{children}</AppShell>;
}
