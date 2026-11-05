const assert=require('node:assert/strict');
const fs=require('node:fs');
const path=require('node:path');
const os=require('node:os');
const {spawnSync}=require('node:child_process');
const {simulate,firstDivergence}=require('../out/simulation');
const {addDecision,reshapePlan,acceptStep}=require('../out/collaboration');
const {bindContract,enforceContract}=require('../out/contract');
const {handoffArtifact,pilotMetrics,saveEvaluation}=require('../out/artifact');
const {validateArchitecture}=require('../out/architecture');
const scenario=(inputs,stubs={})=>({name:'Referenzfall',version:1,inputs,stubs});
const code=`class Rules {
  public static int Compute(int input, bool enabled) {
    var value = input + 2;
    if (enabled && value > 4) {
      value = Twice(value);
    } else {
      value = 0;
    }
    return value;
  }
  static int Twice(int input) { return input * 2; }
}`;
const cases=[{input:3,enabled:true},{input:1,enabled:true},{input:7,enabled:false},{input:-3,enabled:true}];
const traces=cases.map(inputs=>simulate(code,code.indexOf('Compute'),scenario(inputs)));
assert.deepEqual(traces.map(t=>t.result),[10,0,0,0]);
assert.ok(traces.every(t=>t.outcome==='returned'));
assert.ok(traces[0].steps.some(s=>s.stack.join('/')==='Compute/Twice'));
assert.ok(traces[0].steps.find(s=>s.kind==='condition').origins.value.includes('Szenario'));
assert.deepEqual(simulate(code,code.indexOf('Compute'),scenario(cases[0])),traces[0],'Trace muss reproduzierbar sein');
const changed=simulate(code.replace('input + 2','input + 3'),code.indexOf('Compute'),scenario(cases[0]));
assert.equal(changed.result,12);assert.ok(firstDivergence(traces[0],changed)>=0);
const unknown=simulate('int F(int input) { var x = input + 1; return Port.Get(x); }',0,scenario({input:2}));
assert.equal(unknown.outcome,'unknown');assert.equal(unknown.steps.at(-1).evidence,'unknown');assert.equal(unknown.steps.at(-1).values.x,3);
const stubbed=simulate('int F(int input) { return Port.Get(input); }',0,scenario({input:2},{'Port.Get(2)':{value:8,reason:'Expliziter Testvertrag'}}));
assert.equal(stubbed.result,8);assert.ok(stubbed.steps.some(s=>s.evidence==='assumption'&&s.reason.includes('Expliziter')));
assert.equal(simulate('int F(int input) { return Port.Get(input); }',0,scenario({input:3},{'Port.Get(2)':{value:8,reason:'Nur dieser Fall'}})).outcome,'unknown');
for(const source of [
  'int F(int input) { while(input > 0) { input = input - 1; } return input; }',
  'int F(int input) { return Process.Start("calc"); }',
  'int F(int input) { return input / 0; }',
  'int F(int input) { return input + 2147483647; }',
  'double F(int input) { return input / 2; }',
  'int F(int input) { return new Evil(); }',
  'int F(int input) { throw new Exception(Port.Get()); }',
  'int F(int input) { { var local = 1; } return local; }',
  'int F(int input) { int value = "bad"; return value; }',
  'int F(int input) { return F(input); }'
])assert.equal(simulate(source,0,scenario({input:2})).outcome,'unknown',source);
assert.equal(simulate('int F(int input) { return input; }',0,scenario({input:'wrong'})).outcome,'unknown');
assert.equal(simulate('int F(int input) { if (-input > 0) return 1; return 0; }',0,scenario({input:-2147483648})).outcome,'unknown','Nicht modellierte checked/unchecked-Unterschiede dürfen keinen erfundenen Zweig wählen');
assert.equal(simulate('object F() { }',0,scenario({})).outcome,'unknown','Fehlendes Return ist kein belegter null-Wert');
assert.equal(simulate('int F(bool enabled) { if (enabled) throw new InvalidOperationException(); return 1; }',0,scenario({enabled:true})).outcome,'thrown');
assert.equal(simulate('int F(bool enabled) { if (enabled) return 1; return Port.Get(); }',0,scenario({enabled:true})).result,1);
assert.equal(simulate('bool F(bool enabled) { return enabled || Port.Unknown(); }',0,scenario({enabled:true})).result,true);
assert.throws(()=>simulate(code,0,scenario({}, {'Port.Get':{value:null,reason:''}})),/Begründung/);
assert.throws(()=>simulate(code,0,scenario({input:Infinity})),/endliche/);
console.log('PASS: deterministische C#-Simulation, lokale Aufrufe, Zustände/Herkunft, explizite Stubs, Divergenz und Unknown bei nicht unterstützter Semantik.');

