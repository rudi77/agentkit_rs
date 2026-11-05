import { createHash, randomUUID } from 'node:crypto';
import { invalidateFactoryJob } from './factory';
import { activeStep } from './collaboration';
import { contentOf, Patch, State } from './model';

export interface ChangeContract {
  id:string; version:number; plan_revision:number; step_id?:string; baseline_hash:string;
  path:string; start_line:number; end_line:number; mode:'explore'|'pair'|'delegate';
  operations:['modify']; preserve_public_signatures:boolean; allow_new_symbols:boolean;
  needs_now:string; not_now:string; invariants:string; acceptance:string;
  permissions:{analysis:'read_only';code_write:'propose_only'|'none'|'bounded_apply';shell:'none'};
}
export interface ContractConflict { at:string; reason:string; path:string; patch_reason:string; plan_revision:number }

export function bindContract(state:State, changes?:Partial<Pick<ChangeContract,'mode'|'preserve_public_signatures'|'allow_new_symbols'|'invariants'>>):ChangeContract|undefined {
  invalidateFactoryJob(state,'Der Änderungsvertrag wurde neu gebunden.');
  const scope=state.scope;if(!scope){if(state.contract)(state.contractHistory??=[]).push(structuredClone(state.contract));state.contract=undefined;return;}
  const old=state.contract,step=activeStep(state),mode=changes?.mode??old?.mode??'pair';
  if(!['pair','explore','delegate'].includes(mode))throw new Error('Unbekannter Vertragsmodus.');
  const contract:ChangeContract={id:old?.id??randomUUID(),version:(old?.version??0)+1,plan_revision:state.plan.revision,
    step_id:state.activePlanStepId,baseline_hash:createHash('sha256').update(contentOf(scope)).digest('hex'),path:scope.path,
    start_line:scope.prefix.split('\n').length,end_line:(scope.prefix+scope.editable).split('\n').length,
    operations:['modify'],mode,preserve_public_signatures:changes?.preserve_public_signatures??old?.preserve_public_signatures??true,
    allow_new_symbols:changes?.allow_new_symbols??old?.allow_new_symbols??false,
    needs_now:step?.details?.needs_now||state.plan.goal,not_now:[...new Set([state.plan.not_now,step?.details?.not_now].filter(Boolean).join('\n').split('\n'))].join('\n'),
    invariants:changes?.invariants??step?.details?.invariants??old?.invariants??'',acceptance:state.plan.acceptance,
    permissions:{analysis:'read_only',code_write:mode==='delegate'?'bounded_apply':mode==='pair'?'propose_only':'none',shell:'none'}};
  if(typeof contract.invariants!=='string'||typeof contract.allow_new_symbols!=='boolean'||typeof contract.preserve_public_signatures!=='boolean')throw new Error('Ungültige Contract-Regeln.');
  if(old)(state.contractHistory??=[]).push(structuredClone(old));
  state.contract=contract;return contract;
}

// Kommentare und Literale dürfen keine Scheinsymbole erzeugen. Diese Prüfung
// ergänzt die harte Regiongrenze; sie ist kein semantischer Korrektheitsbeweis.
function structure(text:string):string {
  return text.replace(/\/\*[\s\S]*?\*\/|\/\/[^\n]*|@"(?:""|[^"])*"|"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'/g,match=>' '.repeat(match.length));
}
function symbols(text:string):string[] {
  const source=structure(text),names:string[]=[];
  for(const match of source.matchAll(/\b(class|struct|interface|record|enum)\s+(\w+)/g))names.push(match[1]+':'+match[2]);
  for(const match of source.matchAll(/\b[\w.<>,?\[\]]+\s+(\w+)\s*\([^;{}]*\)\s*\{/g))names.push('method:'+match[1]);
  return names.sort();
}
function publicSignatures(text:string):string[] {
  return [...structure(text).matchAll(/\b(?:public|protected)\s+[^{};=]+(?=[{};=])/g)].map(match=>match[0].replace(/\s+/g,' ').trim()).sort();
}
export function enforceContract(state:State,patch:Patch):void {
  const contract=state.contract,scope=state.scope;if(!contract||!scope)throw new Error('Bitte einen aktuellen Änderungsvertrag über die Codeauswahl festlegen.');
  if(contract.plan_revision!==state.plan.revision||contract.baseline_hash!==createHash('sha256').update(contentOf(scope)).digest('hex')||contract.path!==patch.path)throw new Error('Der Änderungsvertrag gehört zu einem anderen Plan- oder Codestand.');
  if(contract.mode==='explore'||contract.permissions.code_write==='none')throw new Error('Explore erlaubt keine Änderungsvorschläge. Bitte ausdrücklich zu Pair wechseln.');
  if(!scope.path.toLowerCase().endsWith('.cs'))return;
  const before=contentOf(scope),after=scope.prefix+scope.editable.replace(patch.old_text,()=>patch.new_text)+scope.suffix;
  if(contract.preserve_public_signatures&&JSON.stringify(publicSignatures(before))!==JSON.stringify(publicSignatures(after)))throw new Error('Der Vorschlag verändert eine geschützte öffentliche Signatur. Bitte einen begründeten neuen Contract vereinbaren.');
  if(!contract.allow_new_symbols){const previous=symbols(before);for(const name of symbols(after)){const index=previous.indexOf(name);if(index<0)throw new Error('Der Vorschlag ergänzt ein nicht vereinbartes Symbol: '+name);previous.splice(index,1);}}
}
