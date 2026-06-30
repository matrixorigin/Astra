// @vitest-environment jsdom

import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

vi.mock("next/navigation", () => ({
  useRouter: () => ({ push: vi.fn() }),
  useSearchParams: () => new URLSearchParams(),
}));

vi.mock("@/lib/auth/actions", () => ({
  loginAction: vi.fn(async () => ({ ok: false })),
  externalProvidersAction: vi.fn(async () => ({
    ok: true,
    providers: [
      { id: "moi", display_name: "MOI", credential_type: "password" },
    ],
  })),
}));

import { externalProvidersAction } from "@/lib/auth/actions";
import LoginPage from "@/app/login/page";

describe("login page external auth", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("defaults to Astra user login without loading external providers", () => {
    render(<LoginPage />);

    expect(
      screen.getByRole("button", { name: "Astra user" }),
    ).toBeInTheDocument();
    expect(screen.queryByRole("combobox", { name: "Provider" })).toBeNull();
    expect(externalProvidersAction).not.toHaveBeenCalled();
  });

  it("loads and renders external auth providers", async () => {
    const user = userEvent.setup();
    render(<LoginPage />);

    await user.click(screen.getByRole("button", { name: "External user" }));

    const provider = await screen.findByRole("combobox", { name: "Provider" });
    expect(provider).toHaveValue("moi");
    expect(screen.getByRole("option", { name: "MOI" })).toBeInTheDocument();
  });
});
