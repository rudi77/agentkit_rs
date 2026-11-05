const { runTests } = require('@vscode/test-electron');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

async function main() {
  const source = path.resolve(__dirname, '..');
  const extension = process.env.TANDEM_TEST_EXTENSION_PATH || source;
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'tandem-e2e-'));
  const workspace = path.join(root, 'workspace');
  fs.mkdirSync(workspace);
  for (const file of ['Normalize.cs', 'verify.cjs']) {
    fs.copyFileSync(path.join(source, 'demo', file), path.join(workspace, file));
  }
  // Ein eigener Host mit eigenen Einstellungen lässt die geöffnete IDE und die
  // echte Azure-Konfiguration des Entwicklers unverändert.
  const installed = process.env.TANDEM_VSCODE_EXECUTABLE || (process.platform === 'win32'
    ? path.join(process.env.LOCALAPPDATA, 'Programs/Microsoft VS Code/Code.exe') : undefined);
  const options = {
    ...(installed && fs.existsSync(installed) ? { vscodeExecutablePath: installed } : {}),
    extensionDevelopmentPath: extension,
    extensionTestsPath: path.join(source, 'out/test/run.js'),
    launchArgs: [workspace, '--user-data-dir', path.join(root, 'user'), '--extensions-dir', path.join(root, 'extensions'),
      '--disable-extensions', '--skip-welcome', '--skip-release-notes', '--disable-workspace-trust'],
  };
  const phases=process.env.TANDEM_TEST_PHASES?.split(',')||['flow', 'reload', 'ui', 'interruption', 'verification', 'repository', 'repository-reload', 'guidance','code-map-host','contract-host','simulation-host','delegation-host','cooperation-v2','graph-host','verification-workspace-host','factory-host','factory-interruption-host','code-map-race-host','simulation-loops-host','simulation-invalidation-host'];
  // Die bisherigen Reload-Slices teilen bewusst ihren Zustand. Die zusätzlichen
  // Fähigkeiten erhalten eigene Projekte, damit ungespeicherte Edits aus einem
  // anderen Test nicht als vermeintlich aktuelle Prüfgrundlage auftreten.
  const independent=new Set(['code-map-host','contract-host','simulation-host','delegation-host','cooperation-v2','graph-host','verification-workspace-host','factory-host','factory-interruption-host','code-map-race-host','simulation-loops-host','simulation-invalidation-host']);
  for (const phase of phases) {
    let phaseOptions=options;
    if(independent.has(phase)){
      const phaseRoot=fs.mkdtempSync(path.join(os.tmpdir(),'tandem-'+phase+'-')),phaseWorkspace=path.join(phaseRoot,'workspace');fs.mkdirSync(phaseWorkspace);
      for(const file of ['Normalize.cs','verify.cjs'])fs.copyFileSync(path.join(source,'demo',file),path.join(phaseWorkspace,file));
      phaseOptions={...options,launchArgs:[phaseWorkspace,'--user-data-dir',path.join(phaseRoot,'user'),'--extensions-dir',path.join(phaseRoot,'extensions'),'--disable-extensions','--skip-welcome','--skip-release-notes','--disable-workspace-trust']};
    }
    await runTests({ ...phaseOptions,
      extensionTestsPath: path.join(source, ['flow','reload'].includes(phase) ? 'out/test/run.js' : `out/test/${phase}.js`),
      extensionTestsEnv: {
        TANDEM_TEST_PHASE: phase, TANDEM_TEST_NODE: process.execPath,
        // Ein versehentlich zu früh gestarteter Adapter darf niemals die echte
        // Benutzerkonfiguration für einen automatisierten Test verwenden.
        AZURE_OPENAI_ENDPOINT:'http://127.0.0.1:1', AZURE_OPENAI_API_KEY:'offline-test-only',
        AZURE_OPENAI_DEPLOYMENT:'offline', AZURE_OPENAI_API_VERSION:'2024-10-21',
        OPENAI_BASE_URL:'http://127.0.0.1:1/v1', OPENAI_API_KEY:'offline-test-only',
        ANTHROPIC_API_KEY:'offline-test-only', ANTHROPIC_BASE_URL:'http://127.0.0.1:1'
      } });
  }
  console.log('TANDEM: Ablauf und Wiederladen im echten VS-Code-Host bestanden.');
  console.log(`Testprojekt: ${workspace}`);
}
main().catch(error => { console.error(error); process.exitCode = 1; });
