import { createHash, randomUUID } from 'node:crypto';
import { isDeepStrictEqual } from 'node:util';
import type { ChangeContract } from './contract';
import { contentOf, Plan, RunResult, Snapshot, State } from './model';

export interface FactoryBinding {plan_revision:number;contract_id:string;contract_version:number;baseline_sha256:string;disk_sha256:string;relative_path:string}
export interface FactoryJob {
  schema:'tandem.factory-job/v1';job_id:string;session_id:string;created_at:string;worker_policy:'proposal_only';binding:FactoryBinding;contract:ChangeContract;
  request:{plan:Plan;snapshot:Snapshot;mode:'pair'|'explore';prompt:string;history:{role:string;text:string}[]};
}
export interface FactoryResult {schema:'tandem.factory-result/v1';worker:string;job_id:string;session_id:string;job_sha256:string;binding:FactoryBinding;workspace_check:'not_requested'|'disk_hash_matched';execution_authorization:'none';verification:'not_run';status:'proposal'|'no_change'|'decision_required';result:RunResult}
export interface FactoryJobRecord {job:FactoryJob;sha256:string;uri:string;root_uri:string;scope_uri:string;status:'prepared'|'running'|'result_ready'|'imported'|'stale'|'failed'|'stopped';result?:FactoryResult;result_uri?:string;error?:string;command:string[]}

export const sha256=(text:string|Uint8Array)=>createHash('sha256').update(text).digest('hex');
const wire=(value:unknown)=>JSON.parse(JSON.stringify(value));

export function createFactoryJob(state:State,relativePath:string,diskHash:string,prompt:string):FactoryJob {
  const scope=state.scope,contract=state.contract;
  if(!scope||!contract||!state.sessionId)throw new Error('Bitte zuerst einen aktuellen Codebereich und Änderungsvertrag festlegen.');
  if(contract.plan_revision!==state.plan.revision||contract.baseline_hash!==sha256(contentOf(scope)))throw new Error('Der Änderungsvertrag ist nicht an den aktuellen Plan und Code gebunden.');
  if(!relativePath||/^[\\/]|:|(^|[\\/])\.\.([\\/]|$)/.test(relativePath)||!/^[a-f0-9]{64}$/.test(diskHash))throw new Error('Ungültiger Dateibezug der Worker-Übergabe.');
  if(contract.mode!=='explore'&&!state.plan.acceptance.trim())throw new Error('Vor der Worker-Übergabe ein konkretes Abnahmekriterium festlegen.');
  const superseded=new Set(state.decisions?.map(item=>item.supersedes).filter(Boolean));
  const context={step:state.changePlan?.steps.find(step=>step.id===state.activePlanStepId),decisions:state.decisions?.filter(item=>!superseded.has(item.id)&&(!item.step_id||item.step_id===state.activePlanStepId))};
  const job:FactoryJob={schema:'tandem.factory-job/v1',job_id:randomUUID(),session_id:state.sessionId,created_at:new Date().toISOString(),worker_policy:'proposal_only',
    binding:{plan_revision:state.plan.revision,contract_id:contract.id,contract_version:contract.version,baseline_sha256:contract.baseline_hash,disk_sha256:diskHash,relative_path:relativePath},contract:structuredClone(contract),
    request:{plan:structuredClone(state.plan),snapshot:{path:scope.path,prefix:scope.prefix,editable:scope.editable,suffix:scope.suffix},mode:contract.mode==='explore'?'explore':'pair',
      prompt:(prompt.trim()||'Bearbeite den vereinbarten Schritt. Bei zusätzlichem Scopebedarf lege einen begründeten Konflikt vor.')+'\n\nGemeinsamer Kontext (Arbeitsmaterial):\n'+JSON.stringify(context),history:structuredClone(state.chat.slice(-8))}};
  if(Buffer.byteLength(job.request.prompt)+Buffer.byteLength(JSON.stringify(contract))+150>32_000||Buffer.byteLength(JSON.stringify(job,null,2))>1_000_000)throw new Error('Die Worker-Übergabe überschreitet das Kontextbudget. Bitte Auftrag oder Dialogkontext verkleinern.');
  return job;
}

