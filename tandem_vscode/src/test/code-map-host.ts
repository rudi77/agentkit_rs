import * as assert from 'node:assert/strict';
import * as vscode from 'vscode';
import { CodeMap } from '../code-map';
import { State } from '../model';

export async function run():Promise<void> {
  const folder=vscode.workspace.workspaceFolders?.[0];assert.ok(folder);const uri=vscode.Uri.joinPath(folder.uri,'Map.cs');
  await vscode.workspace.fs.writeFile(uri,Buffer.from('class Map {\n int Entry() { return Next(); }\n int Next() { return 1; }\n}\n'));
  const editor=await vscode.window.showTextDocument(uri);editor.selection=new vscode.Selection(1,5,1,5);
  const first=new vscode.DocumentSymbol('Entry','',vscode.SymbolKind.Method,new vscode.Range(1,0,1,30),new vscode.Range(1,5,1,10));
  const second=new vscode.DocumentSymbol('Next','',vscode.SymbolKind.Method,new vscode.Range(2,0,2,25),new vscode.Range(2,5,2,9));
  const item=(name:string,line:number)=>new vscode.CallHierarchyItem(vscode.SymbolKind.Method,name,'Map',uri,new vscode.Range(line,0,line,25),new vscode.Range(line,5,line,9));
  const registrations=[vscode.languages.registerDocumentSymbolProvider({pattern:'**/Map.cs'},{provideDocumentSymbols:()=>[first,second]}),
    vscode.languages.registerReferenceProvider({pattern:'**/Map.cs'},{provideReferences:()=>[new vscode.Location(uri,new vscode.Range(1,21,1,25))]}),
    vscode.languages.registerCallHierarchyProvider({pattern:'**/Map.cs'},{prepareCallHierarchy:()=>item('Entry',1),provideCallHierarchyIncomingCalls:()=>[],provideCallHierarchyOutgoingCalls:()=>[{to:item('Next',2),fromRanges:[new vscode.Range(1,21,1,25)]}]})];
  const state:State={plan:{revision:1,goal:'Test',not_now:'',acceptance:''},chat:[],audit:[]};let checked=0;
  const map=new CodeMap(()=>state,async target=>{assert.equal(target.toString(),uri.toString());checked++;});
  try{
    await map.focus(editor);const roots=map.getChildren();assert.equal(roots.length,3);const focus=roots[2];assert.equal(focus.label,'Entry');assert.equal(focus.provenance,'analysis');
    const outgoing=focus.children.find(node=>node.label==='Aufgerufene Methoden');assert.equal(outgoing?.children[0].label,'Next');assert.equal(outgoing.children[0].uri?.toString(),uri.toString());
    const refs=focus.children.find(node=>node.label==='Referenzen / mögliche Auswirkung');assert.equal(refs?.children[0].provenance,'analysis');assert.ok(checked);
    editor.selection=new vscode.Selection(2,5,2,5);await map.focus(editor);assert.equal(map.getChildren()[2].label,'Next');
    console.log('PASS: echte Sprachdienst-APIs verknüpfen aktuelle Methode, Aufrufe, Referenzen und Codeanker; Codeauswahl fokussiert den Viewer.');
  }finally{map.dispose();registrations.forEach(item=>item.dispose());}
}
