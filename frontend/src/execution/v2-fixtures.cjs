'use strict';
// Test-only: project the generator-owned frozen v1 fake fixtures onto the v2
// wire shape, mirroring native stored.rs projection (contract 2, null
// profile/model, null extra usage counters, Codex not configured). Fixture
// files themselves stay v1 and byte-preserved.
function upgrade(value) {
  if (Array.isArray(value)) return value.map(upgrade);
  if (value === null || typeof value !== 'object') return value;
  const out = {};
  for (const [key, item] of Object.entries(value)) out[key] = upgrade(item);
  if (out.contract_version === 1) out.contract_version = 2;
  if ('enqueue_order' in out && 'last_seq' in out) {
    if (!('profile_id' in out)) out.profile_id = null;
    if (!('model_id' in out)) out.model_id = null;
  }
  if ('estimated' in out && 'input_tokens' in out) {
    for (const key of ['cached_input_tokens', 'cache_write_input_tokens', 'reasoning_output_tokens']) {
      if (!(key in out)) out[key] = null;
    }
  }
  if ('execution_available' in out && 'workspace' in out && !('codex_availability' in out)) {
    out.codex_availability = 'not_configured';
  }
  return out;
}
module.exports = { upgrade };
