// Isolierter, rein lokaler Beispielarbeitsplatz für reproduzierbare UI-Aufnahmen.
import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import * as http from 'node:http';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run():Promise<void> {
  const extension=vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit')!;
  const controller=await extension.activate(),root=vscode.workspace.workspaceFolders![0].uri;await controller.newTask();
  const config=vscode.workspace.getConfiguration('tandem');
  await config.update('liveDiff',false,vscode.ConfigurationTarget.Global);
  await vscode.workspace.getConfiguration('window').update('title','TANDEM · Beispielprojekt · ${activeEditorShort}',vscode.ConfigurationTarget.Global);
  await vscode.workspace.getConfiguration('workbench').update('colorTheme','Default Dark Modern',vscode.ConfigurationTarget.Global);
  await vscode.workspace.getConfiguration('editor').update('fontSize',15,vscode.ConfigurationTarget.Global);
  const source=`using System;

public class PredictionService
{
    public int Predict(PredictionRequest request)
    {
        var isDirty = request.ImportRunning;
        if (isDirty)
            throw new ServiceUnavailableException();

        return PredictFrom(request.PublishedVersion);
    }

    private int PredictFrom(int version)
    {
        return version * 10;
    }
}

public class PredictionRequest
{
    public bool ImportRunning;
    public int PublishedVersion;
}

public class ServiceUnavailableException : Exception { }
`;
  const code=vscode.Uri.joinPath(root,'src','PredictionService.cs'),test=vscode.Uri.joinPath(root,'tests','PredictionTests.cs');
  await fs.mkdir(path.dirname(code.fsPath),{recursive:true});await fs.mkdir(path.dirname(test.fsPath),{recursive:true});await fs.mkdir(vscode.Uri.joinPath(root,'.tandem').fsPath,{recursive:true});
  await fs.writeFile(code.fsPath,source);await fs.writeFile(test.fsPath,'// Beispiel-Testidee, noch nicht ausgeführt\nvoid Prediction_uses_published_version_during_import() { }\n');
  await fs.writeFile(vscode.Uri.joinPath(root,'.tandem','architecture.json').fsPath,JSON.stringify({nodes:[{id:'prediction',label:'Prediction · veröffentlichter Stand',path:'src/PredictionService.cs',line:5},{id:'request',label:'Request · Importstatus und Planversion',path:'src/PredictionService.cs',line:20},{id:'test',label:'Parallelfall · Verhaltenstest',path:'tests/PredictionTests.cs',line:2}],edges:[{from:'prediction',to:'request',kind:'documented',reason:'Predict liest ImportRunning und PublishedVersion aus dem Request.'},{from:'prediction',to:'test',kind:'hypothesis',reason:'Dieser geplante Test muss das gewünschte Verhalten erst nachweisen.'}]}));
  const makeItem=(name:string,uri:vscode.Uri,line:number)=>new vscode.CallHierarchyItem(vscode.SymbolKind.Method,name,'Beispielprojekt',uri,new vscode.Range(line,0,line,45),new vscode.Range(line,15,line,22));
  const registrations=[vscode.languages.registerDocumentSymbolProvider({pattern:'**/PredictionService.cs'},{provideDocumentSymbols:()=>[
    new vscode.DocumentSymbol('Predict','PredictionRequest → int',vscode.SymbolKind.Method,new vscode.Range(4,4,11,5),new vscode.Range(4,15,4,22)),
    new vscode.DocumentSymbol('PredictFrom','int → int',vscode.SymbolKind.Method,new vscode.Range(13,4,16,5),new vscode.Range(13,16,13,27))]}),
    vscode.languages.registerReferenceProvider({pattern:'**/PredictionService.cs'},{provideReferences:()=>[new vscode.Location(test,new vscode.Range(1,5,1,14))]}),
    vscode.languages.registerCallHierarchyProvider({pattern:'**/PredictionService.cs'},{prepareCallHierarchy:()=>makeItem('Predict',code,4),provideCallHierarchyIncomingCalls:()=>[{from:makeItem('Prediction_uses_published_version_during_import',test,1),fromRanges:[]}],provideCallHierarchyOutgoingCalls:()=>[{to:makeItem('PredictFrom',code,13),fromRanges:[new vscode.Range(10,15,10,26)]}]})];
  let turn=0;let finish!:()=>void;
  const server=http.createServer(async(req,res)=>{
    if(req.method==='GET'){
      if(req.url==='/diff')await controller.preview();
      if(req.url==='/simulation'){await vscode.window.showTextDocument(code);await vscode.commands.executeCommand('tandem.simulate');}
      if(req.url==='/code')await vscode.window.showTextDocument(code,{selection:new vscode.Range(4,15,4,22)});
      if(req.url==='/finish')finish();res.end('ok');return;
    }
    let body='';for await(const part of req)body+=part;const data=JSON.parse(body);let delta:unknown;
    const tool=(name:string,args:unknown)=>({tool_calls:[{index:0,id:'screenshot-'+turn,type:'function',function:{name,arguments:JSON.stringify(args)}}]});
    if(JSON.stringify(data.tools).includes('propose_change_plan')){
      if(turn===0)delta=tool('read_file',{path:'src/PredictionService.cs'});
      else if(turn===1)delta=tool('read_file',{path:'tests/PredictionTests.cs'});
      else if(turn===2){const sources=data.messages.filter((m:any)=>m.role==='tool').map((m:any)=>JSON.parse(m.content).source).filter(Boolean);delta=tool('propose_change_plan',{goal:'Prediction während eines Kontenplanimports lesbar halten',not_now:'Keine neue Queue, keine zusätzliche Serviceklasse, kein allgemeines Refactoring.',open_questions:'',steps:[
        {id:'clarify',title:'Gewünschtes Leseverhalten klären',why:'Ein laufender Import darf die Prediction nicht pauschal blockieren.',files:['src/PredictionService.cs'],acceptance:'Der letzte veröffentlichte Stand ist fachlich zulässig.',source_ids:[sources[0].id]},
        {id:'published',title:'Veröffentlichten Stand verwenden',why:'Die vorhandene Planversion reicht für einen konsistenten Leseweg.',files:['src/PredictionService.cs'],acceptance:'ImportRunning=true und PublishedVersion=12 bleiben auswertbar.',source_ids:[sources[0].id],details:{needs_now:'Mit der letzten vollständig veröffentlichten Version weiterarbeiten.',not_now:'Keine neue Infrastruktur und keine API-Signaturänderung.',smallest_change:'Nur die pauschale Dirty-Sperre entfernen.',reuse:'PredictFrom und PublishedVersion bestehen bereits.',invariants:'Keine halbfertige Planversion lesen.',assumptions:'PublishedVersion stammt aus einem konsistenten Snapshot.',risks:'Fehlende Planversion als eigenen Fall prüfen.'}},
        {id:'verify',title:'Parallelfall real verifizieren',why:'Simulation allein beweist keine echte Nebenläufigkeit.',files:['tests/PredictionTests.cs'],acceptance:'Ein gezielter Verhaltenstest und relevante Regressionstests bestehen.',source_ids:[sources[1].id]}]});}
      else delta={content:'Der vorhandene Leseweg lässt sich lokal anpassen. Die Simulation und reale Tests bleiben getrennte Belege.'};turn++;
    } else if(data.messages.at(-1).role==='tool')delta={content:'Der kleine Vorschlag entfernt nur die pauschale Dirty-Sperre. Keine zusätzliche Klasse, keine reale Prüfung.'};
    else delta=tool('propose_patch',{path:controller.getState().scope!.path,old_text:'        if (isDirty)\n            throw new ServiceUnavailableException();\n',new_text:'        // Der veröffentlichte Stand bleibt auch während des Imports lesbar.\n',reason:'Der Request enthält bereits eine veröffentlichte Planversion. Der Vorschlag bewahrt den vorhandenen PredictFrom-Leseweg.'});
    res.writeHead(200,{'Content-Type':'text/event-stream'});res.end(`data: ${JSON.stringify({choices:[{delta}]})}\n\ndata: [DONE]\n\n`);
  });
  await new Promise<void>(resolve=>server.listen(0,'127.0.0.1',resolve));const address=server.address() as {port:number};
  process.env.AZURE_OPENAI_ENDPOINT=`http://127.0.0.1:${address.port}`;process.env.AZURE_OPENAI_API_KEY='offline-screenshots';process.env.AZURE_OPENAI_DEPLOYMENT='offline';
  await config.update('provider','azure',vscode.ConfigurationTarget.Global);controller.stop();
  try{
    await controller.exploreRepository('Prediction soll während eines Imports die letzte veröffentlichte Planversion verwenden. Gemeinsam die kleinste Änderung prüfen.');await controller.adoptChangePlan();await controller.usePlanStep('published');
    let editor=await vscode.window.showTextDocument(code);editor.selection=new vscode.Selection(4,4,11,5);await controller.capture();
    await controller.send('Bitte nur den vereinbarten kleinen Eingriff vorschlagen.','pair');
    await controller.importArchitecture();await controller.runSimulation({name:'Import läuft · veröffentlichte Version V12',version:1,inputs:{request:{ImportRunning:true,PublishedVersion:12}},stubs:{}},true);
    await vscode.window.showTextDocument(code,{selection:new vscode.Range(4,15,4,22)});await vscode.commands.executeCommand('tandem.refreshMap');await vscode.commands.executeCommand('tandem.open');
    await fs.mkdir(path.join(extension.extensionPath,'test-results','ui'),{recursive:true});
    await fs.writeFile(path.join(extension.extensionPath,'test-results','screenshots-ready.json'),JSON.stringify({port:address.port,root:root.fsPath}));
    console.log('SCREENSHOTS_READY '+address.port);
    await new Promise<void>(resolve=>{finish=resolve;setTimeout(resolve,900000);});
  }finally{controller.stop();registrations.forEach(item=>item.dispose());server.closeAllConnections();server.close();}
}
