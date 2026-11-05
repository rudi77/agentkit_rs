import * as assert from 'node:assert/strict';
import * as http from 'node:http';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run(): Promise<void> {
  const extension = vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');
  assert.ok(extension);
  const controller = await extension.activate();
  const folder = vscode.workspace.workspaceFolders?.[0];
  assert.ok(folder);
  const doc = await vscode.workspace.openTextDocument(vscode.Uri.joinPath(folder.uri, 'Normalize.cs'));
  const editor = await vscode.window.showTextDocument(doc);
  editor.selection = new vscode.Selection(new vscode.Position(0, 0), doc.positionAt(doc.getText().length));
  await controller.capture();
  const original = doc.getText();
  const waiting: http.ServerResponse[] = [];
  const server = http.createServer(async (request, response) => {
    let body = '';
    for await (const chunk of request) { body += chunk; }
    const data = JSON.parse(body);
    if (data.messages.at(-1).role === 'tool') {
      response.writeHead(200, { 'Content-Type': 'text/event-stream' });
      response.end('data: {"choices":[{"delta":{"content":"Vorschlag ist bereit."}}]}\n\ndata: [DONE]\n\n');
    } else { waiting.push(response); }
  });
  await new Promise<void>(resolve => server.listen(0, '127.0.0.1', resolve));
  const address = server.address();
  assert.ok(address && typeof address !== 'string');
  // Nur der Kindprozess erhält diese Testwerte; er kann nicht auf die echte
  // Azure-Konfiguration zurückfallen, weil alle Provider-Felder gesetzt sind.
  process.env.AZURE_OPENAI_ENDPOINT = `http://127.0.0.1:${address.port}`;
  process.env.AZURE_OPENAI_API_KEY = 'offline-interruption-test';
  process.env.AZURE_OPENAI_DEPLOYMENT = 'offline';
  process.env.AZURE_OPENAI_API_VERSION = '2024-10-21';
  const config = vscode.workspace.getConfiguration('tandem');
  await config.update('provider', 'azure', vscode.ConfigurationTarget.Global);
  const awaitRequest = async (index: number) => {
    const deadline = Date.now() + 10_000;
    while (!waiting[index] && Date.now() < deadline) { await new Promise(resolve => setTimeout(resolve, 25)); }
    assert.ok(waiting[index], 'Der echte Azure-Adapter muss die lokale Testgegenstelle erreichen');
  };
  const release = (index: number) => {
    const patch = { path: controller.getState().scope!.path, old_text: 'return input;', new_text: 'return input.Trim();', reason: 'Unterbrechungstest' };
    const delta = { tool_calls: [{ index: 0, id: 'patch', type: 'function', function: { name: 'propose_patch', arguments: JSON.stringify(patch) } }] };
    waiting[index].writeHead(200, { 'Content-Type': 'text/event-stream' });
    waiting[index].end(`data: ${JSON.stringify({ choices: [{ delta }] })}\n\ndata: [DONE]\n\n`);
  };
  try {
    const first = controller.send('Patch gegen diesen Stand vorschlagen.');
    await awaitRequest(0);
    const { goal, acceptance } = controller.getState().plan;
    await controller.updatePlan({ goal, acceptance, not_now: 'Neue menschliche Einschränkung während des Modellaufrufs' });
    release(0);
    await first;
    assert.equal(controller.getState().proposal?.status, 'stale');
    await assert.rejects(controller.apply(), /veraltet/);
    assert.equal(doc.getText(), original);
    console.log('PASS: Contract-Änderung während eines echten laufenden Modellaufrufs sperrt spätes Ergebnis');

    const second = controller.send('Diesen laufenden Aufruf stoppen.');
    const rejection = assert.rejects(second, /gestoppt/);
    await awaitRequest(1);
    controller.stop();
    await rejection;
    assert.equal(controller.getState().proposal?.status, 'stale');
    assert.equal(doc.getText(), original);
    console.log('PASS: Stop beendet den wartenden Adapter und übernimmt kein spätes Ergebnis');

    const third = controller.send('Nach Stop erneut starten.');
    await awaitRequest(2);
    release(2);
    await third;
    assert.equal(controller.getState().proposal?.status, 'ready');
    assert.equal(doc.getText(), original);
    console.log('PASS: neuer AgentKit-Prozess nach Stop kann wieder kontrolliert vorschlagen');
  } finally {
    controller.stop();
    server.closeAllConnections(); server.close();
  }
}
