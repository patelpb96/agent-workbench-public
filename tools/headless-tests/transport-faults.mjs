// Trusted Node-test controls only. No imports into the page or production app.
// Business replies, commits, receipts and conflicts always come from native code.
import { randomUUID } from 'node:crypto';
const copy = value => JSON.parse(JSON.stringify(value));
export class TransportFaults {
  #plan = null; #consumed = false; #injected = 0; #records = []; #external = null;
  #arm(mode, command) {
    if (this.#plan || !['create_session', 'start_fake_run', 'cancel_run', 'save_layout'].includes(command)) {
      throw new Error('Only one supported transport fault may be armed per test.');
    }
    this.#plan = { mode, command };
  }
  dropNextReply(command) { this.#arm('drop_after_commit', command); }
  conflictNextSave() { this.#arm('competing_native_save', 'save_layout'); }
  get records() { return copy(this.#records); } // Private synthetic inputs; never attach these.
  get external() { return copy(this.#external); }
  get evidence() {
    return { mode: this.#plan?.mode ?? null, command: this.#plan?.command ?? null,
      injected: this.#injected, forwarded: this.#records.length,
      receipts: this.#records.map(({ request, reply }) => ({ request_id: request.request_id,
        replayed: reply?.ok?.replayed ?? null, error_code: reply?.error?.code ?? null })),
      external_revision: this.#external?.reply.layout.revision ?? null };
  }
  async forward(bridge, { command, request }) {
    const logical = typeof command === 'string' ? command.replace(/_v2$/, '') : command;
    if (!this.#plan || logical !== this.#plan.command) return bridge.call(command, request);
    if (this.#records.length >= 8) throw new Error('Transport-fault capture budget exceeded.');
    const record = { request: copy(request), reply: null };
    this.#records.push(record);
    const inject = !this.#consumed;
    this.#consumed = true; // Never repeat a fault, even if its native operation fails.
    if (inject && this.#plan.mode === 'competing_native_save') {
      const base = await bridge.snapshot();
      if (!base.layout || request.workspace_id !== base.workspace.workspace_id ||
          request.expected_revision !== base.layout.revision) {
        throw new Error('Conflict injection requires the actual current native base.');
      }
      const competing = { contract_version: 2, workspace_id: base.workspace.workspace_id,
        request_id: randomUUID(), expected_revision: base.layout.revision,
        layout: { ...base.layout, theme: base.layout.theme === 'light' ? 'dark' : 'light' } };
      const reply = await bridge.invoke('save_layout', competing);
      this.#external = { request: copy(competing), reply: copy(reply) };
      this.#injected++;
    }
    const reply = await bridge.call(command, request);
    record.reply = copy(reply);
    if (inject && this.#plan.mode === 'drop_after_commit') {
      if (!('ok' in reply)) throw new Error('Reply-loss injection expected a successful native commit.');
      this.#injected++;
      throw new Error('SIMULATED transport reply lost after native commit.');
    }
    return reply;
  }
}
