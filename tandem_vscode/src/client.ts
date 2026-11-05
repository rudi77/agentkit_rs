import { spawn, ChildProcessWithoutNullStreams } from 'node:child_process';
import * as fs from 'node:fs';
import * as path from 'node:path';
import * as readline from 'node:readline';
import * as vscode from 'vscode';

export class AgentClient implements vscode.Disposable {
  private child?: ChildProcessWithoutNullStreams;
  private nextId = 0;
  private workspaceRoot?: string;
  private pending = new Map<number, { resolve: (value: any) => void; reject: (error: Error) => void; timer: NodeJS.Timeout; event?: (event: any) => void }>();

  constructor(private readonly extensionPath: string, private readonly output: vscode.OutputChannel) {}

  executablePath():string {
    const config = vscode.workspace.getConfiguration('tandem');
    const executable = process.platform === 'win32' ? 'tandem.exe' : 'tandem';
    const configured = config.get<string>('binaryPath', '');
    if (configured && !path.isAbsolute(configured)) { throw new Error('tandem.binaryPath muss absolut sein.'); }
    const candidates = configured ? [configured] : [
      path.join(this.extensionPath, 'bin', executable),
      path.join(this.extensionPath, '..', 'tandem_app', 'target', 'debug', executable),
    ];
    const binary = candidates.find(candidate => fs.existsSync(candidate));
    if (!binary) { throw new Error('TANDEM-Binary fehlt. Im Extension-Verzeichnis npm run build:backend ausführen.'); }
    return binary;
  }

  private start(workspace?: string): ChildProcessWithoutNullStreams {
    if (this.child && this.workspaceRoot !== workspace) { this.stop('Repository gewechselt; bisherigen Adapter beendet.'); }
    if (this.child) { return this.child; }
    const config = vscode.workspace.getConfiguration('tandem');
    const binary=this.executablePath();
    const provider = config.get<string>('provider', 'auto');
    this.output.appendLine(`AgentKit-Adapter starten · Anbieter: ${provider}`);
    const args = ['--provider', provider];
    if (workspace) { args.push('--workspace', workspace); }
    const child = spawn(binary, args, { windowsHide: true, stdio: 'pipe' });
    this.child = child;
    this.workspaceRoot = workspace;
    const lines = readline.createInterface({ input: child.stdout });
    lines.on('line', line => {
      try {
        const msg = JSON.parse(line);
        if (msg.method === 'event') { this.pending.get(msg.params.run_id)?.event?.(msg.params.event); return; }
        const pending = this.pending.get(msg.id);
        if (!pending) { return; }
        clearTimeout(pending.timer);
        this.pending.delete(msg.id);
        if (msg.error) { pending.reject(new Error(msg.error.message)); }
        else { pending.resolve(msg.result); }
      } catch { this.output.appendLine('Ungültige Protokollnachricht vom Adapter.'); }
    });
    child.stderr.on('data', data => this.output.append(data.toString()));
    const failed = (message: string) => {
      lines.close();
      if (this.child !== child) { return; }
      this.child = undefined;
      this.rejectAll(message);
    };
    child.on('error', error => failed(`Adapter konnte nicht starten: ${error.message}`));
    child.on('exit', code => failed(`Adapter beendet (${code ?? 'Signal'}).`));
    return child;
  }

  request<T>(method: string, params: unknown, event?: (event: any) => void, workspace?: string): Promise<T> {
    const child = this.start(workspace);
    const id = ++this.nextId;
    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => { this.stop('Zeitlimit des Agentenlaufs erreicht.'); }, 180_000);
      this.pending.set(id, { resolve, reject, timer, event });
      child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n', error => {
        if (error) { this.stop(`Protokollfehler: ${error.message}`); }
      });
    });
  }

  private rejectAll(message: string): void {
    for (const item of this.pending.values()) { clearTimeout(item.timer); item.reject(new Error(message)); }
    this.pending.clear();
  }

  stop(message = 'Lauf gestoppt. Nicht übernommene Ergebnisse des Laufs wurden verworfen.'): void {
    const child = this.child;
    this.child = undefined;
    child?.kill();
    this.rejectAll(message);
  }

  dispose(): void { this.stop(); }
}
