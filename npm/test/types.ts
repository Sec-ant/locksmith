import { parse, merge, repair, inspect, convergeWorkspace, generateWorkspace, type HookRuntime, type MergeContext, type LockfileError } from 'es-locksmith';

export async function resolve(yaml: string, context: MergeContext): Promise<string> {
  const parsed = await parse(yaml);
  try {
    return await merge(parsed, yaml, parsed, context);
  } catch (error) {
    const failure = error as LockfileError;
    if (failure.code === 'RESOLUTION_REQUIRED' && failure.details.request) {
      return repair(yaml, { manifests: context.manifests, configuration: context.configuration });
    }
    throw error;
  }
}

export async function affectedWorkspaces(yaml: string): Promise<string[][]> {
  const report = await inspect(yaml);
  return report.peers.map(group => group.reachableWorkspaces);
}

export function inWorktree(cwd: URL) {
  return convergeWorkspace('origin/main', { cwd, allowNetwork: true });
}

export function fromManifests(cwd: URL, hookRuntime: HookRuntime): Promise<string> {
  return generateWorkspace({ cwd, hookRuntime, pnpmVersion: '10.34.9', exhaustive: true });
}
