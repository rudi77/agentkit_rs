// Manuelle Abnahme desselben JSON-RPC-Pfads wie in VS Code. Der Adapter liest
// das gewählte Repository; ausschließlich der Ausgabepfad wird geschrieben.
const { spawn } = require('node:child_process');
const readline = require('node:readline');
const path = require('node:path');
const fs = require('node:fs');
const args = process.argv.slice(2);
const option = name => { const index=args.indexOf(name); return index<0?undefined:args[index+1]; };
const workspace=option('--workspace');
const task=option('--task');
if(!workspace||!task) {
  console.error('node scripts/explore-repository.cjs --workspace ABSOLUTER_PFAD --task "Fehlerbeschreibung" [--provider auto] [--output DATEI.json]');
  process.exit(2);
}
const executable=process.platform==='win32'?'tandem.exe':'tandem';
const binary=process.env.TANDEM_BINARY||path.join(__dirname,'../bin',executable);
const child=spawn(binary,['--provider',option('--provider')||'auto','--workspace',path.resolve(workspace)],{windowsHide:true});
const lines=readline.createInterface({input:child.stdout});
const timeout=setTimeout(()=>{console.error('Zeitlimit der Repository-Abnahme erreicht.');child.kill();process.exitCode=1;},300_000);
child.stderr.on('data',data=>process.stderr.write(data));
child.on('error',error=>{console.error(error.message);clearTimeout(timeout);process.exitCode=1;});
let finished=false;
lines.on('line',line=>{
  const message=JSON.parse(line);
  if(message.method==='event') {
    const tool=message.params.event.data?.tool_call;
    if(tool) console.log('Lesewerkzeug:',tool.name,JSON.stringify(tool.args));
    return;
  }
  if(message.id!==1)return;
  finished=true;clearTimeout(timeout);
  if(message.error){console.error(message.error.message);process.exitCode=1;}
  else {
    const output=option('--output');
    if(output){fs.mkdirSync(path.dirname(path.resolve(output)),{recursive:true});fs.writeFileSync(output,JSON.stringify(message.result,null,2));}
    console.log(JSON.stringify({sources:message.result.sources.map(s=>({path:s.path,kind:s.kind,start_line:s.start_line,sha256:s.sha256})),instructions:message.result.instruction_paths,stale:message.result.stale,plan:message.result.suggested_plan,answer:message.result.answer},null,2));
  }
  child.kill();lines.close();
});
child.on('exit',()=>{clearTimeout(timeout);if(!finished){console.error('Adapter vor Abschluss beendet.');process.exitCode=1;}});
child.stdin.write(JSON.stringify({jsonrpc:'2.0',id:1,method:'explore',params:{prompt:task,plan:{revision:1,goal:task,not_now:'Vollständig lesend; keine Produktdateien ändern und keine Tests oder Dienste starten.',acceptance:'Belegte Implementierungen und vorhandene Tests plus gemeinsam bearbeitbarer Änderungsplan.'},history:[],overlays:[]}})+'\n');
