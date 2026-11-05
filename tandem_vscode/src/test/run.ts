import * as assert from 'node:assert/strict';
import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import * as vscode from 'vscode';
import type { TandemController } from '../extension';

export async function run(): Promise<void> {
  const extension = vscode.extensions.getExtension<TandemController>('agentkit-local.tandem-agentkit');
  assert.ok(extension, 'Extension muss im echten Host verfügbar sein');
  const controller = await extension.activate();
  const folder = vscode.workspace.workspaceFolders?.[0];
  assert.ok(folder);
  const file = vscode.Uri.joinPath(folder.uri, 'Normalize.cs');
  const doc = await vscode.workspace.openTextDocument(file);
  if (process.env.TANDEM_TEST_PHASE === 'reload') {
    const state = controller.getState();
    assert.equal(state.plan.goal, 'Rand-Leerzeichen entfernen');
    assert.equal(state.proposal?.status, 'stale', 'Nicht angewendete Vorschläge nach Host-Neustart neu prüfen');
    assert.ok(state.audit.some(entry => entry.action.includes('passed')));
    assert.ok(doc.getText().startsWith('// Mensch: 🦀 bleibt erhalten\n'));
    console.log('PASS: Plan, Herkunft, Prüfung und veralteter Vorschlag nach Neustart erhalten');
    return;
  }
  const config = vscode.workspace.getConfiguration('tandem');
  await config.update('provider', 'demo', vscode.ConfigurationTarget.Global);
  await config.update('verificationProgram', process.env.TANDEM_TEST_NODE, vscode.ConfigurationTarget.Workspace);
  await config.update('verificationArgs', [path.join(folder.uri.fsPath, 'verify.cjs')], vscode.ConfigurationTarget.Workspace);

  async function capture(): Promise<void> {
    const editor = await vscode.window.showTextDocument(doc);
    const start = doc.getText().indexOf('    public static string');
    const end = doc.getText().lastIndexOf('}');
    editor.selection = new vscode.Selection(doc.positionAt(start), doc.positionAt(end));
    await controller.capture();
  }
  await capture();
  await controller.updatePlan({ goal: 'Rand-Leerzeichen entfernen', not_now: 'Keine neue Klasse, keine anderen Methoden', acceptance: 'Quelltextcheck; Verhaltenstest separat ergänzen' });
  const before = doc.getText();
  const failed = await controller.verify();
  assert.equal(failed.status, 'failed', 'Fehlgeschlagene reale Prüfung darf nicht grün werden');
  assert.equal(failed.exitCode, 1);
  console.log('PASS: echtes Prüfprogramm meldet fehlenden Patch');

  const result = await controller.send('Entferne Leerzeichen am Rand mit der vorhandenen Methode.');
  assert.equal(result.patch?.new_text, 'return input.Trim();');
  assert.equal(doc.getText(), before, 'Vorschlag darf Editorinhalt nicht schreiben');
  assert.equal(await fs.readFile(file.fsPath, 'utf8'), before, 'Vorschlag darf Datei nicht schreiben');
  await controller.preview();
  assert.ok(vscode.window.tabGroups.all.some(group => group.tabs.some(tab => tab.input instanceof vscode.TabInputTextDiff)), 'Nativer Diff muss geöffnet sein');
  console.log('PASS: AgentKit erzeugt Vorschlag ohne Writes und zeigt nativen Diff');

  await controller.updatePlan({ goal: 'Rand-Leerzeichen entfernen', not_now: 'Keine API-Änderung; neue Einschränkung', acceptance: 'Quelltextcheck' });
  await assert.rejects(controller.apply(), /veraltet/);
  assert.equal(doc.getText(), before);
  console.log('PASS: Contract-Änderung sperrt alten Patch');

  await capture();
  await controller.send('Bitte den minimalen Patch vorschlagen.');
  const editor = await vscode.window.showTextDocument(doc);
  await editor.edit(edit => edit.insert(new vscode.Position(0, 0), '// Mensch: 🦀 bleibt erhalten\n'));
  await assert.rejects(controller.apply(), /veraltet/);
  assert.ok(doc.getText().startsWith('// Mensch: 🦀 bleibt erhalten\n'));
  await doc.save();
  console.log('PASS: menschlicher Edit invalidiert Patch und bleibt erhalten');

  await capture();
  await controller.send('Bitte erneut gegen den aktuellen Stand vorschlagen.');
  await controller.apply();
  assert.equal(controller.getState().proposal?.status, 'applied');
  assert.ok(doc.getText().includes('return input.Trim();'));
  assert.ok(doc.getText().startsWith('// Mensch: 🦀 bleibt erhalten\n'));
  assert.equal(controller.getState().evidence, undefined, 'Übernahme ist keine Verifikation');
  await assert.rejects(controller.apply(), /veraltet/);
  const passed = await controller.verify();
  assert.equal(passed.status, 'passed');
  assert.equal(passed.exitCode, 0);
  assert.equal(await fs.readFile(file.fsPath, 'utf8'), doc.getText());
  console.log('PASS: kontrollierte Übernahme und echter Prüfprozess mit Exit-Code 0');

  await vscode.window.showTextDocument(doc);
  await vscode.commands.executeCommand('undo');
  assert.ok(doc.getText().includes('return input;'), 'Native Undo muss den TANDEM-Edit zurücknehmen');
  assert.ok(doc.getText().startsWith('// Mensch: 🦀 bleibt erhalten\n'));
  assert.equal(controller.getState().evidence?.status, 'stale', 'Codeänderung invalidiert echte Evidenz');
  await doc.save();
  await capture();
  await controller.send('Noch einen Vorschlag für die Prüfung des Wiederladens.');
  assert.equal(controller.getState().proposal?.status, 'ready');
  console.log('PASS: Undo erhält menschliche Arbeit; Evidenz wird veraltet; Zustand gespeichert');
}
