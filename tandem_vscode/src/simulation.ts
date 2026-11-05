// Dieser Interpreter führt ausschließlich sein eigenes Syntaxmodell aus.
// Produktcode, eval, Netzwerk und Dateizugriffe sind hier nicht verfügbar.
export type SimValue = null | boolean | number | string | SimValue[] | {[key:string]:SimValue};
export type EvidenceClass = 'derived' | 'assumption' | 'hypothesis' | 'unknown';
export interface Scenario { name:string; version:number; inputs:Record<string,SimValue>; stubs:Record<string,{value:SimValue;reason:string}>; start_line?:number; engine_version?:1|2 }
export interface SimStep { line:number; text:string; values:Record<string,SimValue>; origins:Record<string,string>; stack:string[]; evidence:EvidenceClass; reason:string; kind:'enter'|'assign'|'condition'|'return'|'throw'|'unknown'|'call'; result?:SimValue }
export interface SimTrace { method:string; scenario:Scenario; steps:SimStep[]; outcome:'returned'|'thrown'|'unknown'|'completed'; result?:SimValue }
interface Token { text:string; line:number; start:number; end:number }
type Expr = {kind:'literal';value:SimValue} | {kind:'name';name:string} | {kind:'member';object:Expr;name:string} | {kind:'call';target:Expr;args:Expr[]} | {kind:'exception';name:string;args:Expr[]} | {kind:'unary';op:string;value:Expr} | {kind:'binary';op:string;left:Expr;right:Expr};
type Statement = {kind:'block';body:Statement[];token:Token} | {kind:'if';condition:Expr;yes:Statement;no?:Statement;token:Token} | {kind:'while'|'for'|'do';condition:Expr;body:Statement;init?:Statement;update?:Statement;token:Token} | {kind:'jump';flow:'break'|'continue';token:Token} | {kind:'assign';name:string;value:Expr;declared:boolean;declaredType:string;token:Token} | {kind:'return'|'throw'|'expression';value:Expr;token:Token} | {kind:'unknown';reason:string;token:Token};
interface Method { name:string; parameters:string[]; parameterTypes:string[]; returnType:string; body:Token[]; start:number; end:number; line:number; unsupported?:string }
class Unknown extends Error {}

function tokenize(code:string):Token[] {
  const out:Token[]=[];let at=0,line=1;
  while(at<code.length) {
    const rest=code.slice(at), whitespace=rest.match(/^\s+/);
    if(whitespace){line+=(whitespace[0].match(/\n/g)||[]).length;at+=whitespace[0].length;continue;}
    if(rest.startsWith('//')){const end=rest.indexOf('\n');at+=end<0?rest.length:end;continue;}
    if(rest.startsWith('/*')){const end=rest.indexOf('*/',2);if(end<0)throw new Unknown('Nicht abgeschlossener Kommentar');const text=rest.slice(0,end+2);line+=(text.match(/\n/g)||[]).length;at+=text.length;continue;}
    const match=rest.match(/^(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|\d+(?:\.\d+)?|[A-Za-z_][A-Za-z_0-9]*|==|!=|<=|>=|&&|\|\||\?\?|=>|\+\+|--|\+=|-=|\*=|\/=|%=|[^\s])/);
    if(!match)throw new Unknown('Nicht lesbare Syntax');
    out.push({text:match[0],line,start:at,end:at+match[0].length});at+=match[0].length;
  }
  return out;
}
function closing(tokens:Token[],at:number,left:string,right:string):number {
  let depth=0;for(let i=at;i<tokens.length;i++){if(tokens[i].text===left)depth++;if(tokens[i].text===right){depth--;if(depth===0)return i;}}
  throw new Unknown(`Nicht abgeschlossene ${left}-Gruppe`);
}

