import * as assert from 'node:assert/strict';
import * as fs from 'node:fs/promises';
import * as http from 'node:http';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run(): Promise<void> {
  const extension=vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');assert.ok(extension);
  const controller=await extension.activate();const folder=vscode.workspace.workspaceFolders?.[0];assert.ok(folder);
  const uri=vscode.Uri.joinPath(folder.uri,'Flow.cs');await fs.writeFile(uri.fsPath,'class Flow { void DeleteRecord() {} }\n');
  const payloads:any[]=[];let turn=0;
  const server=http.createServer(async(req,res)=>{
    let body='';for await(const part of req)body+=part;const input=JSON.parse(body);payloads.push(input);
    const call=(name:string,args:unknown)=>({tool_calls:[{index:0,id:'step-'+turn,type:'function',function:{name,arguments:JSON.stringify(args)}}]});
    let delta;
    if(turn%3===0)delta=call('read_file',{path:'Flow.cs'});
    else if(turn%3===1){const source=JSON.parse(input.messages.filter((m:any)=>m.role==='tool').at(-1).content).source;delta=call('propose_change_plan',{goal:'Löschvertrag klären',not_now:'Keine Produktänderung',open_questions:'Welcher Endpoint gilt?',steps:[{id:'clarify',title:'Vertrag klären',why:'Der Scope ist offen',files:['Flow.cs'],acceptance:'Endpoint bestätigt',source_ids:[source.id]}]});}
    else delta={content:'Der Plan ist zur gemeinsamen Klärung bereit.'};
    ++turn;res.writeHead(200,{'Content-Type':'text/event-stream'});res.end(`data: ${JSON.stringify({choices:[{delta}]})}\n\ndata: [DONE]\n\n`);
  });
  await new Promise<void>(resolve=>server.listen(0,'127.0.0.1',resolve));const address=server.address();assert.ok(address&&typeof address!=='string');
  process.env.AZURE_OPENAI_ENDPOINT=`http://127.0.0.1:${address.port}`;process.env.AZURE_OPENAI_API_KEY='offline-guidance-test';process.env.AZURE_OPENAI_DEPLOYMENT='offline';
  await vscode.workspace.getConfiguration('tandem').update('provider','azure',vscode.ConfigurationTarget.Global);controller.stop();
  try {
    const task='Untersuche den Löschvertrag vollständig lesend.';
    await controller.exploreRepository(task);await controller.adoptChangePlan();
    const count=payloads.length;
    await controller.revalidateAnalysis();
    assert.equal(payloads.length,count,'Quellenprüfung benötigt keinen Modellaufruf');
    assert.equal(controller.getState().changePlan?.stale,false);
    await controller.usePlanStep('clarify');
    assert.equal(controller.getState().activePlanStepId,'clarify');assert.equal(controller.getState().scope,undefined);
    await controller.openPlanFile('Flow.cs');assert.equal(vscode.window.activeTextEditor?.document.uri.toString(),uri.toString());
    await assert.rejects(controller.openPlanFile('.env'),/gehört nicht/);
    const oldPlan=controller.getState().changePlan;
    await controller.discussRepository('Hard-Delete ist fachlich entschieden. Bitte nur den Endpoint klären.');
    assert.equal(controller.getState().analysisTask,task,'Die ursprüngliche Aufgabe darf durch den Dialog nicht ersetzt werden');
    assert.deepEqual(controller.getState().changePlan,oldPlan,'Ein neuer Vorschlag darf den menschlichen Plan nicht überschreiben');
    assert.ok(payloads.some(p=>JSON.stringify(p.messages).includes('fachlich entschieden')));
    assert.equal(payloads.length,6,'Der Dialog muss den lokalen Doppelgänger verwenden');
    const editor=await vscode.window.showTextDocument(uri);await editor.edit(edit=>edit.insert(new vscode.Position(0,0),'// Menschliche Änderung\n'));
    await controller.revalidateAnalysis();assert.equal(controller.getState().exploration?.stale,true);
    console.log('PASS: aktiver Planschritt, direkte Rückfragen ohne Codeauswahl, erhaltene Originalaufgabe und menschlicher Plan, Quellenprüfung ohne Modell sowie gesperrte Dateiöffnung.');
  }finally{controller.stop();server.closeAllConnections();server.close();}
}
