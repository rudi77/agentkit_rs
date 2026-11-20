import * as assert from 'node:assert/strict';
import * as http from 'node:http';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run():Promise<void>{
  const extension=vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');assert.ok(extension);
  const controller=await extension.activate(),folder=vscode.workspace.workspaceFolders?.[0];assert.ok(folder);await controller.newTask();
  await vscode.workspace.fs.writeFile(vscode.Uri.joinPath(folder.uri,'AGENTS.md'),Buffer.from('Bestehende Tests unverändert lassen. Keine neue Klasse.'));
  let implement=false;const requests:any[]=[];
  const server=http.createServer(async(request,response)=>{
    request.setEncoding('utf8');let body='';for await(const chunk of request)body+=chunk;const data=JSON.parse(body);requests.push(data);
    const last=data.messages.at(-1),lastToolName=last.name||data.messages.flatMap((message:any)=>message.tool_calls||[]).find((call:any)=>call.id===last.tool_call_id)?.function.name,names=data.tools.map((tool:any)=>tool.function.name);
    const tool=(name:string,args:unknown)=>({tool_calls:[{index:0,id:'implementation-tool-'+requests.length,type:'function',function:{name,arguments:JSON.stringify(args)}}]});
    let delta:unknown;
    if(names.includes('propose_patch'))delta=last.role==='tool'?{content:'Der Diff-Vorschlag ist bereit; noch nicht übernommen.'}:tool('propose_patch',{path:controller.getState().scope!.path,old_text:controller.getState().scope!.editable.includes('return input.Trim();')?'return input.Trim();':'return input;',new_text:controller.getState().scope!.editable.includes('return input.Trim();')?'return input.TrimEnd();':'return input.Trim();',reason:'Vorhandene Methode wie vereinbart verwenden.'});
    else if(last.role!=='tool')delta=tool('read_file',{path:'Normalize.cs'});
    else if(lastToolName==='read_file'){
      const source=JSON.parse(last.content).source;assert.ok(source?.id);
      delta=tool('propose_change_plan',{goal:'Rand-Leerzeichen entfernen',not_now:'Keine neuen Klassen; bestehende Tests unverändert',open_questions:'',steps:[{id:'trim',title:'Vorhandene Normalisierung anpassen',why:'Verhalten ist bestätigt',files:['Normalize.cs'],acceptance:'Rückgabe entspricht input.Trim()',source_ids:[source.id]}]});
    }else if(lastToolName==='propose_change_plan'&&implement)delta=tool('begin_implementation',{step_id:'trim'});
    else delta={content:'Der belegte Plan ist bereit.'};
    response.writeHead(200,{'Content-Type':'text/event-stream'});response.end(`data: ${JSON.stringify({choices:[{delta}]})}\n\ndata: [DONE]\n\n`);
  });
  await new Promise<void>(resolve=>server.listen(0,'127.0.0.1',resolve));const address=server.address();assert.ok(address&&typeof address!=='string');
  Object.assign(process.env,{AZURE_OPENAI_ENDPOINT:`http://127.0.0.1:${address.port}`,AZURE_OPENAI_API_KEY:'offline-implementation',AZURE_OPENAI_DEPLOYMENT:'offline',AZURE_OPENAI_API_VERSION:'2024-10-21'});
  await vscode.workspace.getConfiguration('tandem').update('provider','azure',vscode.ConfigurationTarget.Global);controller.stop();
  const uri=vscode.Uri.joinPath(folder.uri,'Normalize.cs'),doc=await vscode.workspace.openTextDocument(uri),before=doc.getText();
  try{
    await controller.chatRequest('Wie können wir die Normalisierung verbessern?');assert.ok(controller.getState().exploration?.suggested_plan);assert.equal(controller.getState().scope,undefined);assert.equal(controller.getState().proposal,undefined);
    await controller.chatUseStep('trim',controller.getState().exploration!.created_at);assert.equal(controller.getState().scope,undefined,'Besprechen allein erweitert den Codebereich nicht');
    implement=true;await controller.chatRequest('Ja, setze den Plan jetzt um!');
    let state=controller.getState();assert.equal(state.activePlanStepId,'trim');assert.equal(state.proposal?.status,'ready');assert.equal(state.proposal?.patch.new_text,'return input.Trim();');assert.equal(state.changePlan?.steps[0].status,'preview');assert.equal(state.evidence,undefined);assert.equal(doc.getText(),before);
    const patchRequest=requests.find(request=>request.tools.some((tool:any)=>tool.function.name==='propose_patch'));assert.ok(patchRequest);assert.ok(patchRequest.messages.some((message:any)=>message.content?.includes('Bestehende Tests unverändert lassen')));
    assert.ok(vscode.window.visibleTextEditors.some(editor=>editor.document.uri.toString()===uri.toString()));
    await controller.apply();assert.ok(doc.getText().includes('return input.Trim();'));assert.equal(controller.getState().evidence,undefined);
    await controller.newTask();implement=false;await controller.chatRequest('Bitte einen Plan erstellen.');
    const at=controller.getState().exploration!.created_at;
    await assert.rejects(controller.chatBeginImplementation('trim','obsolete'),/aktualisiert/);
    await assert.rejects(controller.chatBeginImplementation('trim',at,'other.cs'),/gehört nicht/);assert.equal(controller.getState().scope,undefined);
    await controller.chatBeginImplementation('trim',at);state=controller.getState();assert.equal(state.scope?.uri,uri.toString());assert.ok(state.proposal,'Auch die direkte Checklistenaktion erreicht den Pair-Agenten');
    console.log('PASS: natürlicher Umsetzungsauftrag und Checklistenaktion starten echte Patch-Vorbereitung ohne manuelle Auswahl; Planfrage bleibt lesend, Regeln erhalten, keine automatische Übernahme/Prüfbehauptung, fremde Datei und veraltete Aktion gesperrt.');
  }finally{controller.stop();server.closeAllConnections();server.close();}
}
