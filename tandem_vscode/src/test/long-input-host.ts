import * as assert from 'node:assert/strict';
import * as http from 'node:http';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run():Promise<void> {
  const extension=vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');assert.ok(extension);
  const controller=await extension.activate(),folder=vscode.workspace.workspaceFolders?.[0];assert.ok(folder);await controller.newTask();
  const requests:any[]=[];const server=http.createServer(async(request,response)=>{request.setEncoding('utf8');let body='';for await(const chunk of request)body+=chunk;requests.push(JSON.parse(body));response.writeHead(200,{'Content-Type':'text/event-stream'});response.end('data: {"choices":[{"delta":{"content":"Den vollständigen eingefügten Text lesend geprüft."}}]}\n\ndata: [DONE]\n\n');});
  await new Promise<void>(resolve=>server.listen(0,'127.0.0.1',resolve));const address=server.address();assert.ok(address&&typeof address!=='string');
  Object.assign(process.env,{AZURE_OPENAI_ENDPOINT:`http://127.0.0.1:${address.port}`,AZURE_OPENAI_API_KEY:'offline-long-input',AZURE_OPENAI_DEPLOYMENT:'offline',AZURE_OPENAI_API_VERSION:'2024-10-21'});await vscode.workspace.getConfiguration('tandem').update('provider','azure',vscode.ConfigurationTarget.Global);controller.stop();
  const text='BEGINN_LANGER_TEXT\n'+'Fehler: äöü 🚀 /api/records\n'.repeat(4000)+'ENDE_LANGER_TEXT';assert.ok(Buffer.byteLength(text)>48000);
  try{
    await controller.chatRequest(text);let content=requests.at(-1).messages.at(-1).content;const payload=JSON.parse(content);assert.ok(payload.task.includes(text),JSON.stringify({actual_length:payload.task.length,expected_length:text.length,start:payload.task.slice(0,90),end:payload.task.slice(-90)}));assert.equal(payload.history.length,0,'Aktuelle Nachricht darf nicht doppelt übertragen werden');assert.equal(controller.getState().chat[0].text,text);
    await controller.chatRequest('Bitte die Ursache erklären.');content=requests.at(-1).messages.at(-1).content;const follow=JSON.parse(content);assert.ok(follow.task.includes(text));assert.equal(follow.task.split('BEGINN_LANGER_TEXT').length-1,1);assert.ok(!follow.history.some((item:any)=>item.text.includes('BEGINN_LANGER_TEXT')),'Ursprüngliche Aufgabe steht bereits im Prompt');
    await controller.newTask();const uri=vscode.Uri.joinPath(folder.uri,'Normalize.cs'),doc=await vscode.workspace.openTextDocument(uri),editor=await vscode.window.showTextDocument(doc);editor.selection=new vscode.Selection(0,0,doc.lineCount,0);await controller.capture();const source=doc.getText();
    await controller.chatRequest(text);content=requests.at(-1).messages.at(-1).content;assert.ok(content.includes(text));assert.equal(content.split('ENDE_LANGER_TEXT').length-1,1);
    const edit=new vscode.WorkspaceEdit();edit.insert(uri,new vscode.Position(0,0),'// Eigene Änderung\n');await vscode.workspace.applyEdit(edit);await controller.chatRequest(text);content=requests.at(-1).messages.at(-1).content;assert.ok(content.includes(text),'Auch der Review-Pfad darf den Chattext nicht auf 4 KB kürzen');assert.ok(doc.getText().startsWith('// Eigene Änderung'));assert.ok(doc.getText().endsWith(source));
    const count=requests.length;await assert.rejects(controller.chatRequest('🚀'.repeat(64001)),/256 KB/);assert.equal(requests.length,count);assert.equal(controller.getState().proposal,undefined);assert.equal(controller.getState().evidence,undefined);
    console.log('PASS: lange Unicode-Texte erreichen Azure-Doppelgänger vollständig in Erkundung, Rückfrage, Code-Chat und Review; keine doppelten aktuellen Texte und klare Obergrenze ohne Produktwrites.');
  }finally{controller.stop();server.closeAllConnections();server.close();}
}
