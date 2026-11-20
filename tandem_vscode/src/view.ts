import { randomBytes } from 'node:crypto';
import * as vscode from 'vscode';
import { workbenchHtml } from './workbench-html';
import { chatHtml } from './chat-html';

export class WorkbenchView implements vscode.WebviewViewProvider {
  private view?: vscode.WebviewView;
  private details?:vscode.WebviewPanel;
  private detailsReady?:Promise<void>;
  private latest: unknown;
  ready = false;
  repositoryReady = false;
  constructor(private readonly handle: (message: any) => Promise<unknown>, private readonly extensionUri: vscode.Uri) {}

  resolveWebviewView(view: vscode.WebviewView): void {
    this.view = view;
    this.ready = false;
    this.repositoryReady = false;
    view.webview.options = { enableScripts: true, localResourceRoots: [vscode.Uri.joinPath(this.extensionUri, 'media')] };
    const resource = (file: string) => view.webview.asWebviewUri(vscode.Uri.joinPath(this.extensionUri, 'media', file)).toString();
    view.webview.html = chatHtml(resource('chat.js'), resource('chat.css'), randomBytes(16).toString('hex'), view.webview.cspSource);
    view.webview.onDidReceiveMessage(async message => {
      if (message.action === 'ready') { this.ready = true; }
      if (message.action === 'repositoryReady') { this.repositoryReady = true; }
      try { await this.handle(message); }
      catch (error) { void view.webview.postMessage({ type: 'error', text: error instanceof Error ? error.message : String(error) }); }
    });
    this.post(this.latest);
  }

  async openDetails():Promise<void> {
    if(this.details){this.details.reveal();return this.detailsReady;}
    const panel=vscode.window.createWebviewPanel('tandem.details','TANDEM · Details & Werkzeuge',vscode.ViewColumn.Beside,{enableScripts:true,localResourceRoots:[vscode.Uri.joinPath(this.extensionUri,'media')],retainContextWhenHidden:true});
    this.details=panel;
    this.detailsReady=new Promise<void>((resolve,reject)=>{
      const timer=setTimeout(()=>reject(new Error('Die Detailansicht konnte nicht geladen werden. Bitte erneut öffnen.')),10000);
      panel.webview.onDidReceiveMessage(async message=>{
        if(message.action==='ready'){clearTimeout(timer);resolve();}
        try{await this.handle(message);}catch(error){void panel.webview.postMessage({type:'error',text:error instanceof Error?error.message:String(error)});}
      });
      panel.onDidDispose(()=>{clearTimeout(timer);if(this.details===panel){this.details=undefined;this.detailsReady=undefined;}resolve();});
    });
    const resource=(file:string)=>panel.webview.asWebviewUri(vscode.Uri.joinPath(this.extensionUri,'media',file)).toString();
    panel.webview.html=workbenchHtml(resource('workbench.js'),resource('workbench.css'),randomBytes(16).toString('hex'),panel.webview.cspSource);
    if(this.latest)void panel.webview.postMessage(this.latest);
    return this.detailsReady;
  }

  dispose():void {this.details?.dispose();}

  post(message: unknown): void {
    this.latest = message;
    if (message) { void this.view?.webview.postMessage(message);void this.details?.webview.postMessage(message); }
  }
}
