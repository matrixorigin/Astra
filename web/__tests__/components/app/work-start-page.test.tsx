vi.mock("@/app/(workspace)/works/actions", () => ({
  startWorkAction: vi.fn(),
}));

const push = vi.fn();
vi.mock("next/navigation", () => ({
  useRouter: () => ({ push }),
}));

import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { startWorkAction } from "@/app/(workspace)/works/actions";
import { WorkStartPage } from "@/components/app/work-start-page";

const startWork = vi.mocked(startWorkAction);

beforeEach(() => {
  vi.clearAllMocks();
  Object.defineProperty(globalThis.crypto, "randomUUID", {
    configurable: true,
    value: vi.fn(() => "00000000-0000-4000-8000-000000000021"),
  });
});

test("starts Work from a user goal and navigates by canonical work_id", async () => {
  startWork.mockResolvedValue({ ok: true, workId: "work-1", branchId: "branch-1" });
  render(<WorkStartPage />);

  fireEvent.change(screen.getByRole("textbox", { name: "Work goal" }), {
    target: { value: "  Ship a reliable change  " },
  });
  fireEvent.click(screen.getByRole("button", { name: "Start Work" }));

  await waitFor(() => expect(startWork).toHaveBeenCalledTimes(1));
  expect(startWork).toHaveBeenCalledWith({
    goal: "Ship a reliable change",
    requestId: "web-start-work:00000000-0000-4000-8000-000000000021",
  });
  expect(push).toHaveBeenCalledWith("/works/work-1");
});

test("retries an uncertain Start Work response with the same action identity", async () => {
  startWork
    .mockResolvedValueOnce({
      ok: false,
      status: 503,
      code: "work_write_unavailable",
      retryable: true,
    })
    .mockResolvedValueOnce({ ok: true, workId: "work-1", branchId: "branch-1" });
  render(<WorkStartPage />);
  fireEvent.change(screen.getByRole("textbox", { name: "Work goal" }), {
    target: { value: "Ship a reliable change" },
  });

  fireEvent.click(screen.getByRole("button", { name: "Start Work" }));
  expect(await screen.findByText(/safely try again/i)).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Start Work" }));

  await waitFor(() => expect(startWork).toHaveBeenCalledTimes(2));
  expect(startWork.mock.calls[0]?.[0].requestId).toBe(
    startWork.mock.calls[1]?.[0].requestId,
  );
});
