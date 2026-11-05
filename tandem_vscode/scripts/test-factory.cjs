const assert=require('node:assert/strict');
const {createFactoryJob,assertFactoryCurrent,validateFactoryResult,sha256,invalidateFactoryJob}=require('../out/factory');
const {bindContract}=require('../out/contract');
const {invalidateWork}=require('../out/collaboration');
const {handoffArtifact}=require('../out/artifact');
const sentinel='PRIVATE_WORKER_CODE_48375';
const state={sessionId:'session',plan:{revision:1,goal:'Randzeichen entfernen',not_now:'Keine neuen Typen',acceptance:'Leerzeichenfall prüfen'},scope:{uri:'file:///repo/Demo.cs',path:'repo/Demo.cs',version:1,prefix:'',editable:`string Clean(string input) { return input; } // ${sentinel}`,suffix:''},chat:[],audit:[]};
bindContract(state);
const job=createFactoryJob(state,'Demo.cs','a'.repeat(64),'Kleinsten Vorschlag erstellen');
const record={job,sha256:sha256(JSON.stringify(job,null,2)),uri:'file:///job.json',root_uri:'file:///repo',scope_uri:state.scope.uri,status:'prepared',command:[]};state.factoryJob=record;
assertFactoryCurrent(state,record);
const envelope={schema:'tandem.factory-result/v1',worker:'test',job_id:job.job_id,session_id:job.session_id,job_sha256:record.sha256,binding:job.binding,workspace_check:'not_requested',execution_authorization:'none',verification:'not_run',status:'proposal',result:{answer:sentinel,plan_revision:1,patch:{path:state.scope.path,old_text:'return input;',new_text:'return input.Trim();',reason:'Randzeichen entfernen'}}};
assert.equal(validateFactoryResult(envelope,record).status,'proposal');
for(const mutate of [x=>x.job_id='other',x=>x.job_sha256='0'.repeat(64),x=>x.binding.contract_version++,x=>x.execution_authorization='apply',x=>x.verification='passed',x=>x.status='no_change',x=>x.result.plan_revision++,x=>x.result.patch.extra='hidden',x=>x.extra=true]){
  const changed=structuredClone(envelope);mutate(changed);assert.throws(()=>validateFactoryResult(changed,record));
}
const result=structuredClone(envelope);result.status='decision_required';delete result.result.patch;result.result.scope_suggestion={reason:'Port fehlt',files:['../secret'],symbols:[],alternative:'Bestehenden Port verwenden',risk:'Vertrag erweitert'};assert.throws(()=>validateFactoryResult(result,record));
record.result=envelope;state.factoryHistory=[structuredClone(record)];
assert.ok(!JSON.stringify(handoffArtifact(state)).includes(sentinel),'Metadaten dürfen keine Worker-Snapshots oder Antworten enthalten');
assert.ok(JSON.stringify(handoffArtifact(state,true)).includes(sentinel));
state.plan.revision++;assert.throws(()=>assertFactoryCurrent(state,record));state.plan.revision--;
invalidateWork(state);assert.equal(record.status,'stale');assert.throws(()=>assertFactoryCurrent(state,record));
record.status='running';assert.equal(invalidateFactoryJob(state,'Code geändert'),true);assert.equal(record.status,'stale');
record.status='prepared';bindContract(state);assert.equal(record.status,'stale');
for(const status of ['stopped','imported','stale']){record.status=status;assert.throws(()=>assertFactoryCurrent(state,record));}
console.log('PASS: Worker-Identität, unveränderte Bindung, eingeschränkte Befugnisse, Statuskonsistenz, Plan-/Contract-Veraltung und Metadaten ohne Worker-Code.');
