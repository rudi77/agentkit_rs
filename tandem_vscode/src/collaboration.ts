import { randomUUID } from 'node:crypto';
import { invalidateFactoryJob } from './factory';
import { ChangePlan, ChangeStep, Decision, SharedChangePlan, State, StepDetails, StepStatus } from './model';

export const emptyDetails = (): StepDetails => ({ needs_now:'', not_now:'', smallest_change:'', reuse:'', invariants:'', assumptions:'', risks:'' });

export function activeStep(state: State): ChangeStep | undefined {
  return state.changePlan?.steps.find(step => step.id === state.activePlanStepId);
}

export function rememberPlan(state: State, by: 'human' | 'agent', reason: string): void {
  if (!state.changePlan) { return; }
  (state.planHistory ??= []).push({ at:new Date().toISOString(), by, reason, plan:structuredClone(state.changePlan) });
}

// Ein neuer Vertrag darf niemals einen alten Patch nachträglich legitimieren.
export function invalidateWork(state: State): void {
  invalidateFactoryJob(state,'Plan oder gemeinsame Entscheidung wurde geändert.');
  if (state.simulation) { state.simulation.stale = true; }
  if (state.proposal?.status === 'ready') { state.proposal.status = 'stale'; }
  if (state.evidence) { state.evidence.status = 'stale'; }
  state.suggestion = undefined;
  const step = activeStep(state);
  if (step && ['preview','applied','verified','accepted'].includes(step.status ?? '')) { step.status = 'clarifying'; }
}

export function addDecision(state: State, input: Pick<Decision,'kind'|'text'|'reason'> & { supersedes?: string }): Decision {
  if (!['need','exclude','decision','assumption','risk','test_idea'].includes(input.kind) || typeof input.text !== 'string' || !input.text.trim() || typeof input.reason !== 'string' || !input.reason.trim()) {
    throw new Error('Bitte Erkenntnis, Begründung und einen gültigen Typ angeben.');
  }
  if (input.text.length > 16_000 || input.reason.length > 16_000) { throw new Error('Die Erkenntnis ist zu lang.'); }
  const plan = state.changePlan;
  if (!state.plan.goal.trim()) { throw new Error('Bitte zuerst ein gemeinsames Schrittziel festlegen.'); }
  if (input.supersedes && !state.decisions?.some(item => item.id === input.supersedes)) { throw new Error('Die zu ersetzende Entscheidung existiert nicht.'); }
  const step = activeStep(state);
  const previous=input.supersedes?state.decisions?.find(item=>item.id===input.supersedes):undefined;
  if(previous&&previous.step_id!==step?.id)throw new Error('Bitte zum Ersetzen den ursprünglichen Planschritt wählen.');
  if(previous&&state.decisions?.some(item=>item.supersedes===previous.id))throw new Error('Diese Erkenntnis wurde bereits ersetzt. Bitte die aktuelle Erkenntnis wählen.');
  rememberPlan(state,'human',previous?'Erkenntnis ersetzt':'Erkenntnis festgehalten');
  if(previous){
    const field=({need:'needs_now',exclude:'not_now',assumption:'assumptions',risk:'risks'} as const)[previous.kind as 'need'|'exclude'|'assumption'|'risk'];
    const without=(value:string)=>{
      const lines=value.split('\n'),old=previous.text.split('\n');
      // Die Entscheidung kann mehrzeilig sein. Entferne genau ihren einen
      // Textblock; gleichlautende andere Festlegungen bleiben bestehen.
      for(let i=lines.length-old.length;i>=0;i--)if(lines.slice(i,i+old.length).join('\n')===previous.text){lines.splice(i,old.length);break;}
      return lines.join('\n');
    };
    if(step?.details&&field)step.details[field]=without(step.details[field]);
    else if(previous.kind==='exclude'){if(plan)plan.not_now=without(plan.not_now);else state.plan.not_now=without(state.plan.not_now);}
    if(previous.kind==='test_idea'){if(step)step.acceptance=without(step.acceptance);else state.plan.acceptance=without(state.plan.acceptance);}
  }
  const decision: Decision = { id:randomUUID(), at:new Date().toISOString(), by:'human', step_id:step?.id, kind:input.kind,
    text:input.text.trim(), reason:input.reason.trim(), plan_revision:plan?++plan.revision:state.plan.revision+1, supersedes:input.supersedes };
  (state.decisions ??= []).push(decision);
  const append = (old: string) => old ? old + '\n' + decision.text : decision.text;
  if (step) {
    step.details ??= emptyDetails();
    const field = ({need:'needs_now',exclude:'not_now',assumption:'assumptions',risk:'risks'} as const)[input.kind as 'need'|'exclude'|'assumption'|'risk'];
    if (field) { step.details[field] = append(step.details[field]); }
    if (input.kind === 'test_idea') { step.acceptance = append(step.acceptance); }
  } else if (input.kind === 'exclude') { if(plan)plan.not_now = append(plan.not_now);else state.plan.not_now=append(state.plan.not_now); }
  if(!step&&input.kind==='test_idea')state.plan.acceptance=append(state.plan.acceptance);
  if(step&&plan){state.plan.not_now=[plan.not_now,step.details?.not_now].filter(Boolean).join('\n');state.plan.acceptance=step.acceptance;}
  state.plan.revision++;
  invalidateWork(state);
  return decision;
}

