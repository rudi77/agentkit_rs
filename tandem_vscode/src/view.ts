import { randomBytes } from 'node:crypto';
import * as vscode from 'vscode';
import { workbenchHtml } from './workbench-html';

export class WorkbenchView implements vscode.WebviewViewProvider {
  private view?: vscode.WebviewView;
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
    view.webview.html = workbenchHtml(resource('workbench.js'), resource('workbench.css'), randomBytes(16).toString('hex'), view.webview.cspSource);
    view.webview.onDidReceiveMessage(async message => {
      if (message.action === 'ready') { this.ready = true; }
      if (message.action === 'repositoryReady') { this.repositoryReady = true; }
      try { await this.handle(message); }
      catch (error) { void view.webview.postMessage({ type: 'error', text: error instanceof Error ? error.message : String(error) }); }
    });
    this.post(this.latest);
  }

  post(message: unknown): void {
    this.latest = message;
    if (message) { void this.view?.webview.postMessage(message); }
  }
}
