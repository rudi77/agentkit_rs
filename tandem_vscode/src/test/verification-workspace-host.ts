import * as assert from 'node:assert/strict';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run():Promise<void> {
  const extension=vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');assert.ok(extension);const controller=await extension.activate(),folder=vscode.workspace.workspaceFolders?.[0];assert.ok(folder);
  await controller.newTask();const config=vscode.workspace.getConfiguration('tandem');await config.update('provider','demo',vscode.ConfigurationTarget.Global);controller.stop();
  const uri=vscode.Uri.joinPath(folder.uri,'Bound.cs'),other=vscode.Uri.joinPath(folder.uri,'Other.cs');
  await vscode.workspace.fs.writeFile(uri,Buffer.from('class Bound { }\n'));await vscode.workspace.fs.writeFile(other,Buffer.from('class Other { }\n'));
  const editor=await vscode.window.showTextDocument(uri);editor.selection=new vscode.Selection(0,0,1,0);await controller.capture();await controller.updatePlan({goal:'Prüfstand binden',not_now:'Keine Nebenänderungen',acceptance:'Alle erlaubten Quellinhalte bleiben während der Prüfung gleich'});
  await config.update('verificationProgram',process.env.TANDEM_TEST_NODE,vscode.ConfigurationTarget.Global);await config.update('verificationArgs',['-e','process.exit(0)'],vscode.ConfigurationTarget.Global);
  const clean=await controller.verify();assert.equal(clean.status,'passed');assert.ok(clean.workspace);assert.equal(clean.workspace.before_hash,clean.workspace.after_hash);assert.ok(clean.workspace.file_count>=4);
  await config.update('verificationArgs',['-e','require("node:fs").appendFileSync("Other.cs","// verändert")'],vscode.ConfigurationTarget.Global);
  const changed=await controller.verify();assert.equal(changed.exitCode,0);assert.equal(changed.status,'stale');assert.ok(changed.workspace?.changed_paths.includes('Other.cs'),'Auch eine andere Datei als der gewählte Codebereich muss den Beleg invalidieren');
  await config.update('verificationArgs',['-e','const fs=require("node:fs");fs.mkdirSync("bin",{recursive:true});fs.writeFileSync("bin/output.txt","Buildausgabe")'],vscode.ConfigurationTarget.Global);
  const build=await controller.verify();assert.equal(build.status,'passed','Ausdrücklich ausgeschlossene Buildausgaben gehören nicht zum Quellstand');
  const otherEditor=await vscode.window.showTextDocument(other);await otherEditor.edit(edit=>edit.insert(new vscode.Position(0,0),'// ungespeicherter menschlicher Edit\n'));
  await assert.rejects(controller.verify(),/ungespeichert/,'Andere menschliche Edits dürfen weder automatisch gespeichert noch durch Tests des alten Diskstands verdeckt werden');assert.equal(otherEditor.document.isDirty,true);
  await otherEditor.document.save();
  await config.update('verificationArgs',['-e','setTimeout(()=>process.exit(0),10000)'],vscode.ConfigurationTarget.Global);
  const pending=controller.verify();const deadline=Date.now()+10000;while(controller.getState().evidence?.status!=='running'&&Date.now()<deadline)await new Promise(resolve=>setTimeout(resolve,25));
  await controller.updatePlan({goal:'Während der Prüfung geänderte Absicht',not_now:'Keine Nebenänderungen',acceptance:'Neuer Contract benötigt neue Prüfung'});controller.stop();
  await pending.catch(()=>undefined);assert.notEqual(controller.getState().evidence?.status,'passed');
  console.log('PASS: vollständige Inhaltsbindung des erlaubten Suchraums, fremde Quelländerung trotz Exit 0 erkannt, Buildausgaben getrennt, ungespeicherte menschliche Edits erhalten und Plan-/Stop-Rennen ohne falschen Prüfbeleg.');
}
