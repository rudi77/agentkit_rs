import * as assert from 'node:assert/strict';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run():Promise<void> {
  const extension=vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');assert.ok(extension);const controller=await extension.activate(),folder=vscode.workspace.workspaceFolders?.[0];assert.ok(folder);
  await controller.newTask();await vscode.workspace.getConfiguration('tandem').update('provider','demo',vscode.ConfigurationTarget.Global);controller.stop();
  const uri=vscode.Uri.joinPath(folder.uri,'Graph.cs'),source='class Graph {\n string Run(string input) {\n  var value = Next(input);\n  return input;\n }\n string Next(string input) { return input; }\n}\n';
  await vscode.workspace.fs.writeFile(uri,Buffer.from(source));const editor=await vscode.window.showTextDocument(uri,{viewColumn:vscode.ViewColumn.One});
  const symbol=(name:string,start:number,end:number)=>new vscode.DocumentSymbol(name,'',vscode.SymbolKind.Method,new vscode.Range(start,0,end,50),new vscode.Range(start,8,start,12));
  const item=(name:string,line:number)=>new vscode.CallHierarchyItem(vscode.SymbolKind.Method,name,'Graph',uri,new vscode.Range(line,0,line,45),new vscode.Range(line,8,line,12));
  const registrations=[vscode.languages.registerDocumentSymbolProvider({pattern:'**/Graph.cs'},{provideDocumentSymbols:()=>[symbol('Run',1,4),symbol('Next',5,5)]}),vscode.languages.registerReferenceProvider({pattern:'**/Graph.cs'},{provideReferences:()=>[new vscode.Location(uri,new vscode.Range(2,14,2,18))]}),vscode.languages.registerCallHierarchyProvider({pattern:'**/Graph.cs'},{prepareCallHierarchy:(_doc,position)=>item(position.line===5?'Next':'Run',position.line===5?5:1),provideCallHierarchyIncomingCalls:()=>[],provideCallHierarchyOutgoingCalls:()=>[{to:item('Next',5),fromRanges:[new vscode.Range(2,14,2,18)]}]})];
  try{
    editor.selection=new vscode.Selection(1,0,4,2);await controller.capture();await controller.updatePlan({goal:'Eingabe lokal normalisieren',not_now:'Keine neue Klasse',acceptance:'Gezielten Test ausführen'});await controller.send('Kleinsten Vorschlag erstellen','pair');
    const dir=vscode.Uri.joinPath(folder.uri,'.tandem');await vscode.workspace.fs.createDirectory(dir);await vscode.workspace.fs.writeFile(vscode.Uri.joinPath(dir,'architecture.json'),Buffer.from(JSON.stringify({nodes:[{id:'run',label:'Einstieg',path:'Graph.cs',line:2},{id:'next',label:'Vorhandene Hilfsmethode',path:'Graph.cs',line:6}],edges:[{from:'run',to:'next',kind:'documented',reason:'Run ruft Next auf.'}]})));await controller.importArchitecture();
    await vscode.window.showTextDocument(uri,{viewColumn:vscode.ViewColumn.One,selection:new vscode.Range(1,8,1,8)});await vscode.commands.executeCommand('tandem.refreshMap');controller.openGraph();
    const deadline=Date.now()+10000;while((!controller.isGraphReady()||controller.getGraphSnapshot()?.context?.pending||!controller.getGraphSnapshot()?.layers.local.nodes.some(node=>node.label==='Next'))&&Date.now()<deadline)await new Promise(resolve=>setTimeout(resolve,50));assert.equal(controller.isGraphReady(),true,'Die tatsächliche Karten-Webview muss mit CSP laden und den Host erreichen');
    let graph=controller.getGraphSnapshot();assert.ok(graph);assert.equal(graph.layers.architecture.edges[0].origin,'documented');
    assert.ok(graph.layers.local.nodes.some(node=>node.label==='Next'));assert.ok(graph.layers.impact.nodes.length>1);
    const target=graph.layers.architecture.nodes.find(node=>node.id==='architecture:next');assert.ok(target);await controller.openGraphNode(target);
    assert.ok(vscode.window.visibleTextEditors.some(item=>item.document.uri.toString()===uri.toString()&&item.selection.start.line===5&&item.viewColumn===vscode.ViewColumn.One),'Knoten muss in die ursprüngliche Codespalte navigieren');
    assert.ok(vscode.window.tabGroups.all.some(group=>group.tabs.some(tab=>tab.input instanceof vscode.TabInputWebview&&tab.label.includes('Softwarekarte'))),'Die Karte darf durch Code-Navigation nicht ersetzt werden');
    await assert.rejects(controller.openGraphNode({...target,anchor:{uri:vscode.Uri.joinPath(folder.uri,'.env').toString(),path:'.env',line:1}}),/gesperrt/);
    const revision=graph.revision,currentEditor=vscode.window.visibleTextEditors.find(editor=>editor.document.uri.toString()===uri.toString());assert.ok(currentEditor);await currentEditor.edit(edit=>edit.insert(new vscode.Position(0,0),'// Neue Erkenntnis\n'));await controller.updatePlan({goal:'Geänderten Stand besprechen',not_now:'Keine Nebenarbeiten',acceptance:'Neu prüfen'});graph=controller.getGraphSnapshot();assert.notEqual(graph?.revision,revision);assert.ok(graph?.layers.architecture.nodes.every(node=>node.stale));
    console.log('PASS: echte grafische Karten-Webview, Sprachdienst-Kontext und Impact, dokumentierte Herkunft, geschützte Code-Navigation in eigener Spalte und sichtbare Veraltung.');
  }finally{controller.stop();registrations.forEach(item=>item.dispose());}
}
