import * as assert from 'node:assert/strict';
import * as vscode from 'vscode';
import { CodeMap } from '../code-map';

export async function run():Promise<void> {
  const folder=vscode.workspace.workspaceFolders?.[0];assert.ok(folder);
  const uri=vscode.Uri.joinPath(folder.uri,'RaceMap.cs');await vscode.workspace.fs.writeFile(uri,Buffer.from('class Map { int Read() { return 1; } }'));
  const editor=await vscode.window.showTextDocument(uri);editor.selection=new vscode.Selection(0,16,0,16);
  const symbol=new vscode.DocumentSymbol('Read','',vscode.SymbolKind.Method,new vscode.Range(0,12,0,34),new vscode.Range(0,16,0,20));
  const provider=vscode.languages.registerDocumentSymbolProvider({pattern:'**/RaceMap.cs'},{provideDocumentSymbols:async()=>{await new Promise(resolve=>setTimeout(resolve,350));return[symbol];}});
  const map=new CodeMap(()=>({plan:{revision:1,goal:'Kontext sehen',not_now:'',acceptance:''},chat:[],audit:[]}),async()=>{});
  try{
    const first=map.focus(editor);map.schedule(editor);const same=map.focus(editor);await Promise.all([first,same]);
    assert.equal(map.getChildren().length,3);assert.equal(map.getContext()?.focus.name,'Read');assert.equal(map.getChildren()[2].label,'Read');
    console.log('PASS: verzögerter Sprachdienst und doppelte Fokusanforderungen liefern dieselbe abgeschlossene Softwarekarte statt eines überholenden Ladezustands.');
  }finally{map.dispose();provider.dispose();}
}
