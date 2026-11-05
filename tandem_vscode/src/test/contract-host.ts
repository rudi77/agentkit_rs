import * as assert from 'node:assert/strict';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run():Promise<void> {
  const extension=vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');assert.ok(extension);
  const controller=await extension.activate(),folder=vscode.workspace.workspaceFolders?.[0];assert.ok(folder);
  await vscode.workspace.getConfiguration('tandem').update('provider','demo',vscode.ConfigurationTarget.Global);controller.stop();await controller.newTask();
  const uri=vscode.Uri.joinPath(folder.uri,'Contract.cs'),source='class Demo { public static string Normalize(string input) { return input; } }\n';
  await vscode.workspace.fs.writeFile(uri,Buffer.from(source));const editor=await vscode.window.showTextDocument(uri);
  editor.selection=new vscode.Selection(new vscode.Position(0,0),new vscode.Position(1,0));await controller.capture();
  await controller.updatePlan({goal:'Randzeichen behandeln',not_now:'Keine neue Klasse',acceptance:'Gezielter Test'});
  const rules={mode:'explore' as const,preserve_public_signatures:true,allow_new_symbols:false,invariants:'Keine Seiteneffekte'};
  await controller.updateContract(rules);await assert.rejects(controller.send('Ändere etwas','pair'),/nur lesende/);
  await controller.updateContract({...rules,mode:'pair'});
  const snapshot=controller.getState().scope!;assert.equal(editor.document.getText(),snapshot.prefix+snapshot.editable+snapshot.suffix,'Code und Scope müssen vor dem Modelllauf identisch sein');
  await controller.send('Kleinsten Vorschlag erstellen','pair');
  assert.equal(controller.getState().proposal?.status,'ready');assert.equal(editor.document.getText(),source);
  await controller.editProposal();const draftEditor=vscode.window.activeTextEditor;assert.ok(draftEditor);assert.equal(draftEditor.document.isUntitled,true);
  const replace=async(text:string)=>{await draftEditor.edit(edit=>edit.replace(new vscode.Range(draftEditor.document.positionAt(0),draftEditor.document.positionAt(draftEditor.document.getText().length)),text));};
  await replace('return input; } class UngefragterService {');await assert.rejects(controller.useEditedDraft(),/nicht vereinbartes Symbol/);
  assert.ok(controller.getState().contractConflict);assert.equal(editor.document.getText(),source);
  await replace('return input + "!";');await controller.useEditedDraft();
  assert.equal(controller.getState().proposal?.patch.new_text,'return input + "!";');assert.ok(controller.getState().proposalHistory?.length);
  assert.equal(editor.document.getText(),source,'Bearbeiteter Entwurf darf nicht automatisch angewendet werden');
  await controller.apply();assert.match(editor.document.getText(),/return input \+ "!";/);
  assert.ok((controller.getState().contractHistory?.length??0)>=2);
  console.log('PASS: echte Explore-/Pair-Rechte, bearbeitbarer isolierter Entwurf, abgewehrte Zusatzklasse, erneute Scopeprüfung und kontrollierter Apply.');
}
