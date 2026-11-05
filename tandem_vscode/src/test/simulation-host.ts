import * as assert from 'node:assert/strict';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run():Promise<void> {
  const extension=vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');assert.ok(extension);
  const controller=await extension.activate(),folder=vscode.workspace.workspaceFolders?.[0];assert.ok(folder);
  await vscode.workspace.getConfiguration('tandem').update('provider','demo',vscode.ConfigurationTarget.Global);controller.stop();await controller.newTask();
  const uri=vscode.Uri.joinPath(folder.uri,'Simulate.cs');
  const source='class Demo {\n public static string Normalize(string input) {\n  return input;\n }\n}\n';
  await vscode.workspace.fs.writeFile(uri,Buffer.from(source));
  const editor=await vscode.window.showTextDocument(uri);
  editor.selection=new vscode.Selection(new vscode.Position(1,1),new vscode.Position(3,2));
  await controller.capture();await controller.updatePlan({goal:'Eingabe prüfen',not_now:'Keine Nebenarbeiten',acceptance:'Randfälle verstehen'});
  const scenario={name:'Leerzeichen',version:1,inputs:{input:'  Wert  '},stubs:{}};
  await controller.runSimulation(scenario);
  let session=controller.getState().simulation;assert.ok(session);assert.equal(session.baseline.result,'  Wert  ');assert.equal(session.baseline.outcome,'returned');
  assert.equal(editor.document.getText(),source,'Simulation darf den Produktcode nicht ändern');
  await vscode.commands.executeCommand('tandem.simulate');
  await controller.focusSimulation(1,'baseline');
  await assert.rejects(controller.focusSimulation(999,'baseline'),/existiert/);
  await controller.send('Kleinsten Vorschlag erstellen','pair');
  await controller.runSimulation(scenario,true);session=controller.getState().simulation;assert.ok(session?.proposed);
  assert.equal(session.proposed.outcome,'unknown','Nicht modellierte Stringmethode muss Unknown bleiben');
  assert.equal(session.scenario.version,2);assert.ok(controller.getState().simulationHistory?.length);
  await controller.focusSimulation(1,'proposed');
  assert.ok(vscode.window.visibleTextEditors.some(item=>item.document.uri.scheme==='tandem-preview'));
  assert.equal(editor.document.getText(),source,'Auch der Vergleich übernimmt keinen Patch');
  await controller.updatePlan({goal:'Neue Entscheidung',not_now:'Keine Änderungen',acceptance:'Besprechen'});
  assert.equal(controller.getState().simulation?.stale,true);
  await assert.rejects(controller.focusSimulation(0,'baseline'),/aktuellen/);
  await controller.stopSimulation();
  console.log('PASS: echte Host-Simulation und Patchvergleich ohne Produktwrites, Snapshotbindung, historische Szenarien, Editorfokus und Invalidierung bei Planänderung.');
}
