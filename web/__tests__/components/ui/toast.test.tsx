import { act, render, screen } from "@testing-library/react";
import { useEffect, useState } from "react";
import { ToastProvider, useToast } from "@/components/ui/toast";

function ToastHarness() {
  const [renders, setRenders] = useState(0);
  const { addToast } = useToast();

  useEffect(() => {
    addToast("Saved", "info", 1000);
  }, [addToast]);

  useEffect(() => {
    const timer = window.setInterval(() => {
      setRenders((value) => value + 1);
    }, 16);
    return () => window.clearInterval(timer);
  }, []);

  return <div>renders {renders}</div>;
}

describe("ToastProvider", () => {
  it("auto-dismisses toasts even while children re-render frequently", () => {
    vi.useFakeTimers();
    try {
      render(
        <ToastProvider>
          <ToastHarness />
        </ToastProvider>,
      );

      expect(screen.getByText("Saved")).toBeInTheDocument();
      act(() => {
        vi.advanceTimersByTime(1300);
      });

      expect(screen.queryByText("Saved")).not.toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });
});