const step={id:'one',title:'Wert ändern',why:'Heute benötigt',files:['Flow.cs'],source_ids:['s1'],acceptance:'Gezielter Test'};
const plan={goal:'Ziel',not_now:'Keine neue Klasse',open_questions:'',steps:[step,{...step,id:'two',title:'Zweiter Schritt'}],revision:1,stale:false,root_uri:'file:///repo',catalog_id:'c',sources:[]};
const state={plan:{revision:2,goal:'Wert ändern',not_now:'',acceptance:'Test'},changePlan:plan,activePlanStepId:'one',chat:[],audit:[],proposal:{status:'ready'},evidence:{status:'passed'}};
addDecision(state,{kind:'exclude',text:'Kein neuer Service',reason:'Bestehender Port reicht'});
assert.equal(state.changePlan.steps[0].details.not_now,'Kein neuer Service');assert.equal(state.proposal.status,'stale');assert.equal(state.evidence.status,'stale');
assert.equal(state.decisions[0].by,'human');assert.equal(state.planHistory[0].plan.steps[0].details,undefined);
const split=reshapePlan(plan,'one','split');assert.equal(split.steps.length,3);assert.notEqual(split.steps[0].id,split.steps[1].id);
const merge=reshapePlan(plan,'one','merge');assert.equal(merge.steps.length,1);assert.match(merge.steps[0].title,/Zweiter/);
assert.equal(reshapePlan(plan,'one','defer').steps[0].status,'deferred');
assert.throws(()=>acceptStep(state,'behavior','Passt'),/bestandene/);
acceptStep(state,'clarification','Es bleibt beim bisherigen Vertrag');assert.equal(state.changePlan.steps[0].status,'accepted');assert.equal(state.acceptances[0].kind,'clarification');
addDecision(state,{kind:'risk',text:'Neuer Sachverhalt',reason:'Vertrag prüfen'});assert.equal(state.changePlan.steps[0].status,'clarifying');
console.log('PASS: Entscheidungen invalidieren alte Rechte/Belege, Planhistorie bleibt erhalten, Teilen/Zusammenführen/Zurückstellen und getrennte fachliche Abnahme.');
const contractState={plan:{revision:1,goal:'Wert ändern',not_now:'Keine neue Klasse',acceptance:'Test'},chat:[],audit:[],scope:{path:'Flow.cs',prefix:'',editable:'public class Flow { public int Run(int input) { return input; } }',suffix:'',uri:'file:///Flow.cs',version:1}};
bindContract(contractState);
const patch=(old_text,new_text)=>({path:'Flow.cs',old_text,new_text,reason:'Testvorschlag'});
assert.doesNotThrow(()=>enforceContract(contractState,patch('return input;','return input + 1;')));
assert.throws(()=>enforceContract(contractState,patch('public int Run','public long Run')),/Signatur/);
assert.throws(()=>enforceContract(contractState,patch('return input;','return input; } private int Extra() { return 1;')),/Symbol/);
assert.throws(()=>enforceContract(contractState,patch('return input;','return input; } class Extra {')),/Symbol/);
bindContract(contractState,{mode:'explore'});assert.throws(()=>enforceContract(contractState,patch('return input;','return input+1;')),/Explore/);
console.log('PASS: revisionsgebundener Contract, geschützte öffentliche Signaturen, keine ungefragten Typen/Methoden und Explore ohne Patches.');
saveEvaluation(contractState,{cognitive_load:3,comprehension:6,interruption_cost:2,explanation:'Die bestehende Methode erhält den kleinsten Eingriff.',notes:'Keine unnötige Klasse'});
assert.equal(pilotMetrics(contractState).evaluations.length,1);assert.throws(()=>saveEvaluation(contractState,{cognitive_load:0,comprehension:8,interruption_cost:2,explanation:'',notes:''}));
const metadata=handoffArtifact(contractState,false),complete=handoffArtifact(contractState,true);
assert.equal(metadata.execution_authorization,'none');assert.equal(metadata.requires_revalidation,true);assert.equal(metadata.state.scope.editable,undefined);assert.equal(complete.state.scope.editable,contractState.scope.editable);
assert.doesNotThrow(()=>validateArchitecture({nodes:[{id:'a',label:'Lesepfad',path:'Flow.cs',line:1}],edges:[{from:'a',to:'a',kind:'hypothesis',reason:'Noch zu prüfen'}]}));
assert.throws(()=>validateArchitecture({nodes:[{id:'a',label:'Gefahr',path:'../secret',line:1}],edges:[]}));
assert.throws(()=>validateArchitecture({nodes:[],edges:[{from:'a',to:'b',kind:'documented',reason:'Fehlende Knoten'}]}));
console.log('PASS: lokale Pilotevaluation, nicht ausführbarer versionierter Übergabeexport und explizite Architektur-Provenienz ohne erfundene Codeanker.');

