const api=acquireVsCodeApi(),el=id=>document.getElementById(id);
let session,index=0,stopped=false,seenId,parameterKey,parameters=[];
const labels={derived:'Aus Code ableitbar',assumption:'Szenarioannahme',hypothesis:'KI-Hypothese',unknown:'Unbekannt'};
const post=(action,extra={})=>api.postMessage({action,...extra});
function error(text){el('simError').textContent=text;el('simError').hidden=!text;}
function start(compare){try{const inputs=parameters.length?Object.fromEntries(parameters.map((p,i)=>{const control=el('parameter-'+i);let value=control.value;if(p.type==='int'){if(value.trim()===''||!Number.isInteger(Number(value)))throw new Error('Bitte eine ganze Zahl für '+p.name+' eingeben.');value=Number(value);}else if(p.type==='bool'){if(!['true','false'].includes(value))throw new Error('Bitte '+p.name+' auf true oder false setzen.');value=value==='true';}else if(!['string','string?'].includes(p.type))value=JSON.parse(value);return[p.name,value];})):JSON.parse(el('scenarioInputs').value);const scenario={name:el('scenarioName').value,version:1,inputs,stubs:JSON.parse(el('scenarioStubs').value)};if(el('scenarioStart').value)scenario.start_line=Number(el('scenarioStart').value);post('simulate',{scenario,compare});error('');}catch(e){error('Bitte die Szenariowerte und JSON-Felder prüfen. '+e.message);}}
el('simulate').onclick=()=>start(false);el('compareSimulation').onclick=()=>start(true);
function move(target){if(!session||session.stale)return;index=Math.max(0,Math.min(target,Math.max(session.baseline.steps.length,session.proposed?.steps.length||0)-1));stopped=false;render();post('focusSimulation',{index,side:'baseline'});}
el('stepBack').onclick=()=>move(index-1);el('stepInto').onclick=()=>move(index+1);
el('stepOver').onclick=()=>{const steps=session.baseline.steps,depth=steps[index]?.stack.length||0;let next=index+1;while(next<steps.length-1&&steps[next].stack.length>depth)next++;move(next);};
el('stepOut').onclick=()=>{const steps=session.baseline.steps,depth=steps[index]?.stack.length||0;let next=index+1;while(next<steps.length-1&&steps[next].stack.length>=depth)next++;move(next);};
el('runTo').onclick=()=>{const target=Number(el('runToLine').value),next=session.baseline.steps.findIndex((step,i)=>i>index&&step.line===target);if(next<0)error('Die Zeile wird im verbleibenden Trace nicht erreicht.');else move(next);};
el('stopSimulation').onclick=()=>{stopped=true;post('stopSimulation');render();};
el('saveSimulationLearning').onclick=()=>post('simulationLearning',{text:el('simulationLearning').value,id:session.id});
function element(tag,text,parent){const node=document.createElement(tag);node.textContent=text;parent.append(node);return node;}
function column(trace,title,side){const box=document.createElement('article');box.className='card';element('h2',title,box);const step=trace.steps[index];
  if(!step){element('p','Dieser Pfad ist bereits beendet: '+trace.outcome,box);return box;}
  element('p',labels[step.evidence]+' · Zeile '+step.line,box).className='eyebrow';element('pre',step.text,box);element('p',step.reason,box);
  element('h3','Call Stack',box);element('pre',step.stack.join(' → ')||'–',box);
  element('h3','Werte und Herkunft',box);const dl=document.createElement('dl');
  for(const [name,value] of Object.entries(step.values)){element('dt',name,dl);element('dd',JSON.stringify(value)+' · '+(step.origins[name]||'Unbekannt'),dl);}box.append(dl);
  if(Object.hasOwn(step,'result'))element('p','Ergebnis: '+JSON.stringify(step.result),box);
  const focus=element('button','Zeile im Editor zeigen',box);focus.onclick=()=>post('focusSimulation',{index,side});focus.disabled=session.stale;
  return box;
}
function render(){el('traceArea').hidden=!session;el('simStale').hidden=!session?.stale;if(!session)return;
  el('simBinding').textContent=`${session.path} · Szenario v${session.scenario.version} · Interpreter v${session.scenario.engine_version||1} · Code ${session.baseline_hash.slice(0,8)}${session.duration_ms!==undefined?' · '+session.duration_ms+' ms lokal':''}`;
  el('tracePosition').textContent=`${stopped?'Gestoppt · ':''}Simulierter Schritt ${index+1} / ${Math.max(session.baseline.steps.length,session.proposed?.steps.length||0)} · keine reale Verifikation`;
  el('divergence').hidden=session.divergence===undefined;el('divergence').textContent='Erste Abweichung bei Simulationsschritt '+(session.divergence+1)+'. Verglichen werden dieselben Eingaben und Stubs.';
  el('traceColumns').replaceChildren(column(session.baseline,'Aktueller Code','baseline'));if(session.proposed)el('traceColumns').append(column(session.proposed,'Nicht übernommener Vorschlag','proposed'));
  for(const id of ['stepBack','stepInto','stepOver','stepOut','runTo','saveSimulationLearning'])el(id).disabled=session.stale;
  el('stepBack').disabled=session.stale||index===0;
  for(const id of ['stepInto','stepOver','stepOut','runTo'])el(id).disabled=session.stale||index>=Math.max(session.baseline.steps.length,session.proposed?.steps.length||0)-1;
}
window.addEventListener('message',({data})=>{if(data.type==='error'){error(data.text);return;}if(data.type!=='simulationState')return;session=data.session;el('compareSimulation').disabled=!data.canCompare;el('simulate').disabled=data.busy;
  if(data.context){el('simulationMethod').textContent=data.context.method?'Methode: '+data.context.method:data.context.limitation;const key=JSON.stringify(data.context);if(key!==parameterKey){parameterKey=key;parameters=data.context.parameters||[];el('simulationParameters').replaceChildren();parameters.forEach((p,i)=>{const label=element('label',p.name+' · '+p.type,el('simulationParameters'));const control=document.createElement(p.type==='bool'?'select':['int','string','string?'].includes(p.type)?'input':'textarea');control.id='parameter-'+i;if(p.type==='bool'){for(const [value,text] of [['','Bitte wählen'],['true','true'],['false','false']]){const option=element('option',text,control);option.value=value;}}else if(p.type==='int')control.type='number';else if(!['string','string?'].includes(p.type))control.placeholder='Objekt als JSON oder null';label.append(control);if(session&&Object.hasOwn(session.scenario.inputs,p.name))control.value=['string','string?'].includes(p.type)?session.scenario.inputs[p.name]:JSON.stringify(session.scenario.inputs[p.name]);});el('rawInputLabel').hidden=parameters.length>0;}}
  if(session?.id!==seenId){seenId=session?.id;index=Math.max(0,session?.baseline.steps.findIndex(step=>session.scenario.start_line?step.line===session.scenario.start_line:step.kind!=='enter')||0);stopped=false;if(session){el('scenarioName').value=session.scenario.name;el('scenarioStart').value=session.scenario.start_line??'';el('scenarioInputs').value=JSON.stringify(session.scenario.inputs,null,2);el('scenarioStubs').value=JSON.stringify(session.scenario.stubs,null,2);el('scenarioEditor').open=false;}error('');}render();});
post('simulationReady');