function methods(tokens:Token[]):Method[] {
  const result:Method[]=[];
  for(let i=1;i<tokens.length;i++) {
    if(tokens[i].text!=='('||!/^[A-Za-z_]\w*$/.test(tokens[i-1].text)||['if','while','for','switch','catch','using','lock'].includes(tokens[i-1].text))continue;
    const end=closing(tokens,i,'(',')');if(tokens[end+1]?.text!=='{')continue;
    const bodyEnd=closing(tokens,end+1,'{','}');const parameters:string[]=[],parameterTypes:string[]=[];let part:Token[]=[],unsupported:string|undefined;
    let header=i-2;while(header>0&&![';','{','}'].includes(tokens[header-1].text))header--;
    const signature=tokens.slice(header,end).map(t=>t.text);
    const returnType=tokens[i-2]?.text==='>'&&tokens[i-4]?.text==='<'&&tokens[i-5]?.text==='Task'?tokens[i-3].text:tokens[i-2]?.text||'unknown';
    if(signature.some(s=>['double','float','decimal','long','uint','ulong','short','byte','ref','out','params'].includes(s)))unsupported='Dieses Zahlen-/Parameter-Typsystem ist noch nicht unterstützt.';
    for(let j=i+1;j<=end;j++) {
      if(tokens[j].text===','||j===end){if(part.length){if(part.some(t=>['ref','out','params','='].includes(t.text)))unsupported='ref/out/params und optionale Parameter werden noch nicht simuliert.';parameters.push(part.at(-1)!.text);parameterTypes.push(part.slice(0,-1).map(t=>t.text).join(''));}part=[];}
      else part.push(tokens[j]);
    }
    result.push({name:tokens[i-1].text,parameters,parameterTypes,returnType,body:tokens.slice(end+1,bodyEnd+1),start:tokens[header].start,end:tokens[bodyEnd].end,line:tokens[i-1].line,unsupported});i=bodyEnd;
  }
  return result;
}

