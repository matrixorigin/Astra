// @vitest-environment node

const cookieValues = new Map<string, string>();
const cookieStore = {
  get: vi.fn((key: string) => {
    const value = cookieValues.get(key);
    return value ? { value } : undefined;
  }),
  set: vi.fn((key: string, value: string) => {
    cookieValues.set(key, value);
  }),
  delete: vi.fn((key: string) => {
    cookieValues.delete(key);
  }),
};

vi.mock("next/headers", () => ({
  cookies: vi.fn(async () => cookieStore),
}));

vi.mock("next/navigation", () => ({
  redirect: vi.fn(),
}));

vi.mock("@/lib/auth/runtime-auth-client", () => ({
  runtimeExternalLogin: vi.fn(),
  runtimeExternalProviders: vi.fn(),
  runtimeLogin: vi.fn(),
  runtimeLogout: vi.fn(),
  runtimeMe: vi.fn(),
  runtimeRefresh: vi.fn(),
  runtimeRegister: vi.fn(),
}));

import { loginAction } from "@/lib/auth/actions";
import {
  runtimeExternalLogin,
  runtimeLogin,
} from "@/lib/auth/runtime-auth-client";

describe("auth actions external login", () => {
  beforeEach(() => {
    cookieValues.clear();
    vi.clearAllMocks();
    vi.mocked(runtimeLogin).mockResolvedValue({
      ok: true,
      data: { access_token: "internal-access", refresh_token: "internal-refresh" },
    });
    vi.mocked(runtimeExternalLogin).mockResolvedValue({
      ok: true,
      data: { access_token: "astra-access", refresh_token: "astra-refresh" },
    });
  });

  it("uses regular Astra login when auth_mode is not external", async () => {
    const form = new FormData();
    form.set("username", "alice");
    form.set("password", "secret");

    const result = await loginAction({ ok: false }, form);

    expect(result).toEqual({ ok: true });
    expect(runtimeExternalLogin).not.toHaveBeenCalled();
    expect(runtimeLogin).toHaveBeenCalledWith(expect.any(String), {
      username: "alice",
      password: "secret",
    });
    expect(cookieValues.get("astra_access_token")).toBe("internal-access");
    expect(cookieValues.get("astra_refresh_token")).toBe("internal-refresh");
  });

  it("uses external login when auth_mode is external", async () => {
    const form = new FormData();
    form.set("auth_mode", "external");
    form.set("provider_id", "moi");
    form.set("username", "admin");
    form.set("password", "admin");

    const result = await loginAction({ ok: false }, form);

    expect(result).toEqual({ ok: true });
    expect(runtimeLogin).not.toHaveBeenCalled();
    expect(runtimeExternalLogin).toHaveBeenCalledWith(expect.any(String), {
      provider_id: "moi",
      username: "admin",
      password: "admin",
      scope_id: undefined,
    });
    expect(cookieValues.get("astra_access_token")).toBe("astra-access");
    expect(cookieValues.get("astra_refresh_token")).toBe("astra-refresh");
  });
});
