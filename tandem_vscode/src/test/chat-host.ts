import * as assert from 'node:assert/strict';
import * as http from 'node:http';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run():Promise<void> {
  const extension=vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');assert.ok(extension);
  const controller=await extension.activate(),folder=vscode.workspace.workspaceFolders?.[0];assert.ok(folder);await controller.newTask();
  const requests:any[]=[],waiting:http.ServerResponse[]=[];
  const server=http.createServer(async(request,response)=>{
    let body='';for await(const chunk of request)body+=chunk;const data=JSON.parse(body);requests.push(data);
    const last=data.messages.at(-1),names=data.tools.map((tool:any)=>tool.function.name);
    if(last.role!=='tool'&&last.content?.includes('Warte absichtlich')&&!last.content.includes('Stattdessen')){waiting.push(response);return;}
    let delta:unknown;
    if(last.role==='tool')delta={content:last.name==='propose_plan'?'So ist das Ziel eingegrenzt.':'Der kleine Vorschlag ist bereit.'};
    else if(names.includes('propose_patch'))delta={tool_calls:[{index:0,id:'patch',type:'function',function:{name:'propose_patch',arguments:JSON.stringify({path:controller.getState().scope!.path,old_text:'return input;',new_text:'return input.Trim();',reason:'Vorhandene Methode verwenden, keine neue Klasse.'})}}]};
    else if(!names.includes('list_files')&&!last.content.includes('Begutachte die menschliche'))delta={tool_calls:[{index:0,id:'plan',type:'function',function:{name:'propose_plan',arguments:JSON.stringify({goal:'Rand-Leerzeichen entfernen',not_now:'Keine neue Klasse oder Refactorings',acceptance:'Die vorhandene Methode gibt input.Trim() zurück.',reason:'Eine vorhandene String-Methode reicht.'})}}]};
    else delta={content:'Ich habe den aktuellen Code lesend geprüft. Lass uns beim vorhandenen Aufbau bleiben.'};
    response.writeHead(200,{'Content-Type':'text/event-stream'});response.end(`data: ${JSON.stringify({choices:[{delta}]})}\n\ndata: [DONE]\n\n`);
  });
  await new Promise<void>(resolve=>server.listen(0,'127.0.0.1',resolve));const address=server.address();assert.ok(address&&typeof address!=='string');
  Object.assign(process.env,{AZURE_OPENAI_ENDPOINT:`http://127.0.0.1:${address.port}`,AZURE_OPENAI_API_KEY:'offline-chat-test',AZURE_OPENAI_DEPLOYMENT:'offline',AZURE_OPENAI_API_VERSION:'2024-10-21'});
  const config=vscode.workspace.getConfiguration('tandem');await config.update('provider','azure',vscode.ConfigurationTarget.Global);controller.stop();
  try{
    await vscode.commands.executeCommand('tandem.open');const deadline=Date.now()+10000;while(!controller.isWorkbenchReady()&&Date.now()<deadline)await new Promise(resolve=>setTimeout(resolve,30));assert.equal(controller.isWorkbenchReady(),true);
    await controller.chatRequest('Wie funktioniert die Normalisierung?');assert.equal(controller.getState().chat[0].text,'Wie funktioniert die Normalisierung?');assert.ok(controller.getState().exploration);assert.equal(controller.getState().proposal,undefined);
    await controller.chatRequest('Bitte beim vorhandenen Aufbau bleiben.');assert.equal(controller.getState().analysisTask,'Wie funktioniert die Normalisierung?');assert.ok(requests.some(request=>request.messages.some((message:any)=>typeof message.content==='string'&&message.content.includes('TANDEM-Chat'))));
    await controller.newTask();const uri=vscode.Uri.joinPath(folder.uri,'Normalize.cs'),doc=await vscode.workspace.openTextDocument(uri),editor=await vscode.window.showTextDocument(doc);const source=doc.getText();editor.selection=new vscode.Selection(0,0,doc.lineCount,0);await controller.capture();
    await controller.chatRequest('Entferne Rand-Leerzeichen. Keine neue Klasse.');assert.ok(controller.getState().suggestion?.acceptance);assert.equal(controller.getState().plan.acceptance,'');assert.equal(controller.getState().proposal,undefined);assert.equal(doc.getText(),source);
    await controller.chatPrepare();assert.equal(controller.getState().proposal?.status,'ready');assert.equal(doc.getText(),source);assert.equal(controller.getState().evidence,undefined);assert.ok(controller.getState().plan.not_now.includes('Keine neue Klasse'));
    const first=controller.chatRequest('Warte absichtlich auf meinen Einwand.');const until=Date.now()+10000;while(!waiting.length&&Date.now()<until)await new Promise(resolve=>setTimeout(resolve,25));assert.ok(waiting.length);
    await assert.rejects(controller.chatRequest(''),/Nachricht/);await controller.chatRequest('Stattdessen verwende bitte weiterhin die vorhandene Methode.');await first;assert.equal(controller.getState().proposal?.status,'ready');assert.equal(doc.getText(),source);
    await controller.apply();await config.update('verificationProgram',process.env.TANDEM_TEST_NODE!,vscode.ConfigurationTarget.Workspace);await config.update('verificationArgs',[vscode.Uri.joinPath(folder.uri,'verify.cjs').fsPath],vscode.ConfigurationTarget.Workspace);assert.equal((await controller.verify()).status,'passed');
    const edit=new vscode.WorkspaceEdit();edit.insert(uri,new vscode.Position(0,0),'// Meine eigene Änderung\n');await vscode.workspace.applyEdit(edit);await controller.chatRequest('Prüfe bitte meine eigene Änderung.');assert.ok(controller.getState().humanChange?.review);assert.equal(controller.getState().chat.at(-2)?.text,'Prüfe bitte meine eigene Änderung.');assert.ok(doc.getText().startsWith('// Meine eigene Änderung'));assert.equal(controller.getState().evidence?.status,'stale');
    await vscode.commands.executeCommand('tandem.details');assert.ok(vscode.window.tabGroups.all.some(group=>group.tabs.some(tab=>tab.label==='TANDEM · Details & Werkzeuge')));
    console.log('PASS: tatsächlicher Chat-Host, Repository-Rückfragen, Codekontext, Vereinbarung ohne Formular, kontrollierter Patch/Apply/realer Test, Unterbrechen, menschliches Review und optionale Detailwerkzeuge.');
  }finally{controller.stop();server.closeAllConnections();server.close();}
}
