import * as assert from 'node:assert/strict';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run():Promise<void> {
  const extension=vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');assert.ok(extension);
  const controller=await extension.activate(),folder=vscode.workspace.workspaceFolders?.[0];assert.ok(folder);await controller.newTask();
  const uri=vscode.Uri.joinPath(folder.uri,'Loop.cs'),source='class Loops {\n int Sum(int input) {\n  int result = 0;\n  for (int i = 0; i < input; i++) {\n   if (i == 2) { continue; }\n   result += i;\n  }\n  return result;\n }\n}\n';
  await vscode.workspace.fs.writeFile(uri,Buffer.from(source));const editor=await vscode.window.showTextDocument(uri);editor.selection=new vscode.Selection(1,0,8,2);await controller.capture();await controller.updatePlan({goal:'Schleifenverhalten verstehen',not_now:'Keine Codeänderung',acceptance:'Werte und Zweige vergleichen'});
  await controller.runSimulation({name:'Drei Durchläufe',version:1,inputs:{input:3},stubs:{},start_line:6});let session=controller.getState().simulation!;
  assert.equal(session.scenario.engine_version,2);assert.equal(session.baseline.result,1);assert.equal(session.baseline.outcome,'returned');assert.equal(session.baseline.steps.filter(step=>step.line===4&&step.kind==='condition').length,4);assert.equal(editor.document.getText(),source);
  const first=session.baseline.steps.findIndex(step=>step.line===6);await controller.focusSimulation(first,'baseline');assert.equal(editor.selection.active.line,5);
  await controller.runSimulation({name:'Zweite Variante',version:1,inputs:{input:5},stubs:{}});session=controller.getState().simulation!;assert.equal(session.scenario.version,2);assert.equal(session.baseline.result,8);assert.equal(controller.getState().simulationHistory?.at(-1)?.baseline.result,1);
  await controller.saveSimulationLearning(session.id,'Ein echter Test soll bei input=5 die Summe 8 prüfen.');assert.equal(controller.getState().decisions?.at(-1)?.kind,'test_idea');assert.equal(controller.getState().evidence,undefined);assert.equal(editor.document.getText(),source);
  console.log('PASS: neuer Host-Simulator nutzt versionierte Schleifensemantik, Zeilenfokus, getrennte Szenariovarianten und Testidee ohne Produktwrites oder falschen Prüfbeleg.');
}
