import * as assert from 'node:assert/strict';
import * as vscode from 'vscode';
import { spawnSync } from 'node:child_process';
import type { TandemController } from '../extension';

export async function run():Promise<void> {
  const extension=vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');assert.ok(extension);
  const controller=await extension.activate(),folder=vscode.workspace.workspaceFolders?.[0];assert.ok(folder);
  await vscode.workspace.getConfiguration('tandem').update('provider','demo',vscode.ConfigurationTarget.Global);controller.stop();await controller.newTask();
  const uri=vscode.Uri.joinPath(folder.uri,'Worker.cs'),source='class Demo { public static string Normalize(string input) { return input; } }\n';
  await vscode.workspace.fs.writeFile(uri,Buffer.from(source));const editor=await vscode.window.showTextDocument(uri);
  editor.selection=new vscode.Selection(0,0,1,0);await controller.capture();await controller.updatePlan({goal:'Randzeichen entfernen',not_now:'Keine weiteren Dateien',acceptance:'Leerzeichenfall mit gezieltem Test prüfen'});
  await controller.prepareFactoryJob();let record=controller.getState().factoryJob!;assert.equal(record.status,'prepared');
  const child=spawnSync(record.command[0],record.command.slice(1),{encoding:'utf8',windowsHide:true,timeout:30000});
  assert.equal(child.status,0,child.stderr);const result=JSON.parse(child.stdout);assert.equal(result.status,'proposal');assert.equal(result.workspace_check,'disk_hash_matched');assert.equal(result.execution_authorization,'none');
  assert.equal(editor.document.getText(),source);assert.equal(controller.getState().proposal,undefined);
  const resultUri=vscode.Uri.joinPath(folder.uri,'worker-result.json');await vscode.workspace.fs.writeFile(resultUri,Buffer.from(JSON.stringify(result)));
  const forged=structuredClone(result);forged.result.patch.new_text='return input; } class Ungefragt {';
  await vscode.workspace.fs.writeFile(resultUri,Buffer.from(JSON.stringify(forged)));await assert.rejects(controller.importFactoryResult(resultUri),/nicht vereinbartes Symbol/);assert.equal(controller.getState().proposal,undefined);
  await vscode.workspace.fs.writeFile(resultUri,Buffer.from(JSON.stringify(result)));await controller.importFactoryResult(resultUri);
  assert.equal(controller.getState().factoryJob?.status,'imported');assert.equal(controller.getState().proposal?.status,'ready');assert.equal(controller.getState().evidence,undefined,'Worker darf keinen Testbeleg erzeugen');assert.equal(editor.document.getText(),source);
  await assert.rejects(controller.importFactoryResult(resultUri),/bereits verarbeitet/);
  await controller.prepareFactoryJob();await controller.runFactoryJob();record=controller.getState().factoryJob!;assert.equal(record.status,'result_ready');assert.ok(record.result_uri);
  await controller.importFactoryResult(vscode.Uri.parse(record.result_uri!));assert.equal(controller.getState().proposal?.status,'ready');assert.equal(editor.document.getText(),source);
  await controller.prepareFactoryJob();await controller.updatePlan({goal:'Anderes Ziel',not_now:'Keine Nebenarbeit',acceptance:'Neu prüfen'});assert.equal(controller.getState().factoryJob?.status,'stale');await assert.rejects(controller.runFactoryJob(),/vorbereiten/);
  await controller.prepareFactoryJob();await editor.edit(edit=>edit.insert(new vscode.Position(0,0),'// Edit\n'));await assert.rejects(controller.runFactoryJob());assert.equal(controller.getState().factoryJob?.status,'stale');
  console.log('PASS: eigenständiger Demo-Worker und JSON-RPC-Worker, geprüfter Import ohne Produkt-Schreibzugriff/Testbehauptung, abgewehrte Zusatzklasse, Doppelimport und veralteter Plan/Code.');
}
