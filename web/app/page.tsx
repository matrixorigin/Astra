import { LandingScreen } from "@/components/app/landing-screen";
import { HomeScreen } from "@/components/app/home-screen";
import { AppShell } from "@/components/app/app-shell";
import { getCurrentUser } from "@/lib/auth/actions";

export default async function HomePage() {
  const user = await getCurrentUser();
  if (!user) {
    return <LandingScreen />;
  }

  return (
    <AppShell>
      <HomeScreen />
    </AppShell>
  );
}
