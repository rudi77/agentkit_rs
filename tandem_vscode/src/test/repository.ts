import * as assert from 'node:assert/strict';
import * as fs from 'node:fs/promises';
import * as http from 'node:http';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run(): Promise<void> {
  // Bereits capture() prüft Pfade über einen Kindprozess. Daher vor Aktivierung
  // sperren und nach dem Start des lokalen HTTP-Doppelgängers neu starten.
  process.env.AZURE_OPENAI_ENDPOINT='http://127.0.0.1:1';
  process.env.AZURE_OPENAI_API_KEY='offline-repository-test';
  process.env.AZURE_OPENAI_DEPLOYMENT='offline';
  process.env.AZURE_OPENAI_API_VERSION='2024-10-21';
  const extension=vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');
  assert.ok(extension);
  const controller=await extension.activate();
  const folder=vscode.workspace.workspaceFolders?.[0];
  assert.ok(folder);
  const root=folder.uri;
  await fs.mkdir(vscode.Uri.joinPath(root,'src').fsPath,{recursive:true});
  await fs.mkdir(vscode.Uri.joinPath(root,'tests').fsPath,{recursive:true});
  await fs.writeFile(vscode.Uri.joinPath(root,'AGENTS.md').fsPath,'Lies [CLAUDE.md](CLAUDE.md).\n');
  await fs.writeFile(vscode.Uri.joinPath(root,'CLAUDE.md').fsPath,'Keine bestehenden Tests ändern. Keine PROD-Löschung.\n');
  const code=vscode.Uri.joinPath(root,'src/Service.cs');
  await fs.writeFile(code.fsPath,'class Service { void DeleteRecord() { DeactivateAccount(); } }\n');
  await fs.writeFile(vscode.Uri.joinPath(root,'tests/DeleteTests.cs').fsPath,'void DeleteRecord_returns_405() {}\n');
  const secret=vscode.Uri.joinPath(root,'.env');
  await fs.writeFile(secret.fsPath,'SECRET_SENTINEL_NOT_FOR_MODEL');
  const secretDoc=await vscode.workspace.openTextDocument(secret);
  let editor=await vscode.window.showTextDocument(secretDoc);
  await editor.edit(edit=>edit.insert(new vscode.Position(0,0),'UNSAVED_SECRET_'));
  editor.selection=new vscode.Selection(new vscode.Position(0,0),secretDoc.positionAt(secretDoc.getText().length));
  await assert.rejects(controller.capture(),/gesperrt/);
  const doc=await vscode.workspace.openTextDocument(code);
  editor=await vscode.window.showTextDocument(doc);
  await editor.edit(edit=>edit.insert(new vscode.Position(0,0),'// Ungespeicherte menschliche Ergänzung 🦀\n'));
  const diskBefore=await fs.readFile(code.fsPath,'utf8');
  const payloads:string[]=[];
  let turn=0;
  const server=http.createServer(async(request,response)=>{
    let body='';for await(const chunk of request)body+=chunk;
    payloads.push(body);
    const input=JSON.parse(body);
    let delta:unknown;
    const tool=(name:string,args:unknown)=>({tool_calls:[{index:0,id:`tool-${turn}`,type:'function',function:{name,arguments:JSON.stringify(args)}}]});
    switch(turn++) {
      case 0: delta=tool('search_code',{query:'DeleteRecord'});break;
      case 1: delta=tool('read_file',{path:'src/Service.cs'});break;
      case 2: delta=tool('read_file',{path:'tests/DeleteTests.cs'});break;
      case 3: {
        const results=input.messages.filter((m:any)=>m.role==='tool').map((m:any)=>JSON.parse(m.content));
        const sources=results.filter((r:any)=>r.source).map((r:any)=>r.source);
        delta=tool('propose_change_plan',{goal:'Hard-Delete klären',not_now:'Keine PROD-Mutation; bestehende Tests unverändert',open_questions:'Welcher SBF-Vertrag ist deployt?',steps:[{id:'contract',title:'Vertrag und Adapter abgleichen',why:'Der Service deaktiviert derzeit nur',files:['src/Service.cs','tests/DeleteTests.cs'],acceptance:'Hard-Delete-Nachweise und neue Testfälle benennen',source_ids:sources.map((s:any)=>s.id)}]});break;
      }
      default: delta={content:'Service und vorhandenen Test gefunden. Keine Dateien geändert, keine Tests ausgeführt.'};
    }
    response.writeHead(200,{'Content-Type':'text/event-stream'});
    response.end(`data: ${JSON.stringify({choices:[{delta}]})}\n\ndata: [DONE]\n\n`);
  });
  await new Promise<void>(resolve=>server.listen(0,'127.0.0.1',resolve));
  const address=server.address();assert.ok(address&&typeof address!=='string');
  process.env.AZURE_OPENAI_ENDPOINT=`http://127.0.0.1:${address.port}`;
  process.env.AZURE_OPENAI_API_KEY='offline-repository-test';
  process.env.AZURE_OPENAI_DEPLOYMENT='offline';
  process.env.AZURE_OPENAI_API_VERSION='2024-10-21';
  await vscode.workspace.getConfiguration('tandem').update('provider','azure',vscode.ConfigurationTarget.Global);
  controller.stop();
  try {
    const result=await controller.exploreRepository('Untersuche den Hard-Delete-Pfad und bestehende Tests, nur lesend.');
    assert.ok(result.suggested_plan);
    assert.equal(result.stale,false);
    assert.ok(result.sources.some(s=>s.path==='src/Service.cs'&&s.editor));
    assert.ok(result.sources.some(s=>s.kind==='test'));
    assert.ok(result.instruction_paths.includes('CLAUDE.md'));
    console.log('Geladene Projektregeln im ersten Modellkontext:',JSON.stringify(JSON.parse(payloads[0]).messages[0]).includes('Keine bestehenden Tests'));
    assert.ok(payloads.some(p=>p.includes('Keine bestehenden Tests')));
    assert.ok(payloads.some(p=>p.includes('Ungespeicherte menschliche')));
    assert.ok(payloads.every(p=>!p.includes('SECRET_SENTINEL_NOT_FOR_MODEL')));
    assert.equal(await fs.readFile(code.fsPath,'utf8'),diskBefore);
    assert.equal(await fs.readFile(secret.fsPath,'utf8'),'SECRET_SENTINEL_NOT_FOR_MODEL');
    await controller.adoptChangePlan();
    const shared=controller.getState().changePlan!;
    assert.equal(shared.stale,false);
    await controller.updateChangePlan({...shared,not_now:'Menschliche Ergänzung: kein Bulk-Delete',steps:[...shared.steps,{id:'human-check',title:'Offene Semantik klären',why:'Die Anforderung ist noch nicht eindeutig',files:[],acceptance:'Scope gemeinsam festlegen',source_ids:[]}]});
    assert.equal(controller.getState().changePlan!.steps.length,2);
    assert.equal(controller.getState().changePlan!.revision,shared.revision+1);
    await controller.usePlanStep('contract');
    assert.equal(controller.getState().scope,undefined,'Ein Planschritt darf keine neuen Schreibrechte erteilen');
    const sourceIndex=result.sources.findIndex(s=>s.path==='src/Service.cs');
    await controller.openSource(sourceIndex);
    assert.equal(vscode.window.activeTextEditor?.document.uri.toString(),code.toString());
    editor=await vscode.window.showTextDocument(doc);
    await editor.edit(edit=>edit.insert(new vscode.Position(0,0),'// Nachträglich geändert\n'));
    assert.equal(controller.getState().exploration?.stale,true);
    assert.equal(controller.getState().changePlan?.stale,true);
    await assert.rejects(controller.adoptChangePlan(),/veraltet/);
    await vscode.commands.executeCommand('tandem.open');
    const deadline=Date.now()+10_000;
    while(!controller.isRepositoryWorkbenchReady()&&Date.now()<deadline)await new Promise(resolve=>setTimeout(resolve,100));
    assert.equal(controller.isRepositoryWorkbenchReady(),true);
    console.log('PASS: Repository-Erkundung über echten Host mit Regeln, Quellbelegen, Editor-Snapshots und gesperrten Geheimnissen');
    console.log('PASS: Änderungsplan gemeinsam bearbeitbar, Quellen navigierbar, keine Schreibrechte erweitert und veraltete Pläne gesperrt');
  } finally {controller.stop();server.closeAllConnections();server.close();}
}