const precedence:Record<string,number>={'??':1,'||':2,'&&':3,'==':4,'!=':4,'<':5,'>':5,'<=':5,'>=':5,'+':6,'-':6,'*':7,'/':7,'%':7};
class Parser {
  at=0;
  constructor(readonly tokens:Token[],private readonly extended=false){}
  peek():string{return this.tokens[this.at]?.text??'';}
  take(text?:string):Token{const t=this.tokens[this.at++];if(!t||(text&&t.text!==text))throw new Unknown(`Erwartet: ${text||'Ausdruck'}, gefunden: ${t?.text||'Ende'}`);return t;}
  expression(min=0):Expr {
    let expr:Expr;const token=this.take(),s=token.text;
    if(s==='await') {expr=this.expression(8);}
    else if(['!','-','+'].includes(s))expr={kind:'unary',op:s,value:this.expression(8)};
    else if(s==='('){expr=this.expression();this.take(')');}
    else if(s==='true'||s==='false'||s==='null')expr={kind:'literal',value:s==='null'?null:s==='true'};
    else if(/^\d/.test(s)){if(s.includes('.')||!Number.isSafeInteger(Number(s))||Number(s)>2147483647)throw new Unknown('Nur int-Literale im unterstützten Wertebereich sind ableitbar.');expr={kind:'literal',value:Number(s)};}
    else if(s.startsWith('"')){try{expr={kind:'literal',value:JSON.parse(s)};}catch{throw new Unknown('Nicht unterstütztes Stringliteral');}}
    else if(s.startsWith("'")){if(s.length!==3)throw new Unknown('Nicht unterstütztes Zeichenliteral');expr={kind:'literal',value:s[1]};}
    else if(s==='new')throw new Unknown('Objektkonstruktoren benötigen einen dokumentierten Stub; es wird kein Produktkonstruktor ausgeführt.');
    else if(/^[A-Za-z_]\w*$/.test(s))expr={kind:'name',name:s};
    else throw new Unknown('Nicht unterstützter Ausdruck: '+s);
    while(this.at<this.tokens.length) {
      if(this.peek()==='.') {this.take();expr={kind:'member',object:expr,name:this.take().text};continue;}
      if(this.peek()==='('){this.take();const args:Expr[]=[];while(this.peek()!==')'){args.push(this.expression());if(this.peek()!==',')break;this.take();}this.take(')');expr={kind:'call',target:expr,args};continue;}
      const op=this.peek(),power=precedence[op];if(power===undefined||power<min)break;this.take();const right=this.expression(op==='??'?power:power+1);expr={kind:'binary',op,left:expr,right};
    }
    return expr;
  }
  statement():Statement {
    const token=this.tokens[this.at];if(!token)throw new Unknown('Anweisung fehlt');
    if(this.peek()==='{'){this.take();const body:Statement[]=[];while(this.peek()&&this.peek()!=='}')body.push(this.safeStatement());this.take('}');return{kind:'block',body,token};}
    if(this.peek()==='if'){this.take();this.take('(');const condition=this.expression();this.take(')');const yes=this.safeStatement();let no:Statement|undefined;if(this.peek()==='else'){this.take();no=this.safeStatement();}return{kind:'if',condition,yes,no,token};}
    if(!this.extended&&['while','for','do','break','continue'].includes(this.peek()))throw new Unknown('Nicht unterstützter Kontrollfluss in Interpreter v1: '+this.peek());
    if(this.peek()==='while'){this.take();this.take('(');const condition=this.expression();this.take(')');return{kind:'while',condition,body:this.safeStatement(),token};}
    if(this.peek()==='do'){this.take();const body=this.safeStatement();this.take('while');this.take('(');const condition=this.expression();this.take(')');this.take(';');return{kind:'do',condition,body,token};}
    if(this.peek()==='for'){
      this.take();this.take('(');let init:Statement|undefined,update:Statement|undefined;
      if(this.peek()===';')this.take();else init=this.simpleStatement(';');
      const condition:Expr=this.peek()===';'?{kind:'literal',value:true}:this.expression();this.take(';');
      if(this.peek()===')')this.take();else update=this.simpleStatement(')');
      return{kind:'for',init,condition,update,body:this.safeStatement(),token};
    }
    if(this.peek()==='break'||this.peek()==='continue'){const flow=this.take().text as 'break'|'continue';this.take(';');return{kind:'jump',flow,token};}
    if(this.peek()==='return'||this.peek()==='throw'){const kind=this.take().text as 'return'|'throw';if(this.peek()===';'){this.take();return{kind,value:{kind:'literal',value:null},token};}
      if(kind==='throw'&&this.peek()==='new'){this.take();const name=this.take().text;this.take('(');const args:Expr[]=[];while(this.peek()!==')'){args.push(this.expression());if(this.peek()!==',')break;this.take();}this.take(')');this.take(';');return{kind,value:{kind:'exception',name,args},token};}
      const value=this.expression();this.take(';');return{kind,value,token};}
    if(['foreach','switch','try','lock','using','yield','goto'].includes(this.peek()))throw new Unknown('Nicht unterstützter Kontrollfluss: '+this.peek());
    return this.simpleStatement(';');
  }
  private simpleStatement(end:string):Statement {
    const token=this.tokens[this.at];
    const prefix=['++','--'].includes(this.peek());
    if(prefix||['++','--','+=','-=','*=','/=','%='].includes(this.tokens[this.at+1]?.text)){
      if(!this.extended)throw new Unknown('Diese Zähleränderung benötigt Interpreter v2.');
      const operator=prefix?this.take().text:undefined,name=this.take().text,op=operator??this.take().text;
      if(!/^[A-Za-z_]\w*$/.test(name))throw new Unknown('Nur bekannte lokale Werte können hier verändert werden.');
      const value:Expr={kind:'binary',op:op[0],left:{kind:'name',name},right:op==='++'||op==='--'?{kind:'literal',value:1}:this.expression()};
      this.take(end);return{kind:'assign',name,value,declared:false,declaredType:'',token};
    }
    if(this.tokens[this.at+1]?.text==='='||this.tokens[this.at+2]?.text==='=') {
      let name=this.take().text,declared=false,declaredType='';if(this.peek()!=='='){if(!['var','int','bool','string'].includes(name))throw new Unknown('Nicht unterstützte lokale Deklaration: '+name);declaredType=name;name=this.take().text;declared=true;}this.take('=');const value=this.expression();this.take(end);return{kind:'assign',name,value,declared,declaredType,token};
    }
    const value=this.expression();this.take(end);if(this.extended&&value.kind!=='call')throw new Unknown('Nur Methodenaufrufe sind hier als Ausdrucksanweisung unterstützt.');return{kind:'expression',value,token};
  }
  safeStatement():Statement {
    const start=this.at,token=this.tokens[start];try{return this.statement();}catch(error){
      // Unbekannte Syntax bleibt ein Stop-Knoten. Auch Code nach ihr wird nicht
      // als berechenbar ausgegeben; ein nicht genommener Zweig darf sie enthalten.
      this.at=start;let depth=0;
      while(this.at<this.tokens.length){const s=this.peek();if(s==='}'&&depth===0)break;this.at++;if(s==='{')depth++;if(s==='}')depth--;if(s===';'&&depth===0)break;}
      if(this.at===start)this.at++;
      return{kind:'unknown',reason:error instanceof Error?error.message:String(error),token};
    }
  }
}

