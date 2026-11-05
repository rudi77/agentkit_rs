import * as assert from 'node:assert/strict';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';
export async function run():Promise<void>{
  const extension=vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');assert.ok(extension);
  const controller=await extension.activate();const state=controller.getState();
  assert.equal(state.changePlan?.steps.length,2);
  assert.equal(state.changePlan?.not_now,'Menschliche Ergänzung: kein Bulk-Delete');
  assert.equal(state.exploration?.stale,true);
  assert.equal(state.changePlan?.stale,true);
  assert.ok(state.exploration?.instruction_paths.includes('AGENTS.md'));
  assert.ok(state.exploration?.sources.some(source=>source.kind==='test'));
  assert.ok(!JSON.stringify(state).includes('SECRET_SENTINEL_NOT_FOR_MODEL'));
  console.log('PASS: gemeinsamer Änderungsplan, Regeln und Quellen überleben den Host-Neustart; alte Bindung bleibt als veraltet markiert');
}
