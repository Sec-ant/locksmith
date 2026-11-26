/** Effective pnpm settings. Set packageManager to an exact pnpm@version to override the version inferred from the manifest and bootstrap lock. */
export type Configuration = Record<string, unknown> & { packageManager?: string }
export type Manifest = Record<string, unknown>
export type Manifests = Record<string, Manifest>
export type Patches = Record<string, string>
export type DocumentKind = 'project' | 'environment'
export type LockInput = string | Lockfile

export class Lockfile {
  private constructor()
  toYaml(): Promise<string>
  document(kind: DocumentKind): Promise<Record<string, unknown> | null>
}

export type RegistryRequest =
  | { kind: 'version'; name: string; selector: string }
  | { kind: 'manifest'; name: string; version: string }
export type RegistryEvidence =
  | { request: RegistryRequest; manifest: Manifest; registry: string; includeTarball?: boolean }
  | { unavailableManifest: { name: string; version: string } }

export interface MergeContext {
  target: 'ours' | 'theirs'
  /** All final workspace members, including ".". */
  manifests: Manifests
  /** Base, ours, theirs. */
  configurations: [Configuration, Configuration, Configuration]
  configuration: Configuration
  patches?: Patches
  registry?: RegistryEvidence[]
}
export interface ManifestChange {
  importer: string
  before?: Manifest | null
  after?: Manifest | null
}
export interface ConvergeOptions {
  baselineConfiguration?: Configuration
  configuration?: Configuration
  patches?: Patches
  registry?: RegistryEvidence[]
}
export interface RepairContext {
  manifests: Manifests
  configuration: Configuration
  parents?: Array<{ yaml: string; configuration: Configuration }>
  patches?: Patches
  registry?: RegistryEvidence[]
}
export interface DependencyClosure {
  importers: string[]
  packages: string[]
  snapshots: string[]
}
export interface PeerIssue {
  snapshot: string
  peer: string
  wanted: string
  resolved: string | null
}
export interface Conflict {
  document: DocumentKind
  path: string[]
  kind: 'bothChanged' | 'deleteModify' | 'sharedDependency'
  base: unknown
  ours: unknown
  theirs: unknown
}
export type LockfileError = Error & (
  | { code: 'CONFLICTS'; details: { conflicts: Conflict[] } }
  | { code: 'INVALID_GRAPH'; details: { issues: Array<{ document: DocumentKind; path: string[]; message: string }> } }
  | { code: 'RESOLUTION_REQUIRED'; details: { path: string[]; reason: string; request: RegistryRequest | null } }
  | { code: 'INVALID_MANIFEST'; details: { importer: string } }
  | { code: 'MANIFEST_MISMATCH'; details: { importer: string; side: string; reason: string } }
  | { code: 'GLOBAL_CONFIGURATION_CHANGED'; details: { importer: string; field: string } }
  | { code: 'UNSUPPORTED_VERSION'; details: { version: unknown } }
  | { code: 'UNMET_PEERS'; details: { issues: PeerIssue[] } }
  | { code: 'INVALID_YAML' | 'INVALID_DOCUMENT' | 'WORKSPACE_INPUT' | 'SERIALIZATION_FAILED' | 'OUTPUT_MISMATCH'; details: null }
)

export function parse(yaml: string): Promise<Lockfile>
export function validate(lock: LockInput): Promise<void>
export function dependencyClosure(lock: LockInput, importer: string): Promise<DependencyClosure>
export interface Inspection {
  summary: { workspaces: number; packages: number; snapshots: number; reachableSnapshots: number }
  peerPolicy: 'semver' | 'pnpm9' | 'pnpm10' | 'pnpm11' | 'pnpm12' | 'unspecified'
  peers: Array<{
    status: 'missing' | 'outOfRange' | 'unknown' | 'allowed'
    requester: string
    peer: string
    wanted: string
    provider: string
    resolvedVersion: string | null
    disposition: 'accepted' | 'rejected' | 'unknown'
    reason: 'providerVersion' | 'opaqueSpecifier' | 'packageManager' | null
    exception: string | null
    bindings: Array<{ snapshot: string; reference: string | null }>
    matchingVersionsInLock: string[]
    /** Importers with a direct dependency edge to a requester snapshot. */
    directWorkspaces: string[]
    /** Importers that reach a requester through any recorded path, including direct edges. */
    reachableWorkspaces: string[]
    /** A shortest path from a workspace to the requester, or null when unreachable. */
    examplePath: string[] | null
  }>
  multiplicity: Array<{ name: string; releases: Array<{ id: string; version: string | null; snapshots: number }> }>
  externalLinks: Array<{ sourceKind: 'importer' | 'snapshot'; from: string; dependency: string; target: string }>
}
export function inspect(lock: LockInput, options?: { configuration?: Configuration; manifests?: Manifests }): Promise<Inspection>
/** Uses the same pnpm version matching policy as inspect, including when the version is unknown. */
export function inspectPeers(lock: LockInput, options?: { configuration?: Configuration; manifests?: Manifests }): Promise<PeerIssue[]>
/** Merge the branches against their ancestor. A conflict or unresolved selection rejects with a LockfileError. */
export function merge(base: LockInput, ours: LockInput, theirs: LockInput, context?: MergeContext): Promise<string>
/** For a pnpm version migration, include every final workspace manifest in changes. */
export function converge(baseline: LockInput, candidate: LockInput, changes: ManifestChange[], options?: ConvergeOptions): Promise<string>
/** Repair the dependencies reachable from the supplied projects. Use validate() to check all recorded references. */
export function repair(yaml: string, context: RepairContext): Promise<string>

export interface WorkspaceOptions {
  cwd?: string | URL
  allowNetwork?: boolean
  signal?: AbortSignal
}
/** Absolute path to the bundled Rust CLI, suitable for spawn() or a Git driver. */
export function getCliPath(): string
export function inspectWorkspace(options?: Pick<WorkspaceOptions, 'cwd' | 'signal'>): Promise<Inspection>
export function convergeWorkspace(ref: string, options?: WorkspaceOptions): Promise<string>
export function repairWorkspace(ref?: string, options?: WorkspaceOptions): Promise<string>
