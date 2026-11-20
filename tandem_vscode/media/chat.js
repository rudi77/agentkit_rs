const api=acquireVsCodeApi(),el=id=>document.getElementById(id),post=(action,extra={})=>api.postMessage({action,...extra});
let current,sessionId,pending,transcriptKey='',actionsKey='';
let draft=api.getState?.();
const show=(id,value)=>el(id).hidden=!value;
function inline(parent,value){for(const part of value.split(/(\*\*[^*]+\*\*|`[^`]+`)/g)){const tag=part.startsWith('**')&&part.endsWith('**')?'strong':part.startsWith('`')&&part.endsWith('`')?'code':undefined;if(tag){const n=document.createElement(tag);n.textContent=part.slice(tag==='strong'?2:1,tag==='strong'?-2:-1);parent.append(n);}else parent.append(document.createTextNode(part));}}
function markdown(target,value){target.replaceChildren();let list,paragraph,fence;for(const line of String(value||'').split('\n')){if(line.startsWith('```')){if(fence)fence=undefined;else{const pre=document.createElement('pre');fence=document.createElement('code');pre.append(fence);target.append(pre);}list=paragraph=undefined;continue;}if(fence){fence.append(document.createTextNode(line+'\n'));continue;}if(!line.trim()){list=paragraph=undefined;continue;}const heading=line.match(/^(#{1,6})\s+(.*)/),bullet=line.match(/^\s*(?:[-*]|\d+\.)\s+(.*)/);if(heading){const n=document.createElement(heading[1].length<3?'h2':'h3');inline(n,heading[2]);target.append(n);list=paragraph=undefined;}else if(bullet){if(!list){list=document.createElement(/^\s*\d/.test(line)?'ol':'ul');target.append(list);}const n=document.createElement('li');inline(n,bullet[1]);list.append(n);paragraph=undefined;}else{list=undefined;if(!paragraph){paragraph=document.createElement('p');target.append(paragraph);}else paragraph.append(document.createTextNode(' '));inline(paragraph,line);}}}
function button(parent,label,action,extra={},primary=false){const n=document.createElement('button');n.textContent=label;n.className=primary?'primary':'';n.dataset.mutation=['openSource','preview'].includes(action)?'false':'true';n.onclick=()=>post(action,extra);parent.append(n);return n;}
function node(parent,tag,text){const n=document.createElement(tag);if(text!==undefined)n.textContent=text;parent.append(n);return n;}
function card(title){const n=node(el('contextActions'),'article');n.className='card';node(n,'strong',title);return n;}
function actions(parent){const n=node(parent,'div');n.className='actions';return n;}
function saveDraft(){draft={sessionId,text:el('prompt').value};api.setState?.(draft);}
const encoder=new TextEncoder(),maxInputBytes=Number(el('prompt').dataset.maxBytes);
function inputSize(){const bytes=encoder.encode(el('prompt').value).length;show('inputSize',bytes>=16000);el('inputSize').textContent=Math.ceil(bytes/1000)+' / '+Math.floor(maxInputBytes/1000)+' KB'+(bytes>maxInputBytes?' · Bitte auf mehrere Nachrichten aufteilen':'');el('inputSize').classList.toggle('over-limit',bytes>maxInputBytes);return bytes;}
function submit(){if(!current||!el('prompt').value.trim()||current.verifying||pending?.text===el('prompt').value)return;const bytes=inputSize();if(bytes>maxInputBytes){show('error',true);el('error').textContent='Deine Nachricht enthält '+Math.ceil(bytes/1000)+' KB. Pro Nachricht sind bis zu '+Math.floor(maxInputBytes/1000)+' KB möglich. Dein Text bleibt erhalten; teile größere Logs auf mehrere Nachrichten auf.';return;}pending={text:el('prompt').value};post('chatSend',{prompt:pending.text});}
el('compose').onsubmit=e=>{e.preventDefault();submit();};
el('prompt').onkeydown=e=>{if(e.key==='Enter'&&!e.shiftKey&&!e.isComposing){e.preventDefault();submit();}};
el('prompt').oninput=()=>{inputSize();saveDraft();el('send').disabled=!el('prompt').value.trim()||!!current?.verifying;};
for(const id of ['capture','captureAgain'])el(id).onclick=()=>post('capture');
for(const id of ['stop','openGraph','openSimulation','openDetails','newTask','restoreTask','clearVisualFocus','clearScope'])el(id).onclick=()=>{post(id);el('menu').open=false;};
document.addEventListener('pointerdown',event=>{if(!el('menu').contains(event.target))el('menu').open=false;});
el('menu').onkeydown=event=>{if(event.key==='Escape'){el('menu').open=false;el('menu').querySelector('summary').focus();}};
document.querySelectorAll('[data-example]').forEach(n=>n.onclick=()=>{el('prompt').value=n.dataset.example;el('prompt').focus();el('prompt').oninput();});
el('newMessages').onclick=()=>{el('scroll').scrollTop=el('scroll').scrollHeight;show('newMessages',false);};
el('scroll').onscroll=()=>{if(el('scroll').scrollHeight-el('scroll').scrollTop-el('scroll').clientHeight<90)show('newMessages',false);};
function planChecklist(parent,plan,s,proposalAt){
  const list=node(parent,'ul');list.className='plan-checklist';
  const labels={idea:'Geplant',clarifying:'In Klärung',ready:'Bereit',preview:'Diff vorgeschlagen',applied:'Vorschlag übernommen',verified:'Prüfung bestanden',accepted:'Abgeschlossen',deferred:'Zurückgestellt'};
  for(const step of plan.steps){
    const item=node(list,'li'),details=node(item,'details'),summary=node(details,'summary');details.open=step.id===s.activePlanStepId;
    const check=node(summary,'input');check.type='checkbox';check.checked=step.status==='accepted';check.disabled=true;check.setAttribute('aria-label',step.status==='accepted'?'Schritt abgeschlossen':'Schritt noch nicht abgeschlossen');
    node(summary,'span',step.title).className='step-title';node(summary,'small',labels[step.status]||'Geplant').className='step-status';
    const sub=node(details,'ul');sub.className='plan-subitems';
    node(node(sub,'li'),'p','Erfolgskriterium: '+step.acceptance);
    for(const file of step.files){const fileItem=node(sub,'li');node(fileItem,'code',file);button(fileItem,'Änderung vorschlagen','chatBeginImplementation',{id:step.id,proposalAt,file});}
    const background=node(details,'details');node(background,'summary','Begründung und Grenzen');if(step.why)node(background,'p',step.why);
    const detailLabels={needs_now:'Bedarf',smallest_change:'Kleinster Eingriff',reuse:'Wiederverwendung',invariants:'Beibehalten',not_now:'Nicht-Ziele',assumptions:'Annahmen',risks:'Risiken'};
    for(const [key,label] of Object.entries(detailLabels))if(step.details?.[key])node(background,'p',label+': '+step.details[key]);
    button(details,'Schritt besprechen','chatUseStep',{id:step.id,proposalAt});
  }
  if(plan.open_questions){const d=node(parent,'details');node(d,'summary','Offene Fragen');node(d,'p',plan.open_questions);}
  if(plan.not_now){const d=node(parent,'details');node(d,'summary','Nicht-Ziele');node(d,'p',plan.not_now);}
}
function renderActions(s,blocked){
  const key=JSON.stringify([s.exploration?.created_at,s.changePlan?.revision,s.changePlan?.stale,s.changePlan?.steps,s.dismissedPlanProposalAt,s.activePlanStepId,s.scope?.uri,s.suggestion,s.proposal,s.evidence,s.scopeSuggestion]);
  if(actionsKey!==key){actionsKey=key;el('contextActions').replaceChildren();
    if(s.proposal?.status==='ready'){
      const c=card('Änderungsvorschlag');node(c,'p',s.proposal.patch.reason);node(c,'p',s.proposal.patch.path).className='quiet';const a=actions(c);button(a,'Diff ansehen','preview',{},true);button(a,'Übernehmen','apply');button(a,'Verwerfen','discard');
    }else if(s.proposal?.status==='stale'){const c=card('Der Vorschlag ist veraltet');node(c,'p','Code oder Vereinbarung haben sich geändert. Besprich die Korrektur hier oder aktualisiere die Codeauswahl.');}
    if(s.scopeSuggestion){const c=card('Dafür wäre mehr Code nötig');node(c,'p',s.scopeSuggestion.reason);node(c,'p','Kleinere Alternative: '+s.scopeSuggestion.alternative);const a=actions(c);button(a,'Beim bisherigen Umfang bleiben','resolveScope',{resolution:'exclude'},true);if(s.changePlan)button(a,'Als eigenen Schritt planen','resolveScope',{resolution:'separate'});}
    if(s.suggestion){const c=card('So könnten wir es eingrenzen');node(c,'p',s.suggestion.goal);node(c,'p','Erfolgskriterium: '+(s.suggestion.acceptance||'Noch zu klären'));const d=node(c,'details');node(d,'summary','Grenzen und Begründung');node(d,'p',s.suggestion.not_now);node(d,'p',s.suggestion.reason);if(s.scope&&s.suggestion.acceptance?.trim())button(actions(c),'Änderung vorschlagen','chatPrepare',{},true);else button(actions(c),'Vereinbarung übernehmen','adopt');}
    if(s.proposal?.status==='applied'||s.evidence){const c=card(s.evidence?.status==='passed'?'Prüfung erfolgreich':s.evidence?.status==='failed'?'Prüfung fehlgeschlagen':s.evidence?.status==='running'?'Prüfung läuft …':'Änderung prüfen');node(c,'p',s.evidence?.status==='stale'?'Der Prüfbeleg ist veraltet.':s.evidence?.command||'Die Übernahme allein bestätigt das Verhalten noch nicht.');button(actions(c),'Tests ausführen','verify',{},true);}
    const plan=s.exploration?.suggested_plan,shared=s.changePlan;
    const pendingPlan=plan&&(!shared||shared.basis_created_at&&shared.basis_created_at!==s.exploration.created_at)&&s.dismissedPlanProposalAt!==s.exploration.created_at;
    if(pendingPlan){
      const c=card('Planvorschlag');node(c,'p',plan.goal);const a=actions(c),next=plan.steps[0];
      if(next)button(a,'Umsetzung beginnen','chatBeginImplementation',{id:next.id,proposalAt:s.exploration.created_at},true);
      if(next)button(a,'Diesen Schritt besprechen','chatUseStep',{id:next.id,proposalAt:s.exploration.created_at});
      if(shared)button(a,'Bestehenden Plan behalten','keepPlan');
      planChecklist(c,plan,s,s.exploration.created_at);
    }else if(shared){
      const c=card('Gemeinsamer Plan');node(c,'p',shared.goal);
      if(shared.stale)node(c,'p','Quellenstand geändert · vor der Umsetzung wird der Planstand geprüft.').className='quiet';
      const next=shared.steps.find(step=>!['accepted','deferred'].includes(step.status));
      if(next)button(actions(c),'Umsetzung beginnen','chatBeginImplementation',{id:next.id},true);
      planChecklist(c,shared,s);
    }
    if(s.exploration?.sources.length){const d=node(el('contextActions'),'details');node(d,'summary',`Quellen · ${s.exploration.sources.length}`);const list=node(d,'div');list.className='sources';s.exploration.sources.forEach((source,index)=>button(list,`${source.path}:${source.start_line}`,'openSource',{index}));}
  }
  el('contextActions').querySelectorAll('button').forEach(n=>n.disabled=blocked&&n.dataset.mutation==='true');
}
function render(message){
  const s=message.state,scroller=el('scroll'),previousScroll=scroller.scrollTop,follow=scroller.scrollHeight-scroller.scrollTop-scroller.clientHeight<100;
  current=message;
  if(sessionId!==s.sessionId){sessionId=s.sessionId;el('prompt').value=draft?.sessionId===sessionId?draft.text||'':'';pending=undefined;transcriptKey=actionsKey='';saveDraft();}
  if(pending&&message.busy&&s.chat.at(-1)?.role==='user'&&s.chat.at(-1).text===pending.text){if(el('prompt').value===pending.text)el('prompt').value='';pending=undefined;saveDraft();}
  const key=JSON.stringify(s.chat);
  const transcriptChanged=transcriptKey!==key;
  if(transcriptKey!==key){transcriptKey=key;el('messages').replaceChildren();for(const item of s.chat){const article=node(el('messages'),'article');article.className='message '+(item.role==='user'?'user':'assistant');node(article,'strong',item.role==='user'?'Du':'TANDEM');if(item.context)node(article,'small',item.context).className='message-context';const body=node(article,'div');if(item.role==='user'){const text=item.text.replace(/^Repository-Aufgabe: /,'');if(text.length>12000){const details=node(body,'details');node(details,'summary','Eingefügter Text · '+Math.ceil(encoder.encode(text).length/1000)+' KB');const content=node(details,'pre',text);content.className='long-input';content.tabIndex=0;content.setAttribute('aria-label','Vollständig eingefügter Text');}else body.textContent=text;}else{body.className='markdown';markdown(body,item.text);}}}
  show('welcome',s.chat.length===0);show('stream',!!message.busy&&!!message.stream&&!message.stream.startsWith('Leseanalyse:'));markdown(el('streamText'),message.stream);el('streamContext').textContent=message.streamContext||'';
  const blocked=message.busy||message.verifying||s.delegation?.status==='running';renderActions(s,blocked);
  el('provider').textContent=message.provider==='demo'?'Offline-Demo':'AgentKit';show('activity',blocked);el('activityText').textContent=message.verifying?'Prüfung läuft …':message.stream?.startsWith('Leseanalyse:')?'Ich schaue mir den Code an …':'TANDEM antwortet …';el('send').textContent=message.busy?'Unterbrechen & senden':'Senden ↑';el('send').disabled=!el('prompt').value.trim()||!!message.verifying;
  const visual=message.visualFocus,origins={analysis:'Sprachdienst',documented:'Dokumentiert',hypothesis:'Hypothese · nicht belegt',intent:'Vereinbart',catalog:'Dateigruppierung'};show('visualContext',!!visual);el('visualLabel').textContent=visual?.label||'';el('visualContext').title=visual?.reason||'';el('visualContext').classList.toggle('stale',!!visual?.stale);el('visualOrigin').textContent=visual?(visual.stale?'Veraltet · ':'')+origins[visual.origin]:'';el('prompt').placeholder=visual?'Frage zu „'+visual.label.slice(0,80)+'“ …':'Frag etwas oder beschreib, was du ändern möchtest …';
  show('attachment',!!s.scope);el('scopeLabel').textContent=s.scope?`Codeauswahl: ${s.scope.path.split(/[\\/]/).pop()} · Zeilen ${s.scope.prefix.split('\n').length}–${(s.scope.prefix+s.scope.editable).split('\n').length}`:'';
  el('capture').disabled=blocked;el('captureAgain').disabled=blocked;el('clearScope').disabled=blocked;el('newTask').disabled=!!message.verifying;el('restoreTask').hidden=!s.archivedSessions?.length;
  show('error',!!message.error);el('error').textContent=message.error||'';
  inputSize();
  if(follow)scroller.scrollTop=scroller.scrollHeight;else{scroller.scrollTop=previousScroll;if(message.busy||transcriptChanged)show('newMessages',true);}
}
window.addEventListener('message',({data})=>{if(data.type==='state')render(data);else if(data.type==='error'){show('error',true);el('error').textContent=data.text;pending=undefined;}});
post('ready');post('repositoryReady');
