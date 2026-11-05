import * as assert from 'node:assert/strict';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run():Promise<void> {
  const extension=vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');assert.ok(extension);
  const controller=await extension.activate(),folder=vscode.workspace.workspaceFolders?.[0];assert.ok(folder);await controller.newTask();
  const uri=vscode.Uri.joinPath(folder.uri,'DirectSimulation.cs'),code='class Rules { int Read(int input) { return input + 1; } }';
  await vscode.workspace.fs.writeFile(uri,Buffer.from(code));const editor=await vscode.window.showTextDocument(uri);editor.selection=new vscode.Selection(0,0,0,code.length);await controller.capture();await controller.updatePlan({goal:'Methode verstehen',not_now:'Keine Nebenarbeit',acceptance:'Randfall prüfen'});
  await controller.runSimulation({name:'Direkt am Code',version:1,inputs:{input:2},stubs:{}});assert.equal(controller.getState().simulation?.baseline.result,3);assert.equal(controller.getState().exploration,undefined);assert.equal(controller.getState().changePlan,undefined);
  await editor.edit(edit=>edit.insert(new vscode.Position(0,0),'// Meine Änderung\n'));assert.equal(controller.getState().simulation?.stale,true,'Direkte Codearbeit ohne Repository-Plan muss den Trace sofort invalidieren');await assert.rejects(controller.focusSimulation(0,'baseline'),/aktuellen/);
  assert.ok(editor.document.getText().startsWith('// Meine Änderung'));assert.equal(controller.getState().evidence,undefined);
  console.log('PASS: menschlicher Edit invalidiert direkte Simulation auch ohne Repository-Plan sofort; historische Navigation gesperrt und menschlicher Code erhalten.');
}
