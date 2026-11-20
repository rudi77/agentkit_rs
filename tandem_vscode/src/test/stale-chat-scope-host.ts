import * as assert from 'node:assert/strict';
import * as http from 'node:http';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run():Promise<void> {
  const extension=vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');assert.ok(extension);
  const controller=await extension.activate(),folder=vscode.workspace.workspaceFolders?.[0];assert.ok(folder);await controller.newTask();
  const requests:any[]=[],server=http.createServer(async(request,response)=>{
    request.setEncoding('utf8');let body='';for await(const chunk of request)body+=chunk;requests.push(JSON.parse(body));
    response.writeHead(200,{'Content-Type':'text/event-stream'});response.end('data: {"choices":[{"delta":{"content":"Ich kann deine Frage am aktuellen Repository lesend untersuchen."}}]}\n\ndata: [DONE]\n\n');
  });
  await new Promise<void>(resolve=>server.listen(0,'127.0.0.1',resolve));const address=server.address();assert.ok(address&&typeof address!=='string');
  Object.assign(process.env,{AZURE_OPENAI_ENDPOINT:`http://127.0.0.1:${address.port}`,AZURE_OPENAI_API_KEY:'offline-stale-scope',AZURE_OPENAI_DEPLOYMENT:'offline',AZURE_OPENAI_API_VERSION:'2024-10-21'});
  await vscode.workspace.getConfiguration('tandem').update('provider','azure',vscode.ConfigurationTarget.Global);controller.stop();
  const uri=vscode.Uri.joinPath(folder.uri,'Normalize.cs'),doc=await vscode.workspace.openTextDocument(uri);
  async function capture(){const editor=await vscode.window.showTextDocument(doc);editor.selection=new vscode.Selection(0,0,doc.lineCount,0);await controller.capture();}
  try{
    await capture();await controller.updatePlan({goal:'Gespeicherter Codebereich',not_now:'Keine weiteren Dateien',acceptance:'Vorhandene Methode verwenden'});
    const savedId=controller.getState().sessionId!;await controller.newTask();
    const edit=new vscode.WorkspaceEdit();edit.insert(uri,new vscode.Position(0,0),'// Neuer Stand nach gespeicherter Unterhaltung\n');await vscode.workspace.applyEdit(edit);await doc.save();const current=doc.getText();
    await controller.restoreTask(savedId);assert.ok(controller.getState().scope);assert.equal(controller.getState().humanChange,undefined);
    await assert.rejects(controller.send('Ändere den alten Bereich.','pair'),/Code wurde geändert/);
    const pasted='Welche Verbesserung sollten wir jetzt einbauen?\n'+'Eingefügter Terminaltext │ äöü\n'.repeat(1700);
    assert.ok(Buffer.byteLength(pasted)>48000);await controller.chatRequest(pasted);
    const state=controller.getState(),request=requests.at(-1),payload=JSON.parse(request.messages.at(-1).content);
    assert.ok(payload.task.includes(pasted));assert.ok(request.tools.some((tool:any)=>tool.function.name==='list_files'));
    assert.ok(!request.tools.some((tool:any)=>tool.function.name==='propose_patch'));assert.equal(state.scope,undefined);assert.equal(state.contract,undefined);
    assert.equal(state.proposal,undefined);assert.equal(doc.getText(),current);assert.ok(state.chat.some(message=>message.text.includes('Codeauswahl ist veraltet')));
    await capture();assert.ok(controller.getState().contract);await controller.clearScope();assert.equal(controller.getState().scope,undefined);assert.equal(controller.getState().contract,undefined);
    const count=requests.length;await controller.chatRequest('Bitte weiterhin nur erklären.');assert.ok(requests.length>count);assert.equal(doc.getText(),current);
    console.log('PASS: langer Chat trotz gespeicherter veralteter Codeauswahl; alter Pair-Auftrag bleibt gesperrt, aktuelles Repository nur lesend, Codeauswahl entfernbar ohne Codeänderung.');
  }finally{controller.stop();server.closeAllConnections();server.close();}
}
