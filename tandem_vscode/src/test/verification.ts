import * as assert from 'node:assert/strict';
import * as fs from 'node:fs/promises';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run(): Promise<void> {
  const extension = vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');
  assert.ok(extension);
  const controller = await extension.activate();
  const folder = vscode.workspace.workspaceFolders?.[0];
  assert.ok(folder);
  const uri = vscode.Uri.joinPath(folder.uri, 'WithBom.cs');
  await fs.writeFile(uri.fsPath, '\uFEFFclass Example { }\r\n', 'utf8');
  const doc = await vscode.workspace.openTextDocument(uri);
  const editor = await vscode.window.showTextDocument(doc);
  editor.selection = new vscode.Selection(new vscode.Position(0, 0), doc.positionAt(doc.getText().length));
  await controller.capture();
  const config = vscode.workspace.getConfiguration('tandem');
  await config.update('verificationProgram', process.env.TANDEM_TEST_NODE, vscode.ConfigurationTarget.Workspace);
  await config.update('verificationArgs', ['-e', 'process.exit(0)'], vscode.ConfigurationTarget.Workspace);
  const pending = controller.verify();
  await assert.rejects(controller.verify(), /noch aktiv/, 'Doppelklick darf keinen zweiten Prüfprozess starten');
  const evidence = await pending;
  assert.equal(evidence.status, 'passed', 'UTF-8-BOM ist keine Änderung zwischen Editorinhalt und gespeicherter Datei');
  assert.match(evidence.diskHash, /^[a-f0-9]{64}$/);
  console.log('PASS: BOM-Datei wird korrekt verifiziert; doppelte Prüfstarts werden abgewehrt');

  await config.update('verificationArgs', ['-e', `require('node:fs').appendFileSync(${JSON.stringify(uri.fsPath)}, '// Test-Nebenwirkung')`], vscode.ConfigurationTarget.Workspace);
  const modified = await controller.verify();
  assert.equal(modified.exitCode, 0);
  assert.equal(modified.status, 'stale', 'Exit-Code 0 darf geänderte Quelldaten nicht als geprüften alten Stand ausgeben');
  console.log('PASS: Test-Nebenwirkung am Quelltext invalidiert den Prüfbeleg trotz Exit-Code 0');
}
