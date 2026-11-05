import * as assert from 'node:assert/strict';
import * as http from 'node:http';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run():Promise<void> {
  const extension=vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');assert.ok(extension);
  const controller=await extension.activate(),folder=vscode.workspace.workspaceFolders?.[0];assert.ok(folder);await controller.newTask();
  const uri=vscode.Uri.joinPath(folder.uri,'Cooperation.cs'),source='class Rule { public int Run(int input) { return input; } }\n';
  await vscode.workspace.fs.writeFile(uri,Buffer.from(source));let editor=await vscode.window.showTextDocument(uri);editor.selection=new vscode.Selection(0,0,1,0);await controller.capture();
  await controller.updatePlan({goal:'Wert gezielt erhöhen',not_now:'Keine neue Klasse',acceptance:'Gezielter Referenztest'});
  const requests:any[]=[];let waiting:http.ServerResponse|undefined;
  const respond=(res:http.ServerResponse,delta:unknown)=>{res.writeHead(200,{'Content-Type':'text/event-stream'});res.end(`data: ${JSON.stringify({choices:[{delta}]})}\n\ndata: [DONE]\n\n`);};
  const server=http.createServer(async(req,res)=>{
    let body='';for await(const part of req)body+=part;const data=JSON.parse(body);requests.push(data);
    if(JSON.stringify(data.messages).includes('Begutachte die menschliche Codeänderung')){respond(res,{content:'Die menschliche Änderung erhöht den Wert. Ein echter Test bleibt erforderlich.'});return;}
    if(data.messages.at(-1).role==='tool'){waiting=res;return;}
    respond(res,{tool_calls:[{index:0,id:'live',type:'function',function:{name:'propose_patch',arguments:JSON.stringify({path:controller.getState().scope!.path,old_text:'return input + 1;',new_text:'return input + 2;',reason:'Gezielter nächster Wert'})}}]});
  });
  await new Promise<void>(resolve=>server.listen(0,'127.0.0.1',resolve));const address=server.address();assert.ok(address&&typeof address!=='string');
  process.env.AZURE_OPENAI_ENDPOINT=`http://127.0.0.1:${address.port}`;process.env.AZURE_OPENAI_API_KEY='offline-cooperation';process.env.AZURE_OPENAI_DEPLOYMENT='offline';
  const config=vscode.workspace.getConfiguration('tandem');await config.update('provider','azure',vscode.ConfigurationTarget.Global);await config.update('liveDiff',true,vscode.ConfigurationTarget.Global);controller.stop();
  try{
    const offset=editor.document.getText().indexOf('return input;');await editor.edit(edit=>edit.replace(new vscode.Range(editor.document.positionAt(offset),editor.document.positionAt(offset+'return input;'.length)),'return input + 1;'));
    assert.ok(controller.getState().humanChange);const humanText=editor.document.getText();await controller.reviewHumanChanges('Bleibt die Änderung minimal?');
    assert.equal(editor.document.getText(),humanText);assert.equal(controller.getState().humanChange?.review?.stale,false);assert.equal(controller.getState().evidence,undefined);
    assert.match(JSON.stringify(requests[0].messages),/Ursprünglicher Ausschnitt/);
    editor=await vscode.window.showTextDocument(uri);editor.selection=new vscode.Selection(0,0,1,0);await controller.capture();assert.ok(controller.getState().humanChangeHistory?.length);
    const pending=controller.send('Zeige die nächste kleine Änderung','pair');
    const deadline=Date.now()+10000;
    const live=()=>vscode.window.tabGroups.all.some(group=>group.tabs.some(tab=>tab.input instanceof vscode.TabInputTextDiff&&tab.input.modified.scheme==='tandem-preview'&&tab.input.modified.path.includes('/live-')));
    while((!waiting||!live())&&Date.now()<deadline)await new Promise(resolve=>setTimeout(resolve,25));
    assert.ok(waiting,'Der Modelllauf muss nach dem Tool-Ergebnis noch offen sein');assert.ok(live(),'Der scopegeprüfte Diff muss schon vor der finalen Antwort sichtbar sein');assert.equal(editor.document.getText(),humanText);
    respond(waiting,{content:'Der begrenzte Vorschlag liegt vor.'});await pending;assert.equal(controller.getState().proposal?.status,'ready');
    waiting=undefined;
    const paused=controller.send('Zwischenstand gemeinsam besprechen','pair').then(()=>assert.fail('Der pausierte Modelllauf muss abbrechen'),error=>assert.match(error.message,/gestoppt|beendet|abgebrochen/));
    const pauseDeadline=Date.now()+10000;while(!waiting&&Date.now()<pauseDeadline)await new Promise(resolve=>setTimeout(resolve,25));assert.ok(waiting);
    await controller.pausePreview();await paused;assert.equal(controller.getState().proposal?.status,'ready');assert.equal(editor.document.getText(),humanText,'Ein festgehaltener Zwischenstand ist weiterhin unapplied');
    const architectureDir=vscode.Uri.joinPath(folder.uri,'.tandem');await vscode.workspace.fs.createDirectory(architectureDir);
    await vscode.workspace.fs.writeFile(vscode.Uri.joinPath(architectureDir,'architecture.json'),Buffer.from(JSON.stringify({nodes:[{id:'rule',label:'Wertregel',path:'Cooperation.cs',line:1}],edges:[{from:'rule',to:'rule',kind:'hypothesis',reason:'Noch zu prüfen'}]})));
    await controller.importArchitecture();assert.equal(controller.getState().architecture?.edges[0].kind,'hypothesis');
    await controller.evaluate({cognitive_load:3,comprehension:6,interruption_cost:2,explanation:'Der Wert wird in derselben Methode erhöht.',notes:'Kein neuer Dienst.'});
    const exported=await controller.exportArtifact(false),artifact=JSON.parse(Buffer.from(await vscode.workspace.fs.readFile(exported)).toString('utf8'));
    assert.equal(artifact.execution_authorization,'none');assert.equal(artifact.state.scope.editable,undefined);assert.equal(artifact.metrics.evaluations.length,1);
    const sessionId=controller.getState().sessionId!;await controller.newTask();assert.equal(controller.getState().scope,undefined);assert.ok(controller.getState().archivedSessions?.some(item=>item.id===sessionId));
    await controller.restoreTask(sessionId);assert.equal(controller.getState().sessionId,sessionId);assert.equal(controller.getState().proposal?.status,'stale');assert.equal(controller.getState().evaluations?.length,1);
    console.log('PASS: menschliche Edits lesend begutachtet, echter Live-Diff vor finaler Modellantwort, Architektur-Provenienz, lokaler Export/Evaluation und verlustfreier Aufgabenwechsel.');
  }finally{controller.stop();server.closeAllConnections();server.close();}
}
