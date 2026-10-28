import { act, fireEvent, render, screen } from "@testing-library/react";
import LoginPage from "@/app/login/page";

vi.mock("next/navigation", () => ({
  useRouter: () => ({ push: vi.fn() }),
  useSearchParams: () => new URLSearchParams(),
}));
vi.mock("@/lib/auth/actions", () => ({ loginAction: vi.fn() }));

afterEach(() => vi.unstubAllGlobals());

it("keeps the sign-in form available when previous-session logout fails", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => new Response("unavailable", { status: 503 })));
  render(<LoginPage />);
  fireEvent.click(screen.getByRole("button", { name: "Sign out previous browser session" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("Could not sign out");
  expect(screen.getByLabelText("Username")).toBeEnabled();
  expect(fetch).toHaveBeenCalledWith("/api/runtime-auth/logout", { method: "POST" });
});

it("prevents signing in while the previous selection is still being signed out", async () => {
  let finish!: (response: Response) => void;
  vi.stubGlobal("fetch", vi.fn(() => new Promise<Response>((resolve) => { finish = resolve; })));
  render(<LoginPage />);
  fireEvent.click(screen.getByRole("button", { name: "Sign out previous browser session" }));
  expect(screen.getByRole("button", { name: /^Sign in$/ })).toBeDisabled();
  await act(async () => finish(new Response(null, { status: 200 })));
  expect(screen.getByRole("status")).toHaveTextContent("Previous browser session signed out");
  expect(screen.getByRole("button", { name: /^Sign in$/ })).toBeEnabled();
});

it("lets the user select a new runtime without revoking the old one", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => new Response(null, { status: 200 })));
  render(<LoginPage />);
  fireEvent.change(screen.getByLabelText(/runtime URL/i), { target: { value: "https://new.example/" } });
  fireEvent.click(screen.getByRole("button", { name: "Select runtime" }));
  expect(await screen.findByRole("status")).toHaveTextContent("Runtime selected");
  expect(fetch).toHaveBeenCalledWith("/api/runtime-config", expect.objectContaining({
    method: "POST", body: JSON.stringify({ apiUrl: "https://new.example/" }),
  }));
});
