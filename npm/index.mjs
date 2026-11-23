import api from './index.cjs';
export default api;
export const {
  Lockfile, parse, validate, merge, converge, repair, dependencyClosure, inspect, inspectPeers,
  getCliPath, generateWorkspace, inspectWorkspace, convergeWorkspace, repairWorkspace,
} = api;
