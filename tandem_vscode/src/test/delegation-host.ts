import * as assert from 'node:assert/strict';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run():Promise<void> {
  const extension=vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');assert.ok(extension);
  const controller=await extension.activate(),folder=vscode.workspace.workspaceFolders?.[0];assert.ok(folder);
  const config=vscode.workspace.getConfiguration('tandem');await config.update('provider','demo',vscode.ConfigurationTarget.Global);controller.stop();await controller.newTask();
  const uri=vscode.Uri.joinPath(folder.uri,'Delegate.cs'),source='class Demo { public static string Normalize(string input) { return input; } }\n';
  await vscode.workspace.fs.writeFile(uri,Buffer.from(source));const editor=await vscode.window.showTextDocument(uri);editor.selection=new vscode.Selection(0,0,1,0);
  await controller.capture();await controller.updatePlan({goal:'Randzeichen entfernen',not_now:'Keine neue Klasse',acceptance:'Prüfprogramm bestätigt gezielten Trim-Aufruf'});
  await controller.updateContract({mode:'delegate',preserve_public_signatures:true,allow_new_symbols:false,invariants:'Öffentliche Signatur bleibt gleich'});
  await assert.rejects(controller.send('Ändere','pair'),/Delegate/);
  await assert.rejects(controller.delegate('Ändere',{max_changed_lines:40,auto_apply:true,low_risk:false}),/risikoarm/);
  await controller.delegate('Klein ändern',{max_changed_lines:1,auto_apply:false,low_risk:true}).then(()=>assert.fail('Budget muss stoppen'),()=>undefined);
  assert.equal(controller.getState().delegation?.status,'needs_attention');assert.equal(editor.document.getText(),source);
  await config.update('verificationProgram',process.env.TANDEM_TEST_NODE,vscode.ConfigurationTarget.Global);
  await config.update('verificationArgs',['-e','const fs=require("node:fs");process.exit(fs.readFileSync("Delegate.cs","utf8").includes("input.Trim()")?0:2)'],vscode.ConfigurationTarget.Global);
  await controller.delegate('Klein ändern',{max_changed_lines:40,auto_apply:true,low_risk:true});
  assert.equal(controller.getState().delegation?.status,'verified');assert.equal(controller.getState().evidence?.status,'passed');assert.match(editor.document.getText(),/input.Trim\(\)/);
  assert.ok(controller.getState().delegationHistory?.length);assert.equal(controller.getState().acceptances?.length??0,0,'Keine automatische fachliche Abnahme');
  console.log('PASS: explizite risikoarme Delegation, Budget-Stopp ohne Writes, kontrollierte automatische Übernahme und festgelegte reale Prüfung, keine automatische fachliche Abnahme.');
}
