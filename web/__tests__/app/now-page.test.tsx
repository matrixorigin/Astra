vi.mock("@/lib/runtime-client", async (importOriginal) => {
  const original = await importOriginal<typeof import("@/lib/runtime-client")>();
  return { ...original, requireRuntimeClient: vi.fn() };
});

import NowPage from "@/app/(workspace)/now/page";
import { requireRuntimeClient } from "@/lib/runtime-client";

const requireClient = vi.mocked(requireRuntimeClient);

beforeEach(() => vi.clearAllMocks());

test("loads one bounded keyset page through the Work SDK", async () => {
  const catalog = { schema_version: 1, entries: [], next_cursor: null } as const;
  const listWorks = vi.fn().mockResolvedValue(catalog);
  requireClient.mockResolvedValue({ sdk: { listWorks } } as never);

  const element = await NowPage({
    searchParams: Promise.resolve({
      before_created_at: "2026-08-01T00:00:00Z",
      before_work_id: "work-1",
    }),
  });

  expect(requireClient).toHaveBeenCalledWith({
    auth: "required",
    operation: "open Now",
  });
  expect(listWorks).toHaveBeenCalledWith({
    cursor: {
      created_at: "2026-08-01T00:00:00Z",
      work_id: "work-1",
    },
    limit: 20,
  });
  expect(element.props).toMatchObject({ page: catalog, isLatest: false });
});