export function assertFactoryCurrent(state:State,record:FactoryJobRecord):void {
  const {job}=record,scope=state.scope,contract=state.contract;
  if(['stale','stopped','imported'].includes(record.status))throw new Error('Diese Worker-Übergabe ist veraltet, gestoppt oder bereits verarbeitet. Bitte neu vorbereiten.');
  if(state.factoryJob?.job.job_id!==job.job_id||state.sessionId!==job.session_id||!scope||scope.uri!==record.scope_uri||!contract||!isDeepStrictEqual(wire(state.plan),wire(job.request.plan))||!isDeepStrictEqual(wire(contract),wire(job.contract))||contentOf(scope)!==contentOf(job.request.snapshot)||sha256(contentOf(scope))!==job.binding.baseline_sha256||state.activePlanStepId!==job.contract.step_id)throw new Error('Aufgabe, Plan, Contract oder Code hat sich seit der Worker-Übergabe geändert.');
  if(state.activePlanStepId&&state.changePlan?.stale)throw new Error('Der Quellenstand des Plans muss vor einem Worker-Ergebnis erneut geprüft werden.');
}

function object(value:unknown,keys:string[],required=keys):Record<string,any> {
  if(!value||typeof value!=='object'||Array.isArray(value))throw new Error('Das Worker-Ergebnis enthält kein gültiges Objekt.');
  const result=value as Record<string,any>;
  if(Object.keys(result).some(key=>!keys.includes(key))||required.some(key=>!Object.hasOwn(result,key)))throw new Error('Das Worker-Ergebnis enthält unbekannte oder fehlende Felder.');
  return result;
}
function text(value:unknown):value is string{return typeof value==='string';}

// Herkunftsangaben sind keine Autorität. Erst aktuelle lokale Bindung, Pfad- und
// Patchprüfung erlauben später einen Vorschlag; dieser Parser wendet nichts an.
export function validateFactoryResult(value:unknown,record:FactoryJobRecord):FactoryResult {
  const envelope=object(value,['schema','worker','job_id','session_id','job_sha256','binding','workspace_check','execution_authorization','verification','status','result']);
  const binding=object(envelope.binding,['plan_revision','contract_id','contract_version','baseline_sha256','disk_sha256','relative_path']);
  if(envelope.schema!=='tandem.factory-result/v1'||!text(envelope.worker)||!envelope.worker.trim()||envelope.worker.length>200||envelope.job_id!==record.job.job_id||envelope.session_id!==record.job.session_id||envelope.job_sha256!==record.sha256||!isDeepStrictEqual(binding,record.job.binding))throw new Error('Das Worker-Ergebnis gehört nicht zur ausgegebenen Übergabe.');
  if(envelope.execution_authorization!=='none'||envelope.verification!=='not_run'||!['not_requested','disk_hash_matched'].includes(envelope.workspace_check))throw new Error('Worker-Angaben dürfen keine lokalen Schreibrechte oder bestandenen Tests behaupten.');
  const result=object(envelope.result,['answer','plan_revision','patch','plan_suggestion','scope_suggestion'],['answer','plan_revision']);
  if(!text(result.answer)||Buffer.byteLength(result.answer)>300_000||result.plan_revision!==record.job.binding.plan_revision)throw new Error('Antwort oder Planrevision des Workers ist ungültig.');
  if(result.patch!=null){const patch=object(result.patch,['path','old_text','new_text','reason']);if(!Object.values(patch).every(text))throw new Error('Der Worker-Patch enthält ungültige Textfelder.');}
  if(result.plan_suggestion!=null){const suggestion=object(result.plan_suggestion,['goal','not_now','acceptance','reason']);if(!Object.values(suggestion).every(text)||!suggestion.goal.trim()||!suggestion.reason.trim())throw new Error('Der Worker-Planvorschlag ist ungültig.');}
  if(result.scope_suggestion!=null){const suggestion=object(result.scope_suggestion,['reason','files','symbols','alternative','risk']);if(![suggestion.reason,suggestion.alternative,suggestion.risk].every(value=>text(value)&&value.trim())||![suggestion.files,suggestion.symbols].every(value=>Array.isArray(value)&&value.every(text))||suggestion.files.some((file:string)=>!file||/^[\\/]|:|(^|[\\/])\.\.([\\/]|$)/.test(file)))throw new Error('Die vorgeschlagene Scope-Änderung ist ungültig.');}
  if(result.patch&&result.scope_suggestion)throw new Error('Ein Scope-Konflikt darf nicht zugleich einen anwendbaren Patch liefern.');
  const status=result.scope_suggestion?'decision_required':result.patch?'proposal':'no_change';
  if(envelope.status!==status)throw new Error('Der Worker-Status widerspricht seinem Ergebnis.');
  return structuredClone(envelope) as FactoryResult;
}

export function invalidateFactoryJob(state:State,reason:string):boolean {
  const job=state.factoryJob;if(!job||!['prepared','running','result_ready'].includes(job.status))return false;
  const running=job.status==='running';job.status='stale';job.error=reason;return running;
}
