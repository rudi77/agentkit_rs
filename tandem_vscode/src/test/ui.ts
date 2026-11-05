import * as assert from 'node:assert/strict';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run(): Promise<void> {
  const extension = vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');
  assert.ok(extension);
  const controller = await extension.activate();
  await vscode.commands.executeCommand('tandem.open');
  const deadline = Date.now() + 10_000;
  while (!controller.isWorkbenchReady() && Date.now() < deadline) {
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  assert.equal(controller.isWorkbenchReady(), true, 'Die echte Webview muss ihr JavaScript laden und den Ready-Handshake senden');
  console.log('PASS: echte Webview lädt CSP-geschütztes JavaScript und erreicht den Extension Host');
}
