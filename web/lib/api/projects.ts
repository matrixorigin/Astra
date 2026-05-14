import { requestJson, toQuery } from '@/lib/api/request';
import type {
  CreateProjectRequest,
  KnowledgeFile,
  ProjectDetail,
  ProjectListResponse,
  ProjectSummary,
} from '@/lib/api/types';

export function listProjects(params: {
  q?: string;
  sort?: 'activity' | 'created' | 'name';
  cursor?: string | null;
  limit?: number;
}) {
  return requestJson<ProjectListResponse>(`/api/projects${toQuery(params)}`);
}

export function createProject(payload: CreateProjectRequest) {
  return requestJson<{ project: ProjectSummary }>('/api/projects', {
    method: 'POST',
    body: JSON.stringify(payload),
  });
}

export function getProject(projectId: string) {
  return requestJson<ProjectDetail>(`/api/projects/${encodeURIComponent(projectId)}`);
}

export function updateProject(projectId: string, payload: Partial<CreateProjectRequest>) {
  return requestJson<ProjectDetail>(`/api/projects/${encodeURIComponent(projectId)}`, {
    method: 'PUT',
    body: JSON.stringify(payload),
  });
}

export function setProjectStar(projectId: string, starred: boolean) {
  return requestJson<{ starred: boolean }>(`/api/projects/${encodeURIComponent(projectId)}/star`, {
    method: starred ? 'POST' : 'DELETE',
  });
}

export function uploadProjectFile(projectId: string, file: File) {
  const form = new FormData();
  form.set('file', file);
  return fetch(`/api/projects/${encodeURIComponent(projectId)}/files`, {
    method: 'POST',
    body: form,
  }).then(async (response) => {
    if (!response.ok) {
      throw new Error(`${response.status} ${response.statusText}`);
    }
    return (await response.json()) as { file: KnowledgeFile };
  });
}
