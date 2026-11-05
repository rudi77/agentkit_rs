import { State } from './model';

export interface Evaluation {at:string;step_id?:string;cognitive_load:number;comprehension:number;interruption_cost:number;explanation:string;notes:string}
export function saveEvaluation(state:State,input:Omit<Evaluation,'at'|'step_id'>):void {
  if(!input||[input.cognitive_load,input.comprehension,input.interruption_cost].some(value=>!Number.isInteger(value)||value<1||value>7)||typeof input.explanation!=='string'||!input.explanation.trim()||typeof input.notes!=='string')throw new Error('Bitte drei Werte von 1 bis 7 und eine eigene Erklärung der Änderung angeben.');
  (state.evaluations??=[]).push({...input,at:new Date().toISOString(),step_id:state.activePlanStepId});
}
export function pilotMetrics(state:State) {
  const started=state.audit[0]?.at,verified=state.audit.find(item=>item.action.includes('Prüfprogramm beendet: passed'))?.at;
  return{started_at:started,first_verified_at:verified,time_to_first_verified_ms:started&&verified?Math.max(0,Date.parse(verified)-Date.parse(started)):undefined,
    observed_host_scope_rejections:state.audit.filter(item=>item.action.startsWith('Contract-Abweichung')).length,
    proposed_scope_changes:state.audit.filter(item=>item.action.startsWith('Scope-Erweiterung begründet')).length,
    applied_patches:state.audit.filter(item=>item.action.startsWith('Patch im Editor übernommen')).length,
    stopped_runs:state.audit.filter(item=>item.action==='Agentenlauf gestoppt').length,
    evaluations:state.evaluations??[],coverage:'Lokale Beobachtungen dieses gespeicherten Ablaufs; keine Aussage über nicht beobachtete Aktionen oder Produktivitätsgewinn.'};
}
export function handoffArtifact(state:State,includeCode=false) {
  const artifact=structuredClone(state);
  // Der Empfänger erhält Rechte als Kontext, niemals eine Ausführungsfreigabe.
  // Ohne Code bleiben Hashes und Quellanker zur erneuten lokalen Prüfung erhalten.
  const content=includeCode?artifact:stripSnapshots(artifact);
  return{schema:'tandem.handoff/v1',created_at:new Date().toISOString(),execution_authorization:'none',requires_revalidation:true,
    includes_code:includeCode,metrics:pilotMetrics(state),state:content};
}
function stripSnapshots(state:State):unknown {
  const scope=(value:State['scope'])=>value?{uri:value.uri,path:value.path,version:value.version,omitted:'Code-Snapshot im Metadatenexport ausgelassen'}:undefined;
  const proposal=(value:State['proposal'])=>value?{revision:value.revision,status:value.status,path:value.patch.path,reason:value.patch.reason,base:scope(value.base)}:undefined;
  const evidence=(value:State['evidence'])=>value?{...value,content:undefined}:undefined;
  const simulation=(value:State['simulation'])=>value?{...value,baseline_content:undefined,proposed_content:undefined,baseline:{...value.baseline,steps:value.baseline.steps.map(step=>({...step,text:undefined}))},proposed:value.proposed?{...value.proposed,steps:value.proposed.steps.map(step=>({...step,text:undefined}))}:undefined}:undefined;
  const humanChange=(value:State['humanChange'])=>value?{...value,before:undefined,after:undefined,review:value.review?{...value.review,answer:undefined}:undefined}:undefined;
  // Worker-Jobs enthalten vollständige Code-Snapshots und Dialogkontext.
  // Im Metadatenexport bleiben ausschließlich Identität und Bindung erhalten.
  const factory=(value:State['factoryJob'])=>value?{status:value.status,sha256:value.sha256,job:{schema:value.job.schema,job_id:value.job.job_id,session_id:value.job.session_id,created_at:value.job.created_at,binding:value.job.binding},omitted:'Worker-Auftrag und Ergebnis im Metadatenexport ausgelassen'}:undefined;
  return{...state,chat:undefined,factoryJob:factory(state.factoryJob),factoryHistory:state.factoryHistory?.map(factory),scope:scope(state.scope),proposal:proposal(state.proposal),proposalHistory:state.proposalHistory?.map(proposal),evidence:evidence(state.evidence),evidenceHistory:state.evidenceHistory?.map(evidence),simulation:simulation(state.simulation),simulationHistory:state.simulationHistory?.map(simulation),humanChange:humanChange(state.humanChange),humanChangeHistory:state.humanChangeHistory?.map(humanChange),exploration:state.exploration?{...state.exploration,answer:undefined,overlays:undefined}:undefined};
}
