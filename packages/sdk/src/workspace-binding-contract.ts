import type {
  ChatConfig,
  ChatRequest,
  WorkspaceBindingRequest,
} from "./types";
import type { WorkspaceBindingRequest as RootWorkspaceBindingRequest } from "./index";
import type { WorkspaceBindingRequest as ReactWorkspaceBindingRequest } from "./react";

// This file is included by the SDK consumer typecheck. Keep these assertions
// compile-time only: request types must not drift back to the event shape.
const canonicalRequest: WorkspaceBindingRequest = {
  kind: "edge_workspace",
  display_name: "Edge workspace",
  root: "/repo",
  source: { kind: "edge_path", path: "/repo" },
  authority: "read_write",
};

const rootRequest: RootWorkspaceBindingRequest = canonicalRequest;
const reactRequest: ReactWorkspaceBindingRequest = canonicalRequest;
const chatConfig: ChatConfig = { workspaceBinding: canonicalRequest };
const chatRequest: ChatRequest = {
  message: "inspect the workspace",
  modelSelection: { offeringId: "model" },
  workspaceBinding: canonicalRequest,
};

void rootRequest;
void reactRequest;
void chatConfig;
void chatRequest;

const invalidCwdRequest: WorkspaceBindingRequest = {
  kind: "edge_workspace",
  // @ts-expect-error `cwd` belongs to observed runtime events, not requests.
  cwd: "/repo",
};

const invalidFallbackRequest: WorkspaceBindingRequest = {
  kind: "edge_workspace",
  // @ts-expect-error fallback policy is an execution result field, not a request field.
  fallback_policy: "disabled",
};

void invalidCwdRequest;
void invalidFallbackRequest;
