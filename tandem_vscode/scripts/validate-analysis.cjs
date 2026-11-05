// Prüft einen gespeicherten Quellenstand über die aktuelle Binary; kein Modellaufruf.
const {spawn}=require('node:child_process');
const fs=require('node:fs');
const path=require('node:path');
const readline=require('node:readline');
const [workspace,report]=process.argv.slice(2);
if(!workspace||!report){console.error('node scripts/validate-analysis.cjs REPOSITORY ERGEBNIS.json');process.exit(2);}
const result=JSON.parse(fs.readFileSync(report,'utf8'));
const binary=process.env.TANDEM_BINARY||path.join(__dirname,'../bin',process.platform==='win32'?'tandem.exe':'tandem');
const child=spawn(binary,['--provider','demo','--workspace',path.resolve(workspace)],{windowsHide:true});
const timeout=setTimeout(()=>{child.kill();process.exitCode=1;},30_000);
const lines=readline.createInterface({input:child.stdout});
child.on('error',error=>{console.error(error.message);clearTimeout(timeout);process.exitCode=1;});
lines.on('line',line=>{
  const msg=JSON.parse(line);if(msg.id!==1)return;
  clearTimeout(timeout);
  const ids=new Set(result.sources.map(source=>source.id));
  const valid=msg.result?.current===true&&result.stale===false&&result.suggested_plan?.steps.length>0&&result.suggested_plan.steps.every(step=>step.source_ids.length>0&&step.source_ids.every(id=>ids.has(id)));
  if(!valid){console.error('Analyse fehlt, ist veraltet oder hat unbelegte Planschritte.');process.exitCode=1;}
  else console.log(`PASS: aktuelle Binary bestätigt ${result.sources.length} Quellen, ${result.instruction_paths.length} Projektregeln und ${result.suggested_plan.steps.length} belegte Planschritte ohne Modellaufruf.`);
  child.kill();lines.close();
});
child.stdin.write(JSON.stringify({jsonrpc:'2.0',id:1,method:'validate_exploration',params:{sources:result.sources,catalog_id:result.catalog_id,overlays:[]}})+'\n');
