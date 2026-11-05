// Eine sichtbare Arbeitsphase, ein gemeinsamer Plan und sichere Textdarstellung.
const api = acquireVsCodeApi();
const el = id => document.getElementById(id);
const post = (action, extra = {}) => api.postMessage({ action, ...extra });
let current, page, intent, selectedStep, dismissedProposal;
let stepRevision = -1, editorRevision = -1, stepDirty = false, planDirty = false;
let draftSteps = [], cardsKey = '', sourcesKey = '', filesKey = '', previewed;
let questionStep;
let contractVersion;
let sessionKey;
let contractDirty=false,discussionKey,pendingDecision,decisionOptionsKey;
const show = (id, visible) => { el(id).hidden = !visible; };
const text = (id, value) => { el(id).textContent = value || ''; };
const short = (value, length = 220) => { const s = String(value || '').replace(/\s+/g, ' ').trim(); return s.length > length ? s.slice(0, length) + '…' : s; };
const basename = path => path.split(/[\\/]/).pop();
const statusLabels = {idea:'Idee',clarifying:'In Klärung',ready:'Bereit',preview:'Vorschlag',applied:'Übernommen',verified:'Real geprüft',accepted:'Fachlich abgeschlossen',deferred:'Zurückgestellt'};
const detailLabels = {needs_now:'Brauchen wir jetzt',not_now:'Brauchen wir nicht',smallest_change:'Kleinster erwarteter Eingriff',reuse:'Vorhandenes wiederverwenden',invariants:'Muss unverändert gelten',assumptions:'Offene Annahmen',risks:'Risiken und Alternativen'};
const decisionLabels = {decision:'Entscheidung',need:'Brauchen wir jetzt',exclude:'Brauchen wir nicht',assumption:'Annahme',risk:'Risiko',test_idea:'Testidee'};

