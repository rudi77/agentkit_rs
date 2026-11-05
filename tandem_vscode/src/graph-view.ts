import * as vscode from 'vscode';
import { randomBytes } from 'node:crypto';
import { graphHtml } from './graph-html';
import { GraphLayer, GraphNode, SoftwareGraph } from './software-graph';

export class GraphView implements vscode.Disposable {
  ready=false;
  private panel?:vscode.WebviewPanel;
  private latest?:SoftwareGraph;
  constructor(private readonly extensionUri:vscode.Uri,private readonly navigate:(node:GraphNode)=>Promise<void>,private readonly activateStep:(id:string)=>Promise<void>){}
  open():void {
    if(this.panel){this.panel.reveal(this.panel.viewColumn,true);return;}
    const panel=vscode.window.createWebviewPanel('tandem.graph','TANDEM · Softwarekarte',{viewColumn:vscode.ViewColumn.Beside,preserveFocus:true},{enableScripts:true,retainContextWhenHidden:true,localResourceRoots:[vscode.Uri.joinPath(this.extensionUri,'media')]});
    this.panel=panel;
    this.ready=false;
    const resource=(file:string)=>panel.webview.asWebviewUri(vscode.Uri.joinPath(this.extensionUri,'media',file)).toString();
    panel.webview.html=graphHtml(resource('graph.js'),resource('graph.css'),randomBytes(18).toString('base64'),panel.webview.cspSource);
    panel.webview.onDidReceiveMessage(async message=>{try{
      if(message.action==='graphReady'){this.ready=true;this.post();return;}
      const node=this.node(message.layer,message.id,message.revision);
      if(message.action==='graphOpenNode')await this.navigate(node);
      else if(message.action==='graphActivateStep'&&node.step_id)await this.activateStep(node.step_id);
      else throw new Error('Unbekannte Kartenaktion.');
    }catch(error){void panel.webview.postMessage({type:'error',text:error instanceof Error?error.message:String(error)});}});
    panel.onDidDispose(()=>{this.panel=undefined;this.ready=false;});this.post();
  }
  update(graph:SoftwareGraph):void {if(this.latest?.revision===graph.revision)return;this.latest=graph;this.post();}
  getSnapshot():SoftwareGraph|undefined{return this.latest;}
  getColumn():vscode.ViewColumn|undefined{return this.panel?.viewColumn;}
  private node(layer:GraphLayer,id:string,revision:string):GraphNode {
    if(!['product','architecture','local','impact'].includes(layer))throw new Error('Unbekannte Kartenebene.');
    if(!this.latest||revision!==this.latest.revision)throw new Error('Der Kartenausschnitt wurde aktualisiert. Bitte das Element erneut wählen.');
    const node=this.latest.layers[layer]?.nodes.find(node=>node.id===id);if(!node)throw new Error('Dieses Kartenelement existiert nicht.');return node;
  }
  private post():void{if(this.latest)void this.panel?.webview.postMessage({type:'graphState',graph:this.latest});}
  dispose():void{this.panel?.dispose();}
}
