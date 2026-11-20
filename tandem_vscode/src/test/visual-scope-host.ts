import * as assert from 'node:assert/strict';
import * as http from 'node:http';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run():Promise<void> {
  const extension=vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');assert.ok(extension);
  const controller=await extension.activate(),folder=vscode.workspace.workspaceFolders?.[0];assert.ok(folder);await controller.newTask();
  const requests:any[]=[];const server=http.createServer(async(request,response)=>{
    let body='';for await(const chunk of request)body+=chunk;const data=JSON.parse(body);requests.push(data);
    const tool=data.tools.some((item:any)=>item.function.name==='propose_patch')?'propose_patch':'propose_plan';
    const args=tool==='propose_patch'?{path:controller.getState().scope!.path,old_text:'return 1;',new_text:'return 2;',reason:'Nur vereinbarte Methode ändern'}:{goal:'Die ausgewählte Methode prüfen',not_now:'Andere Methode unverändert lassen',acceptance:'Rückgabe ist 2',reason:'Kleinste Änderung im gewählten Codebereich'};
    const delta=data.messages.at(-1).role==='tool'?{content:'Vorschlag zum gewählten Bezug bereit.'}:{tool_calls:[{index:0,id:'visual-tool',type:'function',function:{name:tool,arguments:JSON.stringify(args)}}]};
    response.writeHead(200,{'Content-Type':'text/event-stream'});response.end(`data: ${JSON.stringify({choices:[{delta}]})}\n\ndata: [DONE]\n\n`);
  });await new Promise<void>(resolve=>server.listen(0,'127.0.0.1',resolve));const address=server.address();assert.ok(address&&typeof address!=='string');
  Object.assign(process.env,{AZURE_OPENAI_ENDPOINT:`http://127.0.0.1:${address.port}`,AZURE_OPENAI_API_KEY:'offline-visual-scope',AZURE_OPENAI_DEPLOYMENT:'offline',AZURE_OPENAI_API_VERSION:'2024-10-21'});await vscode.workspace.getConfiguration('tandem').update('provider','azure',vscode.ConfigurationTarget.Global);controller.stop();
  try{
    const uri=vscode.Uri.joinPath(folder.uri,'VisualScope.cs'),source='class Rules {\n int First() { return 1; }\n int Other() { return 1; }\n}\n';await vscode.workspace.fs.writeFile(uri,Buffer.from(source));const editor=await vscode.window.showTextDocument(uri);editor.selection=new vscode.Selection(1,0,1,25);await controller.capture();await controller.updatePlan({goal:'Nur First ändern',not_now:'Other unverändert',acceptance:'Gezielter Fall'});
    const dir=vscode.Uri.joinPath(folder.uri,'.tandem');await vscode.workspace.fs.createDirectory(dir);await vscode.workspace.fs.writeFile(vscode.Uri.joinPath(dir,'architecture.json'),Buffer.from(JSON.stringify({nodes:[{id:'first',label:'First',path:'VisualScope.cs',line:2},{id:'other',label:'Other',path:'VisualScope.cs',line:3}],edges:[]})));await controller.importArchitecture();controller.openGraph();const until=Date.now()+10000;while((!controller.isGraphReady()||controller.getGraphSnapshot()?.context?.pending)&&Date.now()<until)await new Promise(resolve=>setTimeout(resolve,30));
    let graph=controller.getGraphSnapshot()!,contract=controller.getState().contract;
    await controller.selectVisualTarget({kind:'node',layer:'architecture',id:'architecture:other',revision:graph.revision});await controller.chatRequest('Was passiert hier?');assert.ok(!requests[0].tools.some((item:any)=>item.function.name==='propose_patch'),'Kartenauswahl ist zunächst ein lesender Gesprächsbezug');assert.deepEqual(controller.getState().contract,contract);await assert.rejects(controller.chatPrepare(),/außerhalb/);assert.equal(controller.getState().proposal,undefined);
    graph=controller.getGraphSnapshot()!;await controller.selectVisualTarget({kind:'node',layer:'architecture',id:'architecture:first',revision:graph.revision});await assert.rejects(controller.chatPrepare(),/andere visuelle/);
    await controller.chatRequest('Diese Methode soll 2 zurückgeben.');await controller.chatPrepare();assert.equal(controller.getState().proposal?.status,'ready');assert.equal(editor.document.getText(),source,'Auch bestätigter Vorschlag darf nicht automatisch schreiben');await controller.apply();assert.match(editor.document.getText(),/First\(\) \{ return 2;/);assert.match(editor.document.getText(),/Other\(\) \{ return 1;/);assert.equal(controller.getState().evidence,undefined);
    console.log('PASS: visueller Fokus erweitert bestehenden Code-Scope nicht; fremde Methode und veraltete Vereinbarung gesperrt, passende Vereinbarung kontrolliert vorgeschlagen und angewendet.');
  }finally{controller.stop();server.closeAllConnections();server.close();}
}