// Echte C#-Referenz, ohne Produktcode oder Netzwerk. NuGet-Quellen sind leer;
// nur das lokal installierte .NET-SDK wird verwendet.
const root=fs.mkdtempSync(path.join(os.tmpdir(),'tandem-simulation-reference-'));
fs.writeFileSync(path.join(root,'NuGet.Config'),'<configuration><packageSources><clear /></packageSources></configuration>');
fs.writeFileSync(path.join(root,'Reference.csproj'),'<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><OutputType>Exe</OutputType><TargetFramework>net10.0</TargetFramework></PropertyGroup></Project>');
const instrumented=code.replace('var value = input + 2;','var value = input + 2; System.Console.WriteLine("STATE|" + value);').replace('if (enabled && value > 4)','System.Console.WriteLine("BRANCH|" + (enabled && value > 4).ToString().ToLowerInvariant()); if (enabled && value > 4)').replace('value = Twice(value);','value = Twice(value); System.Console.WriteLine("STATE|" + value);').replace('value = 0;','value = 0; System.Console.WriteLine("STATE|" + value);').replace('return value;','System.Console.WriteLine("RETURN|" + value); return value;').replace('return input * 2;','System.Console.WriteLine("CALL|Twice"); return input * 2;');
fs.writeFileSync(path.join(root,'Program.cs'),instrumented+'\nclass Program { static void Main() { '+cases.map(c=>`System.Console.WriteLine(Rules.Compute(${c.input},${c.enabled}));`).join(' ')+' } }');
const reference=spawnSync('dotnet',['run','--project',path.join(root,'Reference.csproj'),'--verbosity','quiet'],{encoding:'utf8',timeout:120000,windowsHide:true,cwd:root});
assert.equal(reference.status,0,reference.stdout+'\n'+reference.stderr);
assert.deepEqual(reference.stdout.trim().split(/\r?\n/).filter(line=>/^-?\d+$/.test(line)).map(Number),traces.map(t=>t.result));
const expected=traces.flatMap(trace=>trace.steps.flatMap(step=>step.kind==='assign'&&Object.hasOwn(step.values,'value')?['STATE|'+step.values.value]:step.kind==='condition'?['BRANCH|'+step.reason.endsWith('true')]:step.kind==='enter'&&step.stack.at(-1)==='Twice'?['CALL|Twice']:step.kind==='return'&&step.stack.length===1?['RETURN|'+step.result]:[]));
assert.deepEqual(reference.stdout.trim().split(/\r?\n/).filter(line=>/^(STATE|BRANCH|CALL|RETURN)\|/.test(line)),expected);
console.log('PASS: Werte, Zweige, Aufrufreihenfolge und Ergebnisse von vier Szenarien stimmen mit ausgeführter .NET-Referenz überein. Referenzprojekt: '+root);
