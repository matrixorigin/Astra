// @vitest-environment node

vi.mock("@/lib/runtime-client", async (importOriginal) => {
  const original = await importOriginal<typeof import("@/lib/runtime-client")>();
  return { ...original, requireRuntimeClient: vi.fn() };
});

import {
  ASTRA_WORK_API_MAJOR,
  ASTRA_WORK_API_MAJOR_HEADER,
} from "@astra/sdk";
import { NextRequest } from "next/server";
import { POST } from "@/app/api/works/[workId]/branches/[branchId]/turns/route";
import { requireRuntimeClient } from "@/lib/runtime-client";

const requireClient = vi.mocked(requireRuntimeClient);
const params = Promise.resolve({ workId: "work-1", branchId: "branch-1" });

function request(body: unknown) {
  return new NextRequest(
    "http://localhost/api/works/work-1/branches/branch-1/turns",
    { method: "POST", body: JSON.stringify(body) },
  );
}

beforeEach(() => vi.clearAllMocks());

test("rejects non-exact turn input before contacting the runtime", async () => {
  const extraField = await POST(
    request({
      request_id: "turn-1",
      attachment_id: "attachment-1",
      message: "Continue",
      model: "hidden-override",
    }),
    { params },
  );
  const missingAttachment = await POST(
    request({ request_id: "turn-1", message: "Continue" }),
    { params },
  );

  expect(extraField.status).toBe(400);
  expect((await extraField.json()).code).toBe("invalid_work_turn_request");
  expect(missingAttachment.status).toBe(400);
  expect((await missingAttachment.json()).code).toBe("invalid_work_turn_request");
  expect(requireClient).not.toHaveBeenCalled();
});

test("rejects an oversized declared body before reading or contacting the runtime", async () => {
  const oversized = new NextRequest(
    "http://localhost/api/works/work-1/branches/branch-1/turns",
    {
      method: "POST",
      headers: { "content-length": String(264 * 1024) },
      body: JSON.stringify({
        request_id: "turn-1",
        attachment_id: "attachment-1",
        message: "Continue",
      }),
    },
  );

  const response = await POST(oversized, { params });

  expect(response.status).toBe(413);
  expect(requireClient).not.toHaveBeenCalled();
});

test("streams the authenticated Server-owned turn without buffering it", async () => {
  const backend = new Response(
    'data: {"type":"work_turn_started","schema_version":1,"work_id":"work-1","branch_id":"branch-1","run_id":"run-1"}\n\n',
    { status: 200, headers: { "content-type": "text/event-stream" } },
  );
  const fetchResponse = vi.fn().mockResolvedValue(backend);
  requireClient.mockResolvedValue({ fetchResponse } as never);
  const input = {
    request_id: "turn-1",
    attachment_id: "attachment-1",
    message: "Continue the Work",
  };

  const response = await POST(request(input), { params });

  expect(requireClient).toHaveBeenCalledWith({
    auth: "required",
    operation: "continue Work",
  });
  expect(fetchResponse).toHaveBeenCalledWith(
    "/v1/works/work-1/branches/branch-1/turns",
    expect.objectContaining({
      method: "POST",
      auth: "required",
      body: JSON.stringify(input),
      headers: {
        Accept: "text/event-stream",
        "Content-Type": "application/json",
        [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR,
      },
    }),
  );
  expect(response.status).toBe(200);
  expect(response.headers.get("content-type")).toBe("text/event-stream");
  expect(await response.text()).toContain('"type":"work_turn_started"');
});

test("preserves typed backend conflicts for deterministic client recovery", async () => {
  const backendError = {
    code: "writer_conflict",
    category: "conflict",
    retryable: false,
    action_hints: ["refresh_work"],
  };
  const fetchResponse = vi.fn().mockResolvedValue(
    Response.json(backendError, { status: 409 }),
  );
  requireClient.mockResolvedValue({ fetchResponse } as never);

  const response = await POST(
    request({
      request_id: "turn-1",
      attachment_id: "attachment-1",
      message: "Continue",
    }),
    { params },
  );

  expect(response.status).toBe(409);
  expect(await response.json()).toEqual(backendError);
});

test("classifies transport failure as availability, not invalid identity", async () => {
  const fetchResponse = vi.fn().mockRejectedValue(new TypeError("fetch failed"));
  requireClient.mockResolvedValue({ fetchResponse } as never);

  const response = await POST(
    request({
      request_id: "turn-1",
      attachment_id: "attachment-1",
      message: "Continue",
    }),
    { params },
  );

  expect(response.status).toBe(503);
  expect(await response.json()).toMatchObject({
    code: "work_turn_unavailable",
    category: "availability",
    retryable: true,
  });
});
