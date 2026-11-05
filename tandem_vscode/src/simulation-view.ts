import * as vscode from 'vscode';
import { simulationHtml } from './simulation-html';
import { randomBytes } from 'node:crypto';

export class SimulationView implements vscode.WebviewViewProvider {
  private view?:vscode.WebviewView;
  constructor(private readonly extensionUri:vscode.Uri,private readonly onMessage:(message:any)=>Promise<unknown>,private readonly refresh:()=>void){}
  resolveWebviewView(view:vscode.WebviewView):void {
    this.view=view;view.webview.options={enableScripts:true,localResourceRoots:[vscode.Uri.joinPath(this.extensionUri,'media')]};
    const css=view.webview.asWebviewUri(vscode.Uri.joinPath(this.extensionUri,'media','workbench.css'));
    const script=view.webview.asWebviewUri(vscode.Uri.joinPath(this.extensionUri,'media','simulation.js'));
    view.webview.html=simulationHtml(String(script),String(css),randomBytes(18).toString('base64'),view.webview.cspSource);
    view.webview.onDidReceiveMessage(message=>{if(message.action==='simulationReady'){this.refresh();return;}void this.onMessage(message).catch(error=>view.webview.postMessage({type:'error',text:error instanceof Error?error.message:String(error)}));});
    view.onDidDispose(()=>{this.view=undefined;});this.refresh();
  }
  post(message:unknown):void{void this.view?.webview.postMessage(message);}
}

