// Lokaler HTTP-Doppelgänger: prüft den Azure-Transport ohne echte Zugangsdaten
// oder einen kostenpflichtigen Modellaufruf.
const assert = require('node:assert/strict');
const http = require('node:http');
const { spawn } = require('node:child_process');
const readline = require('node:readline');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

async function main() {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'tandem-azure-'));
  const requests = [];
  const server = http.createServer(async (req, res) => {
    let body = '';
    for await (const part of req) { body += part; }
    requests.push({ headers: req.headers, url: req.url, body: JSON.parse(body) });
    res.writeHead(200, { 'Content-Type': 'text/event-stream' });
    const delta = requests.length % 2 === 1 ? { tool_calls: [{ index: 0, id: 'azure-patch', type: 'function', function: {
      name: 'propose_patch', arguments: JSON.stringify({ path: 'Example.cs', old_text: 'return input;', new_text: 'return input.Trim();', reason: 'Minimaler Azure-Transporttest' })
    } }] } : { content: 'Der Vorschlag ist bereit, noch nicht übernommen.' };
    res.end(`data: ${JSON.stringify({ choices: [{ delta }] })}\n\ndata: [DONE]\n\n`);
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  fs.writeFileSync(path.join(home, 'config.json'), JSON.stringify({ provider: 'azure', azure: {
    endpoint: `http://127.0.0.1:${server.address().port}`, api_key: 'offline-test-key', deployment: 'offline-deployment', api_version: '2024-10-21'
  } }));
  const binary = path.join(__dirname, '../bin', process.platform === 'win32' ? 'tandem.exe' : 'tandem');
  const env = { ...process.env, AGENTKIT_HOME: home };
  // Die Testkonfiguration darf keine vom Aufrufer geerbten echten Azure-Werte nutzen.
  for (const key of ['AZURE_OPENAI_ENDPOINT', 'AZURE_OPENAI_API_KEY', 'AZURE_OPENAI_DEPLOYMENT', 'AZURE_OPENAI_API_VERSION', 'AGENTKIT_PROVIDER']) { delete env[key]; }
  const child = spawn(binary, ['--provider', 'auto'], { env, windowsHide: true });
  child.stderr.on('data', data => process.stderr.write(data));
  const lines = readline.createInterface({ input: child.stdout });
  const pending = new Map();
  let sequence = 0;
  lines.on('line', line => {
    const msg = JSON.parse(line);
    if (msg.id === undefined) { return; }
    const item = pending.get(msg.id);
    if (!item) { return; }
    pending.delete(msg.id); clearTimeout(item.timer);
    if (msg.error) { item.reject(new Error(msg.error.message)); } else { item.resolve(msg.result); }
  });
  function request(method, params) {
    const id = ++sequence;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { pending.delete(id); reject(new Error('Protokoll-Zeitlimit')); }, 15_000);
      pending.set(id, { resolve, reject, timer });
      child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
    });
  }
  try {
    const ping = await request('ping', {});
    assert.equal(ping.provider, 'auto');
    const params = { plan: { revision: 3, goal: 'Trim', not_now: 'Keine neue Klasse', acceptance: 'Gezielter Test' },
      snapshot: { path: 'Example.cs', prefix: 'class Example { ', editable: 'string Clean(string input) { return input; }', suffix: ' }' }, mode: 'pair', prompt: 'Kleinsten Patch vorschlagen', history: [] };
    // Direkt aufeinanderfolgende Läufe prüfen auch den Abschluss des Worker-Threads.
    for (let i = 0; i < 3; i++) {
      const result = await request('run', params);
      assert.equal(result.patch.new_text, 'return input.Trim();');
      assert.equal(result.plan_revision, 3);
    }
    assert.equal(requests.length, 6);
    for (const request of requests) {
      assert.equal(request.headers['api-key'], 'offline-test-key');
      assert.equal(request.url, '/openai/deployments/offline-deployment/chat/completions?api-version=2024-10-21');
      assert.equal(request.body.stream, true);
      assert.deepEqual(request.body.tools.map(tool => tool.function.name).sort(), ['propose_patch', 'propose_plan', 'read_snapshot']);
    }
    console.log('PASS: globale Azure-Konfiguration → JSON-RPC → AgentKit → lokales Azure-SSE → geprüfter Patch; drei direkt folgende Läufe');
  } finally {
    child.kill(); lines.close(); server.closeAllConnections(); server.close();
    for (const item of pending.values()) { clearTimeout(item.timer); }
  }
}
main().catch(error => { console.error(error); process.exitCode = 1; });