export function reshapePlan(plan: SharedChangePlan, id: string, operation: 'split'|'merge'|'defer'): ChangePlan {
  const next = structuredClone(plan), index = next.steps.findIndex(step => step.id === id);
  if (index < 0) { throw new Error('Der Schritt existiert nicht mehr.'); }
  const step = next.steps[index];
  if (operation === 'defer') { step.status = 'deferred'; return next; }
  if (operation === 'split') {
    if (next.steps.length >= 12) { throw new Error('Bitte zuerst einen Schritt abschließen oder zusammenführen.'); }
    // Ein geteilter Schritt ist wieder offen; kein Abschlussbeleg wird dupliziert.
    step.status = 'clarifying';
    next.steps.splice(index+1,0,{...structuredClone(step),id:'human-'+randomUUID(),title:step.title+' · zweiter Teil',status:'clarifying'});
  } else if (operation === 'merge') {
    const following = next.steps[index+1];
    if (!following) { throw new Error('Es gibt keinen nachfolgenden Schritt zum Zusammenführen.'); }
    step.title += ' / ' + following.title;
    step.why += '\n' + following.why;
    step.acceptance += '\n' + following.acceptance;
    step.files = [...new Set([...step.files,...following.files])];
    step.source_ids = [...new Set([...step.source_ids,...following.source_ids])];
    const details = emptyDetails();
    for (const key of Object.keys(details) as (keyof StepDetails)[]) { details[key] = [step.details?.[key],following.details?.[key]].filter(Boolean).join('\n'); }
    step.details = details; step.status = 'clarifying';
    next.steps.splice(index+1,1);
  } else { throw new Error('Unbekannte Planoperation.'); }
  return next;
}

export function acceptStep(state: State, kind: 'behavior'|'clarification', rationale: string): void {
  const step = activeStep(state);
  if (!step || !state.changePlan || state.changePlan.stale) { throw new Error('Bitte einen aktuellen Planschritt auswählen.'); }
  if (!['behavior','clarification'].includes(kind) || typeof rationale !== 'string' || !rationale.trim()) { throw new Error('Bitte den fachlichen Abschluss begründen.'); }
  if (kind === 'behavior' && (state.evidence?.status !== 'passed' || !state.scope || state.evidence.uri !== state.scope.uri)) { throw new Error('Eine Verhaltensänderung benötigt eine aktuelle, bestandene reale Prüfung.'); }
  if(kind==='behavior'&&state.evidence?.plan_revision!==undefined&&state.evidence.plan_revision!==state.plan.revision)throw new Error('Die reale Prüfung gehört zu einer älteren Planrevision.');
  if (kind === 'clarification' && state.proposal?.status === 'applied') { throw new Error('Übernommene Codeänderungen benötigen eine reale Prüfung und einen Verhaltensabschluss.'); }
  rememberPlan(state,'human','Fachlicher Abschluss');
  step.status = 'accepted';
  (state.acceptances ??= []).push({ step_id:step.id,at:new Date().toISOString(),plan_revision:state.changePlan.revision,
    rationale:rationale.trim(),kind,evidence_at:kind==='behavior'?state.evidence?.at:undefined });
}

export const statusLabel: Record<StepStatus,string> = {idea:'Idee',clarifying:'In Klärung',ready:'Bereit',preview:'Vorschlag',applied:'Übernommen',verified:'Real geprüft',accepted:'Fachlich abgeschlossen',deferred:'Zurückgestellt'};
