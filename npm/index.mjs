import api from './index.cjs';
export default api;
export const {
  Lockfile, parse, validate, merge, converge, repair, dependencyClosure, inspectPeers,
  getCliPath, convergeWorkspace, repairWorkspace,
} = api;