// Markdown-Untermenge ohne HTML oder ausführbare Links. Alle Inhalte werden als
// Textknoten angelegt; auch Modellantworten dürfen keine DOM-Struktur einschleusen.
function inline(parent, value) {
  const parts = value.split(/(\*\*[^*]+\*\*|`[^`]+`)/g);
  for (const part of parts) {
    if (part.startsWith('**') && part.endsWith('**')) { const node = document.createElement('strong'); node.textContent = part.slice(2,-2); parent.append(node); }
    else if (part.startsWith('`') && part.endsWith('`')) { const node = document.createElement('code'); node.textContent = part.slice(1,-1); parent.append(node); }
    else { parent.append(document.createTextNode(part)); }
  }
}
function markdown(id, value) {
  const target = typeof id==='string' ? el(id) : id, content = String(value || '');
  if (target.dataset.rendered === content) return;
  target.dataset.rendered = content; target.replaceChildren();
  let list, paragraph, fence;
  for (const line of content.split('\n')) {
    if (line.startsWith('```')) {
      if (fence) { fence = undefined; } else { const pre = document.createElement('pre'); fence = document.createElement('code'); pre.append(fence); target.append(pre); }
      paragraph = list = undefined; continue;
    }
    if (fence) { fence.append(document.createTextNode(line + '\n')); continue; }
    if (!line.trim()) { paragraph = list = undefined; continue; }
    const heading = line.match(/^(#{1,6})\s+(.*)/);
    const bullet = line.match(/^\s*(?:[-*]|\d+\.)\s+(.*)/);
    if (heading) { const node = document.createElement(heading[1].length < 3 ? 'h2' : 'h3'); inline(node,heading[2]); target.append(node); paragraph = list = undefined; }
    else if (bullet) { if (!list) { list = document.createElement(/^\s*\d/.test(line) ? 'ol' : 'ul'); target.append(list); } const li = document.createElement('li'); inline(li,bullet[1]); list.append(li); paragraph = undefined; }
    else { list = undefined; if (!paragraph) { paragraph = document.createElement('p'); target.append(paragraph); } else paragraph.append(document.createTextNode(' ')); inline(paragraph,line); }
  }
}
function go(next) {
  page = next;
  for (const name of ['task','plan','work']) show('page-' + name, name === page);
  document.querySelectorAll('.stages button').forEach(button => {
    if (button.dataset.page === page) button.setAttribute('aria-current','step'); else button.removeAttribute('aria-current');
  });
  window.scrollTo(0,0);
}
function request(action, extra, target) {
  if(contractDirty&&['apply','send','delegate'].includes(action)){fail('Bitte zuerst den geänderten Contract speichern.');return;}
  if (stepDirty && ['apply','send'].includes(action)) { fail('Bitte zuerst die Änderungen am Schritt speichern.'); return; }
  if (planDirty && ['usePlanStep','discussRepository','exploreRepository','adoptChangePlan'].includes(action)) { fail('Bitte zuerst den bearbeiteten Plan speichern.'); return; }
  intent = { action, target, analysis:current?.state.exploration?.created_at, revision:current?.state.changePlan?.revision, scope:current?.state.scope };
  post(action, extra);
  window.scrollTo(0,0);
}
function fail(message) { text('error',message); show('error',true); }
document.querySelectorAll('[data-page]').forEach(button => button.onclick = () => go(button.dataset.page));
el('continuePlan').onclick = () => go('plan');
el('exploreRepository').onclick = () => request('exploreRepository',{task:el('repositoryTask').value},'plan');
el('reanalyze').onclick = () => request('exploreRepository',{task:current.state.analysisTask || el('repositoryTask').value},'plan');
el('revalidate').onclick = () => request('revalidateAnalysis',{},'plan');
el('revalidateWork').onclick = () => request('revalidateAnalysis',{},'work');
el('refreshPlanBasis').onclick=()=>post('refreshPlanBasis');
el('stop').onclick = () => post('stop');
for(const action of ['prepareFactory','runFactory','acceptFactory','importFactory','openFactory'])el(action).onclick=()=>post(action,action==='prepareFactory'?{prompt:el('prompt').value}:{});
el('pausePreview').onclick=()=>post('pausePreview');
for (const id of ['capture','captureAgain','captureDirect','captureEmpty']) el(id).onclick = () => request('capture',{},'work');
el('preview').onclick = () => { previewed = current.state.proposal; post('preview'); render(current); };
el('apply').onclick = () => request('apply',{},'work');
el('discard').onclick = () => post('discard');
el('verify').onclick = () => post('verify');
el('adopt').onclick = () => post('adopt');
el('openSimulation').onclick=()=>post('openSimulation');
el('editProposal').onclick=()=>post('editProposal');el('useDraft').onclick=()=>post('useDraft');
el('saveContract').onclick=()=>post('saveContract',{rules:{mode:el('contractMode').value,preserve_public_signatures:el('preserveSignatures').checked,allow_new_symbols:el('allowNewSymbols').checked,invariants:el('contractInvariants').value}});
el('excludeScope').onclick=()=>post('resolveScope',{resolution:'exclude'});el('separateScope').onclick=()=>post('resolveScope',{resolution:'separate'});
el('delegate').onclick=()=>{if(contractDirty){fail('Bitte zuerst den geänderten Contract speichern.');return;}post('delegate',{prompt:el('prompt').value||'Erledige den vereinbarten risikoarmen Schritt.',policy:{max_changed_lines:Number(el('delegateBudget').value),auto_apply:el('delegateAutoApply').checked,low_risk:el('delegateLowRisk').checked}});};
for(const id of ['contractMode','preserveSignatures','allowNewSymbols','contractInvariants'])el(id).oninput=()=>{contractDirty=true;text('contractDirtyStatus','Ungespeicherte Rechteänderung · zuerst Contract aktualisieren');el('saveContract').className='primary';};
el('arrange').onclick=()=>post('arrange');el('exportArtifact').onclick=()=>post('exportArtifact',{includeCode:el('exportCode').checked});
el('openGraph').onclick=()=>post('openGraph');
el('newTask').onclick=()=>post('newTask');el('restoreTask').onclick=()=>post('restoreTask');
el('reviewHumanChanges').onclick=()=>post('reviewHumanChanges',{prompt:el('prompt').value});
el('saveEvaluation').onclick=()=>post('saveEvaluation',{evaluation:{cognitive_load:Number(el('cognitiveLoad').value),comprehension:Number(el('comprehension').value),interruption_cost:Number(el('interruptionCost').value),explanation:el('ownExplanation').value,notes:el('evaluationNotes').value}});
el('saveDecision').onclick = () => {pendingDecision={text:el('decisionText').value,previousId:current.state.decisions?.at(-1)?.id};post('saveDecision',{decision:{kind:el('decisionKind').value,text:pendingDecision.text,reason:el('decisionReason').value,...(el('decisionReplaces').value?{supersedes:el('decisionReplaces').value}:{})}});};
el('decisionReplaces').onchange=()=>{
  const previous=current.state.decisions?.find(item=>item.id===el('decisionReplaces').value);
  show('decisionReplaceHint',!!previous);text('saveDecision',previous?'Erkenntnis ersetzen':'Erkenntnis speichern');
  if(previous){el('decisionKind').value=previous.kind;el('decisionText').value=previous.text;el('decisionReason').value='';el('decisionText').focus();}
};
el('acceptStep').onclick = () => post('acceptStep',{kind:el('acceptanceKind').value,rationale:el('acceptanceReason').value});
el('discussPlan').onclick = () => request('discussRepository',{prompt:el('planMessage').value},'plan');
el('discussWork').onclick = () => {
  const prompt = el('prompt').value;
  if (current.state.analysisTask) request('discussRepository',{prompt},'plan');
  else request('send',{prompt,mode:'explore'},'work');
};
el('proposePatch').onclick = () => request('send',{prompt:el('prompt').value.trim() || 'Schlage die kleinste Codeänderung für das vereinbarte Ziel vor. Halte die Grenzen und Akzeptanzkriterien des aktuellen Schritts ein.',mode:'pair'},'work');
for (const id of ['goal','not_now','acceptance']) el(id).oninput = () => { stepDirty = true; text('unsaved',' Ungespeichert'); };
el('save').onclick = () => post('save',{goal:el('goal').value,not_now:el('not_now').value,acceptance:el('acceptance').value});

function visiblePlan(s) {
  const pending = s.exploration?.suggested_plan && (!s.changePlan || (s.changePlan.basis_created_at && s.changePlan.basis_created_at !== s.exploration.created_at)) && dismissedProposal !== s.exploration.created_at && s.dismissedPlanProposalAt!==s.exploration.created_at;
  return { plan:pending ? s.exploration.suggested_plan : s.changePlan, pending:!!pending };
}
el('planPrimary').onclick = () => {
  const {pending} = visiblePlan(current.state);
  if (pending) request('adoptChangePlan',{},'plan'); else request('usePlanStep',{id:selectedStep},'work');
};
el('togglePlanEdit').onclick = () => { show('planEditor',true); render(current); };
el('cancelPlanEdit').onclick = () => { show('planEditor',false); planDirty=false; editorRevision=-1; render(current); };
el('keepPlan').onclick = () => { dismissedProposal=current.state.exploration.created_at; selectedStep=current.state.activePlanStepId;post('keepPlan');render(current); };
function field(parent, label, value, rows, update) {
  const wrapper = document.createElement('label'); wrapper.textContent = label;
  const area = document.createElement('textarea'); area.rows = rows; area.value = value; area.setAttribute('aria-label',label);
  area.oninput = () => { update(area.value); markPlanDirty(); }; wrapper.append(area); parent.append(wrapper);
}
function markPlanDirty() { planDirty = true; text('changeDirty','Ungespeicherte Änderungen'); }
function renderEditorSteps() {
  el('changeSteps').replaceChildren();
  draftSteps.forEach((step,index) => {
    const box = document.createElement('div'); box.className = 'step-editor';
    const title = document.createElement('strong'); title.textContent = `Schritt ${index+1}`; box.append(title);
    field(box,'Schrittziel',step.title,2,value => step.title = value);
    field(box,'Warum dieser Schritt?',step.why,2,value => step.why = value);
    field(box,'Betroffene Dateien',step.files.join('\n'),2,value => step.files = value.split('\n').map(s => s.trim()).filter(Boolean));
    field(box,'Abnahmekriterium',step.acceptance,2,value => step.acceptance = value);
    const details=document.createElement('details'), summary=document.createElement('summary');summary.textContent='Bedarf, Minimalität und Risiken';details.append(summary);
    for(const [key,label] of Object.entries(detailLabels))field(details,label,step.details?.[key]||'',2,value=>{step.details ||= Object.fromEntries(Object.keys(detailLabels).map(name=>[name,'']));step.details[key]=value;});
    box.append(details);
    const actions = document.createElement('div'); actions.className = 'actions';
    for (const [label,action] of [
      ['Nach oben',() => { if(index>0) [draftSteps[index-1],draftSteps[index]] = [draftSteps[index],draftSteps[index-1]]; markPlanDirty(); renderEditorSteps(); }],
      ['Entfernen',() => { draftSteps.splice(index,1); markPlanDirty(); renderEditorSteps(); }]
    ]) { const button = document.createElement('button'); button.className = 'secondary small'; button.textContent = label; button.onclick = action; actions.append(button); }
    box.append(actions); el('changeSteps').append(box);
  });
}
for (const id of ['changeGoal','changeNotNow','changeQuestions']) el(id).oninput = markPlanDirty;
el('addChangeStep').onclick = () => { draftSteps.push({id:'human-'+crypto.randomUUID(),title:'Neuer Schritt',why:'Noch zu klären',files:[],acceptance:'Abnahmekriterium festlegen',source_ids:[]}); markPlanDirty(); renderEditorSteps(); };
el('saveChangePlan').onclick = () => request('saveChangePlan',{plan:{goal:el('changeGoal').value,not_now:el('changeNotNow').value,open_questions:el('changeQuestions').value,steps:draftSteps}},'plan');

function renderPlan(s, blocked) {
  const {plan,pending} = visiblePlan(s);
  show('planEmpty',!plan && !s.exploration); show('planContent',!!plan || !!s.exploration);
  show('planNoProposalAnswer',!plan && !!s.exploration);show('planSteps',!!plan);
  el('discussPlan').className=plan?'secondary':'primary';
  if (!plan) {
    text('planTitle',s.exploration?'Was noch zu klären ist':'Wie gehen wir vor?');
    text('planIntro',s.exploration?'TANDEM hat noch keinen Plan vorgeschlagen. Antworte direkt auf die Rückfrage oder ergänze das gewünschte Verhalten.':'Ein kleiner, gemeinsamer Plan hält die Arbeit überschaubar.');
    markdown('planNoProposalAnswer',s.exploration?.answer);
    for(const id of ['planPrimaryActions','planEditor','staleNotice','questionsDetail'])show(id,false);
    el('discussPlan').disabled=blocked;
    return;
  }
  text('planEyebrow',pending ? 'Vorschlag · noch keine Codeänderung' : 'Euer gemeinsamer Plan');
  text('planTitle',pending ? 'Diesen Weg schlägt TANDEM vor' : 'Wähle den nächsten Schritt');
  text('planIntro',short(plan.goal,360));
  if (!plan.steps.some(step => step.id === selectedStep)) selectedStep = s.activePlanStepId || plan.steps[0]?.id;
  if (!plan.steps.some(step => step.id === selectedStep)) selectedStep = plan.steps[0]?.id;
  const stale = s.exploration?.stale || (!pending && s.changePlan?.stale);
  show('staleNotice',stale); show('planPrimaryActions',!stale && el('planEditor').hidden);
  show('refreshPlanBasis',!!s.changePlan?.stale&&!s.exploration?.stale);el('refreshPlanBasis').disabled=blocked;
  const key = JSON.stringify([plan,pending,selectedStep,blocked,planDirty]);
  if (cardsKey !== key) {
    cardsKey = key; el('planSteps').replaceChildren(); el('planSteps').setAttribute('role','radiogroup'); el('planSteps').setAttribute('aria-label','Planschritt');
    plan.steps.forEach((step,index) => {
      const card = document.createElement('div'); card.className = 'step-card'; card.setAttribute('role','radio'); card.setAttribute('aria-checked',String(step.id===selectedStep)); card.tabIndex = 0;
      const heading = document.createElement('div'); heading.className = 'step-heading';
      const number = document.createElement('span'); number.className = 'step-number'; number.textContent = String(index+1).padStart(2,'0');
      const title = document.createElement('span'); title.className = 'step-title'; title.textContent = step.title; heading.append(number,title); card.append(heading);
      const status=document.createElement('small');status.className='step-state';status.textContent=statusLabels[step.status||'clarifying'];card.append(status);
      const why = document.createElement('p'); why.className = 'step-why'; why.textContent = short(step.why,190); card.append(why);
      const detail = document.createElement('details'); const summary = document.createElement('summary'); summary.textContent = 'Dateien und Abnahmekriterium'; detail.append(summary);
      const files = document.createElement('p'); files.className = 'code-location'; files.textContent = step.files.join('\n'); const acceptance = document.createElement('p'); acceptance.textContent = step.acceptance; detail.append(files,acceptance); card.append(detail);
      card.onclick = event => { if(event.target.closest('details')) return; selectedStep=step.id; render(current); };
      card.onkeydown = event => { if(event.target!==card)return;if(event.key==='Enter'||event.key===' ') { event.preventDefault(); selectedStep=step.id; render(current); }else if(['ArrowDown','ArrowRight','ArrowUp','ArrowLeft'].includes(event.key)){event.preventDefault();const direction=['ArrowDown','ArrowRight'].includes(event.key)?1:-1;selectedStep=plan.steps[(index+direction+plan.steps.length)%plan.steps.length].id;render(current);el('planSteps').querySelector('[aria-checked=true]').focus();} };
      el('planSteps').append(card);
    });
    if(!pending) {
      const actions=document.createElement('div');actions.className='actions';
      for(const [label,operation] of [['Schritt teilen','split'],['Mit nächstem zusammenführen','merge'],['Zurückstellen','defer']]) {
        const button=document.createElement('button');button.className='secondary small';button.textContent=label;
        button.disabled=blocked||planDirty||(operation==='merge'&&plan.steps.at(-1)?.id===selectedStep);
        button.onclick=()=>post('reshapeStep',{id:selectedStep,operation});actions.append(button);
      }
      el('planSteps').append(actions);
    }
  }
  text('planPrimary',pending ? 'Diesen Plan verwenden' : 'An diesem Schritt arbeiten');
  el('planPrimary').disabled = blocked;
  el('togglePlanEdit').hidden = pending;
  show('keepPlan',pending && !!s.changePlan);
  if (pending) show('planEditor',false);
  if (s.changePlan && editorRevision !== s.changePlan.revision) {
    editorRevision = s.changePlan.revision; planDirty = false; text('changeDirty','');
    el('changeGoal').value=s.changePlan.goal; el('changeNotNow').value=s.changePlan.not_now; el('changeQuestions').value=s.changePlan.open_questions;
    draftSteps=structuredClone(s.changePlan.steps); renderEditorSteps();
  }
  show('questionsDetail',!!plan.open_questions.trim()); markdown('planQuestions',plan.open_questions);
  el('discussPlan').disabled = blocked;
  el('revalidate').disabled = blocked; el('reanalyze').disabled = blocked;
}

function renderWork(s, blocked) {
  const active = s.changePlan?.steps.find(step => step.id===s.activePlanStepId);
  const hasWork = !!active || !!s.scope;
  const stale = !!active && s.changePlan.stale;
  const humanChange=s.humanChange;show('humanChangeBox',!!humanChange&&humanChange.before!==humanChange.after);el('reviewHumanChanges').disabled=blocked;
  show('humanReviewDetail',!!humanChange?.review);markdown('humanReview',humanChange?.review?.answer);text('humanReviewStatus',humanChange?.review?(humanChange.review.stale?'Begutachtung veraltet · der Code wurde weiter geändert.':'Lesend begutachtet · noch kein realer Test.'):'');
  const needsVerification=s.proposal?.status==='applied'||!!s.evidence;
  show('workEmpty',!hasWork); show('workStale',stale); show('workContent',hasWork && (!stale||needsVerification));
  el('revalidateWork').disabled=blocked;
  text('workTitle',active?.title || (s.scope ? s.plan.goal : 'Gemeinsam am nächsten Schritt arbeiten'));
  text('workIntro',active ? short(active.why,230) : 'Fragen klären, einen kleinen Vorschlag prüfen und das Verhalten verifizieren.');
  if (!hasWork || (stale&&!needsVerification)) return;
  const nextDiscussionKey=(active?.id||'direct')+':'+(s.scope?.uri||'')+':'+(s.scope?.version||'');if(discussionKey!==nextDiscussionKey){discussionKey=nextDiscussionKey;el('workDiscussion').open=!s.scope;}
  text('stepProgress',statusLabels[active?.status||'clarifying']);
  show('stepUnderstanding',!!active);show('decisionEditor',true);show('acceptanceBox',!!active);
  markdown('stepFacts',active?Object.entries(detailLabels).map(([key,label])=>`### ${label}\n${active.details?.[key]||'Noch nicht festgelegt'}`).join('\n\n')+'\n\n### Abnahmekriterium\n'+active.acceptance:'');
  const decisions=(s.decisions||[]).filter(item=>!item.step_id||item.step_id===active?.id);
  const superseded=new Set((s.decisions||[]).map(item=>item.supersedes).filter(Boolean)),effective=decisions.filter(item=>!superseded.has(item.id)),historical=decisions.filter(item=>superseded.has(item.id));
  const describe=item=>`### ${decisionLabels[item.kind]} · ${item.by==='human'?'Du':'TANDEM'}\n${item.text}\n\n${item.reason}`;
  markdown('stepDecisions',effective.map(describe).join('\n\n'));
  show('decisionHistoryBox',historical.length>0);markdown('decisionHistory',historical.map(item=>describe(item)+'\n\nHistorisch · durch eine spätere Erkenntnis ersetzt.').join('\n\n'));
  const replaceable=effective.filter(item=>item.step_id===active?.id),optionsKey=JSON.stringify(replaceable.map(item=>[item.id,item.text]));
  if(decisionOptionsKey!==optionsKey){
    decisionOptionsKey=optionsKey;const selected=el('decisionReplaces').value;el('decisionReplaces').replaceChildren();
    const option=document.createElement('option');option.value='';option.textContent='Neue Erkenntnis festhalten';el('decisionReplaces').append(option);
    for(const item of replaceable){const option=document.createElement('option');option.value=item.id;option.textContent='Ersetzen: '+short(item.text,95);el('decisionReplaces').append(option);}
    el('decisionReplaces').value=replaceable.some(item=>item.id===selected)?selected:'';
  }
  el('decisionReplaces').disabled=!!current.verifying;show('decisionReplaceHint',!!el('decisionReplaces').value);text('saveDecision',el('decisionReplaces').value?'Erkenntnis ersetzen':'Erkenntnis speichern');
  const accepted=(s.acceptances||[]).filter(item=>item.step_id===active?.id).at(-1);
  text('acceptanceResult',active?.status==='accepted'&&accepted?'Fachlich abgeschlossen: '+accepted.rationale:'');
  el('acceptStep').disabled=blocked;el('saveDecision').disabled=!!current.verifying;
  show('stepEditor',!active);
  const clarify = !!active && /klär|entscheid|vertrag|abgleich/i.test(active.title) && !s.scope;
  if(questionStep!==active?.id) { questionStep=active?.id;el('workQuestionsBox').open=clarify;el('chooseCode').open=!clarify; }
  show('workQuestionsBox',!!active && !!s.changePlan.open_questions.trim());
  markdown('workQuestions',short(s.changePlan?.open_questions,550));
  el('discussWork').className = clarify ? 'primary' : 'secondary';
  el('capture').className = clarify ? 'secondary' : 'primary';
  show('chooseCode',!s.scope); show('codeReady',!!s.scope);
  const files = active?.files || [];
  const key = JSON.stringify(files);
  if(filesKey!==key) {
    filesKey=key; el('planFiles').replaceChildren();
    for(const file of files) { const button=document.createElement('button');button.textContent=basename(file);button.title=file;button.onclick=()=>post('openPlanFile',{path:file});el('planFiles').append(button); }
  }
  if(s.scope) {
    const start=s.scope.prefix.split('\n').length, end=start+s.scope.editable.split('\n').length-1;
    text('scope',`${basename(s.scope.path)} · Zeilen ${start}–${end}`);
    const c=s.contract;
    markdown('contractSummary',c?`**${({pair:'Pair',explore:'Explore',delegate:'Delegate'})[c.mode]}** · Contract v${c.version}\n\n**Zweck:** ${c.needs_now}\n\n**Nicht ändern:** ${[...new Set(c.not_now.split("\n"))].join("\n")}\n\n**Erfolg:** ${c.acceptance||'Noch gemeinsam festlegen'}\n\nAgent: lesend analysieren${c.mode!=='explore'?', Patch vorschlagen':''}${c.mode==='delegate'?', begrenzte Übernahme nur mit gewählter Policy':''}. Keine Shell, keine direkten Dateischreibrechte.`:'Wähle den Codebereich erneut, um einen aktuellen Änderungsvertrag zu binden.');
    if(c&&contractVersion!==c.version){contractVersion=c.version;contractDirty=false;text('contractDirtyStatus','');el('saveContract').className='secondary';el('contractMode').value=c.mode;el('preserveSignatures').checked=c.preserve_public_signatures;el('allowNewSymbols').checked=c.allow_new_symbols;el('contractInvariants').value=c.invariants;}
  }
  if(stepRevision!==s.plan.revision) {
    stepRevision=s.plan.revision;stepDirty=false;text('unsaved','');for(const id of ['goal','not_now','acceptance'])el(id).value=s.plan[id];
  }
  const answer=[...s.chat].reverse().find(item=>item.role==='assistant' && item.text!==s.exploration?.answer);
  show('lastAnswer',!!answer);markdown('lastAnswer',answer?.text);
  const p=s.proposal;show('patchBox',!!p);
  if(p) {
    const ready=p.status==='ready';
    text('patchStatus',({ready:'Vorschlag · noch nicht übernommen',stale:'Vorschlag veraltet',applied:'Im Editor übernommen',discarded:'Vorschlag verworfen'})[p.status]);
    text('patchTitle',ready?'Prüfe die konkrete Änderung':p.status==='applied'?'Die Codeänderung ist übernommen':'Ein neuer Vorschlag ist nötig');
    markdown('proposal',p.patch.reason);
    for(const id of ['preview','editProposal','apply','discard']) { show(id,ready);el(id).disabled=blocked; }
    show('useDraft',ready&&!!current.draftAvailable);el('useDraft').disabled=blocked;
    el('preview').className=previewed===p?'secondary':'primary';el('apply').className=previewed===p?'primary':'secondary';
  }
  const applied=p?.status==='applied';
  show('proposePatch',!stale&&s.contract?.mode!=='delegate'&&(!p||['stale','discarded'].includes(p.status)));
  show('delegationBox',!stale&&s.contract?.mode==='delegate');el('delegate').disabled=blocked;
  show('delegationStatus',!!s.delegation);text('delegationStatus',s.delegation?({running:'Delegation läuft',proposed:'Vorschlag bereit',verified:'Real geprüft',stopped:'Gestoppt',needs_attention:'Gemeinsam klären'})[s.delegation.status]+': '+s.delegation.reason:'');
  el('proposePatch').disabled=blocked||!s.scope||s.contract?.mode==='explore';el('discussWork').disabled=blocked;
  show('contractConflict',!!s.contractConflict);text('contractConflict',s.contractConflict?'Vor Anwendung gestoppt: '+s.contractConflict.reason+' Besprecht eine kleinere Alternative oder vereinbart einen neuen Contract.':'');
  const scopeSuggestion=s.scopeSuggestion;show('scopeSuggestionBox',!!scopeSuggestion);el('separateScope').disabled=blocked||!s.changePlan;el('excludeScope').disabled=blocked;
  markdown('scopeSuggestion',scopeSuggestion?`**Grund:** ${scopeSuggestion.reason}\n\n**Betroffen:** ${[...scopeSuggestion.files,...scopeSuggestion.symbols].join(', ')}\n\n**Kleinere Alternative:** ${scopeSuggestion.alternative}\n\n**Risiko:** ${scopeSuggestion.risk}`:'');
  show('verificationBox',applied||!!s.evidence);
  const e=s.evidence;
  text('verificationTitle',e?.status==='passed'?'Die konfigurierte Prüfung war erfolgreich':'Jetzt das Verhalten prüfen');
  text('evidence',e?({running:'Die Prüfung läuft …',passed:'Prüfprogramm erfolgreich beendet. Beurteilt gemeinsam, ob damit das gewünschte Verhalten belegt ist.',failed:'Die Prüfung ist fehlgeschlagen. Prüfe die Ausgabe im Terminal und besprecht die nächste Korrektur.',stale:'Der Code wurde inzwischen geändert. Die Prüfung muss erneut ausgeführt werden.'})[e.status]:'Die Übernahme allein bestätigt noch nicht das gewünschte Verhalten.');
  text('verificationCommand',e?.command || 'Das Prüfprogramm lässt sich in den TANDEM-Einstellungen festlegen.');
  text('verificationScope',e?.workspace?`${e.workspace.file_count} Dateien · Basis ${e.workspace.before_hash.slice(0,12)}${e.workspace.after_hash?' · Danach '+e.workspace.after_hash.slice(0,12):''}\n${e.workspace.coverage}${e.workspace.changed_paths.length?'\nGeänderte Pfade: '+e.workspace.changed_paths.join(', '):''}${e.validation_error?'\n'+e.validation_error:''}`:e?.validation_error||'Der erlaubte Suchraum wird vor und nach dem Prüfprogramm inhaltlich gebunden.');
  el('verify').disabled=blocked||!s.scope;
  show('suggestionBox',!!s.suggestion); markdown('suggestion',s.suggestion?`${s.suggestion.goal}\n\n${s.suggestion.reason}`:'');
}

function render(message) {
  current=message;const s=message.state,blocked=message.busy||message.verifying||s.delegation?.status==='running';
  const job=s.factoryJob,jobStatus=job?.status;
  text('factoryStatus',job?({prepared:'Bereit: lokal bearbeiten oder Job extern weitergeben.',running:'Worker bearbeitet den gebundenen Schritt …',result_ready:'Ergebnis bereit. Als Nächstes gegen aktuellen Code und Contract prüfen.',imported:'Als Ergebnis geprüft. Eine Codeänderung bleibt bis zur Übernahme ein Vorschlag.',stale:'Übergabe veraltet. Mit aktuellem Code und Plan neu vorbereiten.',failed:'Worker konnte den Auftrag nicht abschließen.',stopped:'Worker gestoppt. Für einen neuen Lauf erneut vorbereiten.'})[jobStatus]+(job.error?'\n'+job.error:''):'Noch kein Worker-Job. Speichere den Code und bereite die Übergabe vor.');
  show('prepareFactory',!['prepared','running','result_ready'].includes(jobStatus));el('prepareFactory').disabled=blocked||!s.scope||!s.contract;
  show('runFactory',jobStatus==='prepared');el('runFactory').disabled=blocked;
  show('acceptFactory',jobStatus==='result_ready');el('acceptFactory').disabled=blocked;
  show('openFactory',!!job);el('openFactory').disabled=blocked;
  show('importFactory',['prepared','result_ready','failed'].includes(jobStatus));el('importFactory').disabled=blocked;
  show('factoryCommandBox',!!job);text('factoryCommand',job?.command.join('\n')||'');
  if(sessionKey&&s.sessionId&&sessionKey!==s.sessionId){pendingDecision=undefined;decisionOptionsKey=undefined;el('decisionReplaces').value='';page=intent=selectedStep=dismissedProposal=questionStep=contractVersion=undefined;stepRevision=editorRevision=-1;stepDirty=planDirty=false;cardsKey=sourcesKey=filesKey='';for(const id of ['prompt','planMessage','decisionText','decisionReason','acceptanceReason'])el(id).value='';}
  sessionKey=s.sessionId;show('restoreTask',!!s.archivedSessions?.length);el('newTask').disabled=!!message.verifying;
  if(pendingDecision&&s.decisions?.at(-1)?.text===pendingDecision.text&&s.decisions?.at(-1)?.id!==pendingDecision.previousId){el('decisionText').value='';el('decisionReason').value='';el('decisionReplaces').value='';pendingDecision=undefined;}
  text('delegationCommand','Prüfprogramm: '+(message.verificationCommand||'Noch nicht festgelegt'));
  text('historySummary',`${s.planHistory?.length||0} frühere Planstände · ${s.contractHistory?.length||0} frühere Contracts · ${s.simulationHistory?.length||0} frühere Szenarien`);
  text('pilotMetrics',message.metrics?`${message.metrics.observed_host_scope_rejections} beobachtete Contract-Abweisungen · ${message.metrics.applied_patches} übernommene Patches`:'');
  text('evaluationCount',s.evaluations?.length?`${s.evaluations.length} eigene Einschätzungen lokal gespeichert`:'');
  if(!page) { el('repositoryTask').value=s.analysisTask||'';go(s.exploration||s.changePlan?'plan':s.scope?'work':'task'); }
  if(intent && !blocked) {
    const done = intent.action==='exploreRepository'||intent.action==='discussRepository' ? s.exploration?.created_at!==intent.analysis
      : intent.action==='adoptChangePlan'||intent.action==='saveChangePlan' ? s.changePlan?.revision!==intent.revision
      : intent.action==='usePlanStep' ? !!s.activePlanStepId : intent.action==='capture' ? s.scope!==intent.scope
      : intent.action==='apply' ? s.proposal?.status==='applied' : intent.action==='send' ? true : intent.action==='revalidateAnalysis';
    if(done) {go(intent.target);if(intent.action==='saveChangePlan')show('planEditor',false);intent=undefined;}
  }
  text('provider',message.provider==='demo'?'Offline-Demo':'AgentKit · '+(message.provider==='auto'?'konfiguriertes Modell':message.provider));
  show('activity',blocked);text('activityText',message.verifying?'Prüfung läuft …':message.stream?.startsWith('Leseanalyse:')?'TANDEM untersucht passenden Code und Tests …':'TANDEM arbeitet an deiner Anfrage …');
  show('pausePreview',!!message.canPausePreview&&!!message.busy);
  show('error',!!message.error);text('error',message.error);
  el('exploreRepository').disabled=blocked;show('continuePlan',!!s.exploration||!!s.changePlan);
  renderPlan(s,blocked);renderWork(s,blocked);
  show('supportArea',!!s.exploration||s.chat.length>0);
  markdown('analysis',s.exploration?.answer);
  show('analysisDetail',!!s.exploration);el('sourcesSummary').parentElement.hidden=!s.exploration;
  text('warnings',[...new Set(s.exploration?.warnings||[])].join('\n'));
  text('sourcesSummary',`Quellen und Projektregeln${s.exploration?' · '+s.exploration.sources.length+' Belege':''}`);
  const key=s.exploration?.created_at;
  if(sourcesKey!==key) {
    sourcesKey=key;el('repositorySources').replaceChildren();
    (s.exploration?.sources||[]).forEach((source,index)=>{const button=document.createElement('button');button.textContent=`${basename(source.path)}:${source.start_line}`;button.title=source.path;const small=document.createElement('small');small.textContent=source.kind==='instruction'?'Projektregel':source.kind==='test'?'Testquelle · nicht ausgeführt':'Gelesener Code';button.append(small);button.onclick=()=>post('openSource',{index});el('repositorySources').append(button);});
  }
  el('chat').replaceChildren();
  for(const item of s.chat.filter(item=>item.text!==s.exploration?.answer && item.text!==`Repository-Aufgabe: ${s.analysisTask}`)) {
    const article=document.createElement('article');const label=document.createElement('strong');label.textContent=item.role==='user'?'Du':'TANDEM';const body=document.createElement('div');body.className='markdown';if(item.role==='assistant')markdown(body,item.text);else body.textContent=item.text;article.append(label,body);el('chat').append(article);
  }
  el('audit').replaceChildren();for(const item of s.audit.slice(-20)){const p=document.createElement('p');p.textContent=new Date(item.at).toLocaleTimeString('de-DE',{hour:'2-digit',minute:'2-digit'})+' · '+item.action;el('audit').append(p);}
}
window.addEventListener('message',({data:message})=>{if(message.type==='error'){intent=undefined;fail(message.text);}else if(message.type==='state')render(message);});
post('ready');post('repositoryReady');
