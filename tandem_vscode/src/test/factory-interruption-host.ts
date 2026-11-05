import * as assert from 'node:assert/strict';
import * as http from 'node:http';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run():Promise<void> {
  const extension=vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');assert.ok(extension);
  const controller=await extension.activate(),folder=vscode.workspace.workspaceFolders?.[0];assert.ok(folder);await controller.newTask();
  const doc=await vscode.workspace.openTextDocument(vscode.Uri.joinPath(folder.uri,'Normalize.cs')),editor=await vscode.window.showTextDocument(doc);
  editor.selection=new vscode.Selection(0,0,doc.lineCount,0);await controller.capture();await controller.updatePlan({goal:'Randzeichen entfernen',not_now:'Keine Nebenarbeiten',acceptance:'Leerzeichenfall gezielt prüfen'});
  const waiting:http.ServerResponse[]=[];
  const server=http.createServer(async(request,response)=>{for await(const _chunk of request){}waiting.push(response);});
  await new Promise<void>(resolve=>server.listen(0,'127.0.0.1',resolve));const address=server.address();assert.ok(address&&typeof address!=='string');
  Object.assign(process.env,{AZURE_OPENAI_ENDPOINT:`http://127.0.0.1:${address.port}`,AZURE_OPENAI_API_KEY:'offline-worker-cancel',AZURE_OPENAI_DEPLOYMENT:'offline',AZURE_OPENAI_API_VERSION:'2024-10-21'});
  await vscode.workspace.getConfiguration('tandem').update('provider','azure',vscode.ConfigurationTarget.Global);controller.stop();
  const awaitRequest=async(index:number)=>{const deadline=Date.now()+10000;while(!waiting[index]&&Date.now()<deadline)await new Promise(resolve=>setTimeout(resolve,25));assert.ok(waiting[index],'Der Worker muss die lokale Azure-Testgegenstelle erreichen');};
  try{
    await controller.prepareFactoryJob();const first=controller.runFactoryJob(),rejected=assert.rejects(first,/gestoppt/);await awaitRequest(0);controller.stop();await rejected;
    assert.equal(controller.getState().factoryJob?.status,'stopped');assert.equal(controller.getState().factoryJob?.result,undefined);assert.equal(controller.getState().proposal,undefined);
    await controller.prepareFactoryJob();const second=controller.runFactoryJob(),stale=assert.rejects(second);await awaitRequest(1);
    const before=doc.getText();await editor.edit(edit=>edit.insert(new vscode.Position(0,0),'// Mensch\n'));await stale;
    assert.equal(controller.getState().factoryJob?.status,'stale');assert.equal(controller.getState().proposal,undefined);assert.equal(controller.getState().factoryJob?.result,undefined);assert.equal(doc.getText(),'// Mensch\n'+before);
    console.log('PASS: tatsächlicher Worker-Prozess wird bei Stop und menschlichem Edit beendet; kein spätes Ergebnis, kein Überschreiben menschlicher Arbeit.');
  }finally{controller.stop();server.closeAllConnections();server.close();}
}