function nameOf(expr:Expr):string|undefined {return expr.kind==='name'?expr.name:expr.kind==='member'&&nameOf(expr.object)?nameOf(expr.object)+'.'+expr.name:undefined;}
function copy<T>(value:T):T{return structuredClone(value);}
function checkedNumber(value:SimValue):number {if(typeof value!=='number'||!Number.isSafeInteger(value))throw new Unknown('Nur sichere ganze Zahlen werden arithmetisch simuliert.');return value;}
function checkType(type:string,value:SimValue):void {
  if(type==='var')return;
  if(type==='int'&&typeof value==='number'&&Number.isInteger(value)&&value>=-2147483648&&value<=2147483647)return;
  if(type==='bool'&&typeof value==='boolean')return;
  if((type==='string'||type==='string?')&&(typeof value==='string'||value===null))return;
  if(type==='void'&&value===null)return;
  if(!['int','bool','string','string?','void'].includes(type)&&(value===null||typeof value==='object'))return;
  throw new Unknown(`Wert passt nicht zum unterstützten Typ ${type}.`);
}
function bool(value:SimValue):boolean {if(typeof value!=='boolean')throw new Unknown('Die Bedingung hat keinen bekannten booleschen Wert.');return value;}
function equal(left:SimValue,right:SimValue):boolean {if(left===null||right===null){const other=left===null?right:left;if(typeof other==='number'||typeof other==='boolean')throw new Unknown('Nicht unterstützter Nullvergleich mit einem Werttyp.');return left===right;}if(typeof left!==typeof right)throw new Unknown('Die Gleichheitsoperanden haben unterschiedliche Typen.');if(typeof left==='object'||typeof right==='object')throw new Unknown('Objektidentität wird nicht aus JSON-Gleichheit abgeleitet.');return left===right;}

export function validateScenario(input:unknown):Scenario {
  const s=input as Scenario;
  if(!s||typeof s.name!=='string'||!s.name.trim()||!Number.isSafeInteger(s.version)||s.version<1||!s.inputs||typeof s.inputs!=='object'||Array.isArray(s.inputs)||!s.stubs||typeof s.stubs!=='object'||Array.isArray(s.stubs))throw new Error('Ein Szenario benötigt Name, Version, Eingaben und dokumentierte Stubs.');
  if(JSON.stringify(s).length>64_000)throw new Error('Das Szenario darf höchstens 64 KB groß sein.');
  for(const stub of Object.values(s.stubs)){if(!stub||!Object.hasOwn(stub,'value')||typeof stub.reason!=='string'||!stub.reason.trim())throw new Error('Jeder Stub benötigt Wert und Begründung.');}
  const jsonValue=(value:unknown):boolean=>value===null||typeof value==='string'||typeof value==='boolean'||typeof value==='number'&&Number.isFinite(value)||Array.isArray(value)&&value.every(jsonValue)||typeof value==='object'&&value!==null&&Object.values(value).every(jsonValue);
  if(!jsonValue(s.inputs)||!Object.values(s.stubs).every(stub=>jsonValue(stub.value)))throw new Error('Szenariowerte müssen endliche JSON-Werte sein.');
  if(s.start_line!==undefined&&(!Number.isSafeInteger(s.start_line)||s.start_line<1))throw new Error('Die Startzeile muss positiv sein.');
  if(s.engine_version!==undefined&&![1,2].includes(s.engine_version))throw new Error('Unbekannte Interpreter-Version.');
  return copy(s);
}

