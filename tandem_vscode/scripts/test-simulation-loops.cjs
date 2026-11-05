const assert=require('node:assert/strict'),fs=require('node:fs'),path=require('node:path'),os=require('node:os'),{spawnSync}=require('node:child_process');
const {simulate}=require('../out/simulation');
const scenario=input=>({name:'Schleifenreferenz',version:1,engine_version:2,inputs:{input},stubs:{}});
const sources=[
`int F(int input) {
 int result = 0;
 while (input > 0) {
  if (input == 2) { input--; continue; }
  result += input;
  if (result > 12) { break; }
  input--;
 }
 return result;
}`,
`int F(int input) {
 int result = 0;
 for (int i = 0; i < input; i++) {
  if (i == 2) { continue; }
  result += i;
  if (result > 12) { break; }
 }
 return result;
}`,
`int F(int input) {
 int result = 0;
 do {
  result++;
  input -= 2;
  if (input == 1) { continue; }
 } while (input > 0);
 return result;
}`,
`int F(int input) {
 int result = 0;
 for (int i = 0; i < input; ++i) {
  for (int j = 0; j < 3; j++) {
   if (j == 1) { continue; }
   result += j;
  }
 }
 return result;
}`,
`int F(int input) {
 for (; input > 0; input--) {
  if (input == 3) { return input; }
 }
 return 0;
}`,
`int F(int input) {
 int result = input;
 for (;;) { result *= 2; result /= 2; result %= 5; break; }
 return result;
}`
];
const cases=[-1,0,1,3,7],traces=sources.flatMap(code=>cases.map(input=>simulate(code,0,scenario(input))));
assert.ok(traces.every(trace=>trace.outcome==='returned'),JSON.stringify(traces.filter(trace=>trace.outcome!=='returned')));
const expected=traces.flatMap(trace=>trace.steps.flatMap(step=>step.kind==='assign'&&step.reason.startsWith('result ')?['STATE|'+step.values.result]:step.kind==='condition'&&/^(Bedingung|Schleifenbedingung) ist /.test(step.reason)?['BRANCH|'+step.reason.endsWith('true')]:step.kind==='return'?['RETURN|'+step.result]:[]));
const instrument=(code,index)=>code.replace('int F(',`static int F${index}(`)
 .replace(/for\s*\(([^;]*);([^;]*);([^)]*)\)/g,(_,init,condition,update)=>`for (${init}; C(${condition.trim()||'true'}); ${update})`)
 .replace(/\b(while|if)\s*\(([^)]*)\)/g,'$1 (C($2))')
 .replace(/((?:int\s+)?result\s*(?:=|\+=|\*=|\/=|%=)\s*[^;]+;|result\+\+;)/g,'$1 Console.WriteLine("STATE|" + result);')
 .replace(/return\s+([^;]+);/g,'return R($1);');
const root=fs.mkdtempSync(path.join(os.tmpdir(),'tandem-loop-reference-'));
fs.writeFileSync(path.join(root,'Reference.csproj'),'<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><OutputType>Exe</OutputType><TargetFramework>net10.0</TargetFramework><ImplicitUsings>enable</ImplicitUsings></PropertyGroup></Project>');
fs.writeFileSync(path.join(root,'Program.cs'),`using System; class Program { static bool C(bool value) { Console.WriteLine("BRANCH|" + value.ToString().ToLowerInvariant()); return value; } static int R(int value) { Console.WriteLine("RETURN|" + value); return value; } ${sources.map(instrument).join('\n')} static void Main() { ${sources.flatMap((_,i)=>cases.map(input=>`F${i}(${input});`)).join('\n')} } }`);
const reference=spawnSync('dotnet',['run','--project',path.join(root,'Reference.csproj'),'--verbosity','quiet'],{encoding:'utf8',timeout:90000,windowsHide:true});
assert.equal(reference.status,0,reference.stdout+'\n'+reference.stderr);
assert.deepEqual(reference.stdout.trim().split(/\r?\n/).filter(line=>/^(STATE|BRANCH|RETURN)\|/.test(line)),expected,'Schleifenwerte, Bedingungen und Returns müssen der realen C#-Ausführung entsprechen');
for(const code of [
 'int F(int input) { while (true) {} return 1; }',
 'int F(int input) { for (;;) { continue; } return 1; }',
 'int F(int input) { break; return 1; }',
 'int F(int input) { for (int i = 0; i < 1; i++) {} return i; }',
 'int F(int input) { while (input > 0) { input += 2147483647; } return input; }',
 'int F(int input) { foreach (var x in Missing()) { input++; } return input; }',
]){const trace=simulate(code,0,scenario(1));assert.equal(trace.outcome,'unknown',code);assert.equal(trace.result,undefined);assert.ok(trace.steps.length<=500);assert.equal(trace.steps.at(-1).evidence,'unknown');}
const old=simulate(sources[0],0,{...scenario(1),engine_version:1});assert.equal(old.outcome,'unknown','Historische Interpreter-v1-Szenarien behalten ihre damalige Semantik');
const unreachable=simulate('int F(int input) {\n throw new Exception();\n return input;\n}',0,{...scenario(1),start_line:3});assert.equal(unreachable.outcome,'unknown');
console.log('PASS: 30 Schleifenszenarien stimmen bei lokalen Zuständen, Zweigen und Returns mit realem C# überein; continue/break, Verschachtelung, frühes Return, Budget, Scope und Unknown-Stopp geprüft. Referenz: '+root);
