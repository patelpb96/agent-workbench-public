import { test } from 'node:test';
import assert from 'node:assert/strict';
import { TransportFaults } from './transport-faults.mjs';

test('unarmed gate passes the original call/reply through without capture', async () => {
  const gate = new TransportFaults(), request = { contract_version: 1 }, reply = { ok: { sample: true } };
  const bridge = { call: async (command, value) => {
    assert.equal(command, 'bootstrap_workspace'); assert.equal(value, request); return reply;
  } };
  assert.equal(await gate.forward(bridge, { command: 'bootstrap_workspace', request }), reply);
  assert.deepEqual(gate.records, []); assert.equal(gate.evidence.injected, 0);
});

test('one post-commit reply loss preserves private copies; evidence omits prompt', async () => {
  const gate = new TransportFaults(); let commits = 0;
  gate.dropNextReply('start_fake_run');
  const request = { request_id: 'unit-only', prompt: 'SYNTHETIC PRIVATE INPUT' };
  const bridge = { call: async () => ({ ok: { replayed: commits++ > 0 } }) };
  await assert.rejects(gate.forward(bridge, { command: 'start_fake_run', request }), /after native commit/);
  assert.equal(commits, 1); // Mock adapter unit check, not native durability evidence.
  const captured = gate.records; captured[0].request.prompt = 'tampered';
  assert.equal(gate.records[0].request.prompt, request.prompt);
  const result = await gate.forward(bridge, { command: 'start_fake_run', request });
  assert.equal(result.ok.replayed, true); assert.equal(gate.evidence.injected, 1);
  assert.equal(JSON.stringify(gate.evidence).includes(request.prompt), false);
});

test('unknown targets and rearming are refused; capture is bounded before forwarding', async () => {
  const gate = new TransportFaults();
  for (const command of ['shutdown', 'bootstrap_workspace', 'get_workspace_snapshot', 'list_run_events']) {
    assert.throws(() => gate.dropNextReply(command), /supported transport fault/);
  }
  gate.dropNextReply('save_layout');
  assert.throws(() => gate.conflictNextSave(), /supported transport fault/);
  let calls = 0; const bridge = { call: async () => { calls++; return { ok: { replayed: false } }; } };
  const call = { command: 'save_layout', request: { request_id: 'unit-only' } };
  await assert.rejects(gate.forward(bridge, call), /after native commit/);
  for (let i = 1; i < 8; i++) await gate.forward(bridge, call);
  await assert.rejects(gate.forward(bridge, call), /capture budget/);
  assert.equal(calls, 8); assert.equal(gate.records.length, 8);
});

test('native rejection is not replaced by a fabricated committed-reply loss', async () => {
  const gate = new TransportFaults(); gate.dropNextReply('save_layout');
  const reply = { error: { code: 'validation_failed' } };
  const bridge = { call: async () => reply };
  await assert.rejects(gate.forward(bridge, { command: 'save_layout', request: {} }), /expected a successful native commit/);
  assert.equal(gate.evidence.injected, 0);
  assert.equal(gate.evidence.receipts[0].error_code, 'validation_failed');
});

for (const command of ['create_session', 'cancel_run']) {
  test(command + ' supports one committed-reply loss without exposing request fields', async () => {
    const gate = new TransportFaults(); gate.dropNextReply(command);
    const privateValue = command === 'create_session' ? 'SYNTHETIC PRIVATE LABEL' : 'synthetic-target';
    const request = { request_id: 'unit-only', [command === 'create_session' ? 'label' : 'run_id']: privateValue };
    let calls = 0;
    const bridge = { call: async (actual, value) => {
      assert.equal(actual, command); assert.deepEqual(value, request);
      return { ok: { replayed: calls++ > 0 } }; // Mock adapter check; native receipts are verified in the DOM suite.
    } };
    await assert.rejects(gate.forward(bridge, { command, request }), /after native commit/);
    assert.equal((await gate.forward(bridge, { command, request })).ok.replayed, true);
    assert.equal(calls, 2); assert.equal(gate.evidence.injected, 1);
    assert.equal(JSON.stringify(gate.evidence).includes(privateValue), false);
  });
}