export function simulate(code:string,offset:number,scenarioInput:Scenario):SimTrace {
  const scenario=validateScenario(scenarioInput),steps:SimStep[]=[];
  const sourceLines=code.split('\n');
  const trace:SimTrace={method:'Unbekannt',scenario,steps,outcome:'unknown'};
  let all:Method[]=[],stack:string[]=[],returnTypes:string[]=[],values:Record<string,SimValue>=Object.create(null),origins:Record<string,string>=Object.create(null),types:Record<string,string>=Object.create(null);
  let current:Token={text:'',line:1,start:0,end:0},assumed=false;
  const add=(kind:SimStep['kind'],reason:string,evidence:EvidenceClass=assumed?'assumption':'derived',result?:SimValue)=>{
    if(steps.length>=500)throw new Unknown('Das Schrittlmit von 500 wurde erreicht.');
    steps.push({line:current.line,text:sourceLines[current.line-1]?.trim()||'',values:copy(values),origins:copy(origins),stack:[...stack],kind,reason,evidence,...(result!==undefined?{result:copy(result)}:{})});
  };
  const evaluate=(expr:Expr):SimValue=>{
    if(expr.kind==='literal')return expr.value;
    if(expr.kind==='exception'){expr.args.forEach(evaluate);return expr.name;}
    if(expr.kind==='name'){if(!Object.hasOwn(values,expr.name))throw new Unknown('Szenariowert fehlt: '+expr.name);if(origins[expr.name]?.startsWith('Szenario')||origins[expr.name]?.startsWith('Stub'))assumed=true;return values[expr.name];}
    if(expr.kind==='member'){
      const value=evaluate(expr.object);
      if(expr.name==='Length'&&(typeof value==='string'||Array.isArray(value)))return value.length;
      if(value===null)throw new Unknown('Nullzugriff; der konkrete Fehlerpfad ist nicht modelliert.');
      if(typeof value!=='object'||!Object.hasOwn(value,expr.name))throw new Unknown('Objektwert fehlt: '+(nameOf(expr)||expr.name));
      return (value as {[key:string]:SimValue})[expr.name];
    }
    if(expr.kind==='unary'){const value=evaluate(expr.value);if(expr.op==='!')return !bool(value);const result=expr.op==='-'?-checkedNumber(value):checkedNumber(value);if(result<-2147483648||result>2147483647)throw new Unknown('Numerischer Überlauf bei unärem Operator; checked/unchecked-Semantik ist nicht modelliert.');return result;}
    if(expr.kind==='binary'){
      const l=evaluate(expr.left);if(expr.op==='&&')return bool(l)&&bool(evaluate(expr.right));if(expr.op==='||')return bool(l)||bool(evaluate(expr.right));if(expr.op==='??')return l===null?evaluate(expr.right):l;
      const r=evaluate(expr.right);if(expr.op==='==')return equal(l,r);if(expr.op==='!=')return !equal(l,r);
      if(expr.op==='+'&&typeof l==='string'&&typeof r==='string')return l+r;
      const a=checkedNumber(l),b=checkedNumber(r);if(expr.op==='<')return a<b;if(expr.op==='>')return a>b;if(expr.op==='<=')return a<=b;if(expr.op==='>=')return a>=b;
      if((expr.op==='/'||expr.op==='%')&&b===0)throw new Unknown('Division durch Null: Fehlerpfad benötigt einen expliziten Test.');
      const result=expr.op==='+'?a+b:expr.op==='-'?a-b:expr.op==='*'?a*b:expr.op==='/'?Math.trunc(a/b):a%b;
      if(!Number.isSafeInteger(result)||result<-2147483648||result>2147483647)throw new Unknown('Numerischer Überlauf oder nicht unterstützter Zahlbereich.');return result;
    }
    const name=nameOf(expr.target);if(!name)throw new Unknown('Dynamisches Aufrufziel ist unbekannt.');
    // Stubs sind explizite Szenarioannahmen und dürfen fehlende Argumentwerte
    // nicht verdecken. Schlüssel können einen konkreten Argumentfall binden.
    const args=expr.args.map(evaluate),key=name+'('+JSON.stringify(args).slice(1,-1)+')';
    const stub=Object.hasOwn(scenario.stubs,key)?scenario.stubs[key]:Object.hasOwn(scenario.stubs,name)?scenario.stubs[name]:undefined;
    if(stub){assumed=true;add('call',`Szenario-Stub ${key}: ${stub.reason}`,'assumption',stub.value);return copy(stub.value);}
    const matches=all.filter(method=>method.name===name&&method.parameters.length===args.length);
    if(matches.length!==1)throw new Unknown(`Aufruf ${key} ist nicht eindeutig verfügbar. Bitte Implementierung oder dokumentierten Stub ergänzen.`);
    return invoke(matches[0],args);
  };
  type Flow={kind:'return'|'throw'|'break'|'continue';value:SimValue}|undefined;
  const execute=(statement:Statement):Flow=>{
    current=statement.token;assumed=false;
    if(statement.kind==='unknown')throw new Unknown(statement.reason);
    if(statement.kind==='block'){const before=new Set(Object.keys(values));let flow:Flow;for(const child of statement.body){flow=execute(child);if(flow)break;}for(const name of Object.keys(values))if(!before.has(name)){delete values[name];delete origins[name];delete types[name];}return flow;}
    if(statement.kind==='jump'){add('condition',statement.flow==='break'?'Schleife wird verlassen.':'Nächster Schleifendurchlauf.');return{kind:statement.flow,value:null};}
    if('condition' in statement&&'body' in statement){
      const before=new Set(Object.keys(values));
      if(statement.init)execute(statement.init);
      let first=true;
      while(true){
        current=statement.token;assumed=false;
        if(statement.kind!=='do'||!first){const condition=bool(evaluate(statement.condition));add('condition','Schleifenbedingung ist '+condition);if(!condition)break;}
        first=false;const flow=execute(statement.body);
        if(flow?.kind==='return'||flow?.kind==='throw')return flow;
        if(flow?.kind==='break')break;
        if(statement.update)execute(statement.update);
      }
      for(const name of Object.keys(values))if(!before.has(name)){delete values[name];delete origins[name];delete types[name];}
      return;
    }
    if(statement.kind==='if'){const value=evaluate(statement.condition);add('condition','Bedingung ist '+String(bool(value)));return value?execute(statement.yes):statement.no?execute(statement.no):undefined;}
    const value=evaluate(statement.value);
    current=statement.token;
    if(statement.kind==='assign'){if(!statement.declared&&!Object.hasOwn(values,statement.name))throw new Unknown('Zuweisungsziel ist kein bekannter lokaler Wert: '+statement.name);if(statement.declared&&Object.hasOwn(values,statement.name))throw new Unknown('Mehrdeutige lokale Deklaration: '+statement.name);if(statement.declared)types[statement.name]=statement.declaredType==='var'?typeof value==='number'?'int':typeof value==='boolean'?'bool':typeof value==='string'?'string':'object':statement.declaredType;checkType(types[statement.name],value);values[statement.name]=copy(value);origins[statement.name]=assumed?'Szenarioabhängige Ableitung, Zeile '+current.line:'Aus Code, Zeile '+current.line;add('assign',`${statement.name} wird gesetzt.`);return;}
    if(statement.kind==='expression'){add('call','Aufruf ausgewertet; keine realen Seiteneffekte.');return;}
    if(statement.kind==='return')checkType(returnTypes.at(-1)!,value);
    add(statement.kind,statement.kind==='return'?'Rückgabewert bestimmt.':'Exception modelliert.',undefined,value);return{kind:statement.kind,value};
  };
  const invoke=(method:Method,args:SimValue[]):SimValue=>{
    if(stack.length>=16)throw new Unknown('Maximale Aufruftiefe 16 erreicht.');
    if(method.unsupported)throw new Unknown(method.unsupported);
    const callerValues=values,callerOrigins=origins,callerToken=current,callerTypes=types;
    values=Object.create(null);origins=Object.create(null);types=Object.create(null);
    method.parameters.forEach((name,i)=>{checkType(method.parameterTypes[i],args[i]);values[name]=copy(args[i]);types[name]=method.parameterTypes[i];origins[name]=stack.length?'Szenarioargument aus Aufruf':'Szenarioeingabe';});
    stack.push(method.name);returnTypes.push(method.returnType);current={text:method.name,line:method.line,start:method.start,end:method.start};add('enter','Methode betreten.','assumption');
    const parser=new Parser(method.body,scenario.engine_version===2),body=parser.safeStatement();const flow=execute(body);
    if(flow?.kind==='break'||flow?.kind==='continue')throw new Unknown('break/continue außerhalb einer unterstützten Schleife.');
    if(flow?.kind==='throw'){trace.outcome='thrown';trace.result=flow.value;throw new ModeledThrow(flow.value);}
    if(!flow&&method.returnType!=='void')throw new Unknown('Der betrachtete Pfad liefert keinen belegten Rückgabewert.');
    const result=flow?.value??null;
    checkType(method.returnType,result);
    stack.pop();returnTypes.pop();values=callerValues;origins=callerOrigins;current=callerToken;types=callerTypes;
    return result;
  };
  try {
    if(code.length>500_000)throw new Unknown('Der Code überschreitet 500 KB.');
    all=methods(tokenize(code));const candidates=all.filter(method=>method.start<=offset&&offset<method.end);
    const entry=candidates.length===1?candidates[0]:all.length===1?all[0]:undefined;
    if(!entry)throw new Unknown('Bitte eine unterstützte C#-Methode mit Blockrumpf auswählen.');
    trace.method=entry.name;const args=entry.parameters.map(name=>{if(!Object.hasOwn(scenario.inputs,name))throw new Unknown('Eingabeparameter fehlt: '+name);return scenario.inputs[name];});
    const result=invoke(entry,args);trace.result=result;trace.outcome=steps.some(step=>step.kind==='return'&&step.stack.length===1)?'returned':'completed';
  } catch(error) {
    if(error instanceof ModeledThrow){trace.outcome='thrown';trace.result=error.value;}
    else {trace.outcome='unknown';delete trace.result;const reason=error instanceof Error?error.message:String(error);if(steps.length>=500)steps.pop();add('unknown',reason,'unknown');}
  }
  if(scenario.start_line&&!steps.some(step=>step.line===scenario.start_line)&&trace.outcome!=='unknown'){
    trace.outcome='unknown';delete trace.result;
    if(steps.length>=500)steps.pop();add('unknown','Die Startzeile wird in diesem Szenario nicht als unterstützte Anweisung erreicht.','unknown');
  }
  return trace;
}
class ModeledThrow {constructor(readonly value:SimValue){}}

export function firstDivergence(left:SimTrace,right:SimTrace):number|undefined {
  const length=Math.max(left.steps.length,right.steps.length);
  for(let i=0;i<length;i++){const a=left.steps[i],b=right.steps[i];if(!a||!b||JSON.stringify([a.kind,a.values,a.result,a.text])!==JSON.stringify([b.kind,b.values,b.result,b.text]))return i;}
  return undefined;
}

export function simulationParameters(code:string,offset:number):{method?:string;parameters:{name:string;type:string}[];limitation?:string} {
  try {
    const all=methods(tokenize(code)),candidates=all.filter(method=>method.start<=offset&&offset<method.end);
    const entry=candidates.length===1?candidates[0]:all.length===1?all[0]:undefined;
    if(!entry)return{parameters:[],limitation:'Bitte eine C#-Methode mit Blockrumpf als Arbeitsbereich wählen.'};
    return{method:entry.name,parameters:entry.parameters.map((name,i)=>({name,type:entry.parameterTypes[i]})),limitation:entry.unsupported};
  }catch(error){return{parameters:[],limitation:error instanceof Error?error.message:String(error)};}
}
