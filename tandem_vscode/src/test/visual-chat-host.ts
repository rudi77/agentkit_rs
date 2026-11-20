import * as assert from 'node:assert/strict';
import * as http from 'node:http';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run():Promise<void> {
  const extension=vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');assert.ok(extension);
  const controller=await extension.activate(),folder=vscode.workspace.workspaceFolders?.[0];assert.ok(folder);await controller.newTask();
  const requests:any[]=[],waiting:http.ServerResponse[]=[];let holdNext=false;
  const reply=(response:http.ServerResponse,text:string)=>{response.writeHead(200,{'Content-Type':'text/event-stream'});response.end(`data: ${JSON.stringify({choices:[{delta:{content:text}}]})}\n\ndata: [DONE]\n\n`);};
  const server=http.createServer(async(request,response)=>{let body='';for await(const chunk of request)body+=chunk;requests.push(JSON.parse(body));if(holdNext){holdNext=false;waiting.push(response);}else reply(response,'Diese Verbindung ist eine Hypothese. Lass uns den vorhandenen Code prüfen.');});
  await new Promise<void>(resolve=>server.listen(0,'127.0.0.1',resolve));const address=server.address();assert.ok(address&&typeof address!=='string');
  Object.assign(process.env,{AZURE_OPENAI_ENDPOINT:`http://127.0.0.1:${address.port}`,AZURE_OPENAI_API_KEY:'offline-visual-test',AZURE_OPENAI_DEPLOYMENT:'offline',AZURE_OPENAI_API_VERSION:'2024-10-21'});await vscode.workspace.getConfiguration('tandem').update('provider','azure',vscode.ConfigurationTarget.Global);controller.stop();
  try{
    const a=vscode.Uri.joinPath(folder.uri,'VisualA.cs'),b=vscode.Uri.joinPath(folder.uri,'VisualB.cs');
    await vscode.workspace.fs.writeFile(a,Buffer.from('class A {\n int Read() { return 1; }\n}\n'));await vscode.workspace.fs.writeFile(b,Buffer.from('class B {\n int Published() { return 1; }\n}\n'));
    const dir=vscode.Uri.joinPath(folder.uri,'.tandem');await vscode.workspace.fs.createDirectory(dir);await vscode.workspace.fs.writeFile(vscode.Uri.joinPath(dir,'architecture.json'),Buffer.from(JSON.stringify({nodes:[{id:'prediction',label:'Prediction',path:'VisualA.cs',line:2},{id:'published',label:'Veröffentlichter Stand',path:'VisualB.cs',line:2}],edges:[{from:'prediction',to:'published',kind:'hypothesis',reason:'Vorhandene Lesemethode könnte ausreichen; fachlich prüfen.'}]})));
    await vscode.window.showTextDocument(a);await controller.importArchitecture();controller.openGraph();const ready=Date.now()+10000;while((!controller.isGraphReady()||controller.getGraphSnapshot()?.context?.pending)&&Date.now()<ready)await new Promise(resolve=>setTimeout(resolve,30));assert.equal(controller.isGraphReady(),true);
    let graph=controller.getGraphSnapshot()!;await controller.selectVisualTarget({layer:'architecture',kind:'edge',edge_index:0,revision:graph.revision},true);
    assert.equal(controller.getVisualFocus()?.label,'Prediction → Veröffentlichter Stand');assert.equal(controller.getVisualFocus()?.origin,'hypothesis');assert.equal(controller.getState().scope,undefined);assert.equal(controller.getState().contract,undefined);
    await controller.chatRequest('Brauchen wir diese Verbindung?');assert.ok(JSON.stringify(requests[0]).includes('Vorhandene Lesemethode könnte ausreichen'));assert.ok(JSON.stringify(requests[0]).includes('hypothesis'));assert.ok(JSON.stringify(requests[0]).includes('keine neue Codeauswahl oder Schreibfreigabe'));assert.ok(!requests[0].tools.some((tool:any)=>tool.function.name==='propose_patch'));assert.match(controller.getState().chat[0].context!,/Prediction → Veröffentlichter Stand/);assert.equal(controller.getState().proposal,undefined);
    holdNext=true;const answer=controller.chatRequest('Worauf beruht diese Beziehung?');const deadline=Date.now()+10000;while(!waiting.length&&Date.now()<deadline)await new Promise(resolve=>setTimeout(resolve,25));assert.ok(waiting.length);
    graph=controller.getGraphSnapshot()!;await controller.selectVisualTarget({layer:'architecture',kind:'node',id:'architecture:prediction',revision:graph.revision});reply(waiting[0],'Antwort zur zuvor gewählten Verbindung.');await answer;
    assert.equal(controller.getVisualFocus()?.label,'Prediction');assert.match(controller.getState().chat.at(-1)?.context!,/Prediction → Veröffentlichter Stand/,'Späte Antwort bleibt an ihren tatsächlichen Gesprächsbezug gebunden');
    const count=requests.length,doc=await vscode.workspace.openTextDocument(a),edit=new vscode.WorkspaceEdit();edit.insert(a,new vscode.Position(0,0),'// Menschlicher Edit\n');await vscode.workspace.applyEdit(edit);assert.equal(controller.getVisualFocus()?.stale,true);await assert.rejects(controller.chatRequest('Was passiert hier?'),/veraltet/);assert.equal(requests.length,count,'Veralteter Bezug darf keine neue Modellanfrage starten');assert.ok(doc.getText().startsWith('// Menschlicher Edit'));
    await assert.rejects(controller.selectVisualTarget({layer:'architecture',kind:'node',id:'invented',revision:graph.revision}));assert.equal(controller.getState().scope,undefined);assert.equal(controller.getState().evidence,undefined);
    await controller.newTask();assert.equal(controller.getVisualFocus(),undefined);
    console.log('PASS: echter Karten-/Chat-Bezug im Modellkontext, Hypothese bleibt Hypothese, keine Änderungsrechte, stabile Zuordnung später Antworten und sofortige Sperre nach menschlichem Edit.');
  }finally{controller.stop();server.closeAllConnections();server.close();}
}
