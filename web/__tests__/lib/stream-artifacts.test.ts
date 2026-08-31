import {
  artifactsFromToolCallEnd,
  mergeChatArtifacts,
} from "@/lib/api/stream-artifacts";

describe("stream artifact projection", () => {
  it("projects file metadata and download identity from tool_call_end", () => {
    expect(
      artifactsFromToolCallEnd({
        type: "tool_call_end",
        artifacts: [
          {
            artifact_id: "artifact-1",
            name: "report.pdf",
            type: "file",
            data: {
              file_id: "file-1",
              filename: "report.pdf",
              mime_type: "application/pdf",
              byte_size: 42,
              download_url: "/api/files/file-1",
            },
            metadata: { source: "moi_cli" },
          },
        ],
      }),
    ).toEqual([
      expect.objectContaining({
        id: "artifact-1",
        kind: "file",
        source: "moi_cli",
        filename: "report.pdf",
        contentType: "application/pdf",
        sizeBytes: 42,
        downloadUrl: "/api/files/file-1",
      }),
    ]);
  });

  it("rejects executable download schemes and malformed artifacts", () => {
    expect(
      artifactsFromToolCallEnd({
        type: "tool_call_end",
        artifacts: [
          {
            artifact_id: "artifact-1",
            type: "file",
            data: { download_url: "javascript:alert(1)" },
          },
          { type: "file" },
        ],
      }),
    ).toEqual([
      expect.objectContaining({ id: "artifact-1", downloadUrl: null }),
    ]);
  });

  it("drops malformed inline base64 while preserving artifact download metadata", () => {
    expect(
      artifactsFromToolCallEnd({
        type: "tool_call_end",
        artifacts: [
          {
            artifact_id: "artifact-1",
            type: "file",
            name: "report.pdf",
            data: {
              encoding: "base64",
              data: "!",
              download_url: "/api/files/file-1",
            },
          },
        ],
      }),
    ).toEqual([
      expect.objectContaining({
        id: "artifact-1",
        filename: "report.pdf",
        downloadUrl: "/api/files/file-1",
        content: null,
      }),
    ]);
  });

  it("keeps valid padded and unpadded inline base64", () => {
    const artifacts = artifactsFromToolCallEnd({
      type: "tool_call_end",
      artifacts: [
        {
          artifact_id: "padded",
          type: "file",
          data: { encoding: "base64", data: "YQ==" },
        },
        {
          artifact_id: "unpadded",
          type: "file",
          data: { encoding: "base64", data: "YWI" },
        },
      ],
    });

    expect(artifacts.map((artifact) => artifact.content)).toEqual([
      { encoding: "base64", data: "YQ==" },
      { encoding: "base64", data: "YWI" },
    ]);
  });

  it("accumulates artifacts by identity without duplicating retries", () => {
    expect(
      mergeChatArtifacts(
        [{ id: "artifact-1", kind: "file", title: "old" }],
        [
          { id: "artifact-1", kind: "file", title: "new" },
          { id: "artifact-2", kind: "text" },
        ],
      ),
    ).toEqual([
      { id: "artifact-1", kind: "file", title: "new" },
      { id: "artifact-2", kind: "text" },
    ]);
  });
});
