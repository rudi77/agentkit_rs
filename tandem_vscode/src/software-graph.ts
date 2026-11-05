import { createHash } from 'node:crypto';
import { State } from './model';

export type GraphLayer='product'|'architecture'|'local'|'impact';
export type GraphOrigin='analysis'|'documented'|'hypothesis'|'intent'|'catalog';
export interface CodeAnchor {uri:string;path:string;line:number;character?:number}
export interface CodeEndpoint extends CodeAnchor {name:string}
export interface CodeContext {
  focus:CodeEndpoint;version:number;sha256:string;callers:CodeEndpoint[];callees:CodeEndpoint[];references:CodeEndpoint[];
  incoming_available:boolean;outgoing_available:boolean;references_available:boolean;pending?:boolean;note?:string;
}
export interface GraphNode {id:string;label:string;kind:'intent'|'step'|'component'|'code'|'test_candidate'|'patch';origin:GraphOrigin;anchor?:CodeAnchor;step_id?:string;status?:string;reason:string;stale?:boolean}
export interface GraphEdge {from:string;to:string;origin:GraphOrigin;reason:string}
export interface GraphSlice {nodes:GraphNode[];edges:GraphEdge[];notes:string[];focus_id?:string}
export interface SoftwareGraph {revision:string;context?:CodeContext;layers:Record<GraphLayer,GraphSlice>}

const slice=():GraphSlice=>({nodes:[],edges:[],notes:[]});
function uriFor(root:string,file:string):string {
  const base=new URL(root.endsWith('/')?root:root+'/');
  return new URL(file.split(/[\\/]/).map(encodeURIComponent).join('/'),base).toString();
}
function addNode(view:GraphSlice,node:GraphNode):void {if(!view.nodes.some(item=>item.id===node.id))view.nodes.push(node);}
const anchorId=(anchor:CodeAnchor)=>anchor.uri+':'+anchor.line+':'+(anchor.character??0);
function endpoint(view:GraphSlice,item:CodeEndpoint,kind:GraphNode['kind']='code'):string {
  const id='code:'+anchorId(item);addNode(view,{id,label:item.name,kind,origin:'analysis',anchor:item,reason:kind==='test_candidate'?'Sprachdienst-Referenz; Testkandidat anhand des Dateinamens, noch kein Abdeckungsnachweis.':'Symbol oder Referenz vom Sprachdienst im betrachteten Code-Snapshot.'});return id;
}

export function softwareGraph(state:State,context?:CodeContext):SoftwareGraph {
  const product=slice(),architecture=slice(),local=slice(),impact=slice();
  const plan=state.changePlan;
  if(plan){
    product.focus_id='intent';product.nodes.push({id:'intent',label:plan.goal,kind:'intent',origin:'intent',reason:'Gemeinsam vereinbartes Produktziel, kein automatisch abgeleiteter Codevertrag.',stale:plan.stale});
    for(const step of plan.steps){const id='step:'+step.id;product.nodes.push({id,label:step.title,kind:'step',origin:'intent',reason:step.why,step_id:step.id,status:step.status??'clarifying',stale:plan.stale});product.edges.push({from:'intent',to:id,origin:'intent',reason:'Geplanter Beitrag zum gemeinsamen Ziel.'});
      for(const file of step.files){const anchor={uri:uriFor(plan.root_uri,file),path:file,line:1},fileId='file:'+anchor.uri;addNode(product,{id:fileId,label:file.split(/[\\/]/).pop()!,kind:'code',origin:'intent',anchor,reason:'Im Plan benanntes Codeelement; keine Behauptung einer bereits implementierten Änderung.',stale:plan.stale});product.edges.push({from:id,to:fileId,origin:'intent',reason:'Geplanter Arbeitsbezug, keine symbolische Abhängigkeit.'});}
    }
    if(plan.stale)product.notes.push('Planbasis veraltet. Vor weiteren Änderungen Quellenstand und Contract prüfen.');
  }else product.notes.push('Noch kein gemeinsamer Repository-Plan. Beginne mit einer Aufgabe im TANDEM-Arbeitsplatz.');
  const imported=state.architecture;
  if(imported){
    for(const node of imported.nodes)architecture.nodes.push({id:'architecture:'+node.id,label:node.label,kind:'component',origin:'documented',anchor:{uri:uriFor(imported.root_uri,node.path),path:node.path,line:node.line},reason:'Import aus '+imported.source_uri+' · Dokument '+imported.sha256.slice(0,12),stale:imported.stale});
    for(const edge of imported.edges)architecture.edges.push({from:'architecture:'+edge.from,to:'architecture:'+edge.to,origin:edge.kind,reason:edge.reason});
    architecture.notes.push('Importierter Architekturausschnitt. Dokumentierte Beziehungen und Hypothesen sind getrennt; keine vollständige Systemanalyse.');
    if(imported.stale)architecture.notes.push('Quellen des Architekturimports wurden geändert. Codeanker und Beziehungen erneut prüfen.');
  }else{
    const analysis=state.exploration;
    for(const source of analysis?.sources??[]){if(source.kind==='instruction')continue;const directory=source.path.includes('/')?source.path.slice(0,source.path.lastIndexOf('/')):'.',groupId='directory:'+directory;
      const anchor={uri:uriFor(analysis!.root_uri,source.path),path:source.path,line:source.start_line};
      addNode(architecture,{id:groupId,label:directory,kind:'component',origin:'catalog',anchor,reason:'Dateigruppierung aus tatsächlich gelesenen Quellen. Keine abgeleitete Architekturabhängigkeit.',stale:analysis?.stale});
      const id='source:'+source.id;architecture.nodes.push({id,label:source.path.split('/').pop()!,kind:source.kind==='test'?'test_candidate':'code',origin:'catalog',anchor,reason:'Gelesene Quelle · '+source.sha256.slice(0,12),stale:analysis?.stale});architecture.edges.push({from:groupId,to:id,origin:'catalog',reason:'Enthaltene Datei; keine Aufrufbeziehung.'});
    }
    architecture.notes.push('Gelesener Dateiausschnitt, keine vollständige Architekturkarte. Explizite Beziehungen lassen sich aus .tandem/architecture.json importieren.');
  }
  if(context){
    const focus=endpoint(local,context.focus);local.focus_id=focus;
    for(const caller of context.callers)local.edges.push({from:endpoint(local,caller),to:focus,origin:'analysis',reason:'Aufruf laut Sprachdienst.'});
    for(const callee of context.callees)local.edges.push({from:focus,to:endpoint(local,callee),origin:'analysis',reason:'Aufgerufene Methode laut Sprachdienst.'});
    for(const ref of context.references)local.edges.push({from:endpoint(local,ref,/test|spec/i.test(ref.path)?'test_candidate':'code'),to:focus,origin:'analysis',reason:'Symbolreferenz laut Sprachdienst; nicht zwangsläufig ein Methodenaufruf.'});
    local.notes.push('Direkter Codekontext · Editorversion '+context.version+' · Snapshot '+context.sha256.slice(0,12)+'.');
    if(!context.incoming_available||!context.outgoing_available||!context.references_available)local.notes.push('Einzelne Sprachdienst-Abfragen liefern keine Daten. Fehlende Kanten beweisen keine Unabhängigkeit.');
    if(context.note)local.notes.push(context.note);if(context.pending)local.notes.push('Codefokus wird aktualisiert. Die dargestellten Beziehungen gehören noch zum letzten erfassten Snapshot.');
    const focused=architecture.nodes.filter(node=>node.anchor?.uri===context.focus.uri).sort((a,b)=>Math.abs(a.anchor!.line-context.focus.line)-Math.abs(b.anchor!.line-context.focus.line))[0];architecture.focus_id=focused?.id;
  }else local.notes.push('Wähle im echten Editor eine Methode. TANDEM nutzt die verfügbaren Symbole, Aufrufe und Referenzen des Sprachdiensts.');
  const proposal=state.proposal;
  if(proposal){
    const first=proposal.base.prefix.split('\n').length,last=(proposal.base.prefix+proposal.base.editable).split('\n').length;
    impact.focus_id='patch';impact.nodes.push({id:'patch',label:proposal.patch.path.split(/[\\/]/).pop()!,kind:'patch',origin:'intent',anchor:{uri:proposal.base.uri,path:proposal.patch.path,line:first},reason:proposal.patch.reason,status:proposal.status,stale:proposal.status!=='ready'});
    impact.notes.push(`${proposal.patch.old_text.split('\n').length} entfernte / ${proposal.patch.new_text.split('\n').length} neue Zeilen. Status: ${{ready:'Vorschlag',stale:'Veraltet',applied:'Übernommen',discarded:'Verworfen'}[proposal.status]}.`);
    if(context&&!context.pending&&context.focus.uri===proposal.base.uri&&context.focus.line>=first&&context.focus.line<=last){
      const id=endpoint(impact,context.focus);impact.edges.push({from:'patch',to:id,origin:'intent',reason:'Die ausgewählte Methode liegt im erlaubten Patchbereich.'});
      for(const ref of [...context.callers,...context.references])impact.edges.push({from:id,to:endpoint(impact,ref,/test|spec/i.test(ref.path)?'test_candidate':'code'),origin:'analysis',reason:'Möglicherweise betroffenes Codeelement aufgrund einer Sprachdienst-Beziehung. Keine bewiesene Verhaltensänderung.'});
    }else impact.notes.push('Für mögliche Auswirkungen zuerst eine Methode innerhalb des Patchbereichs im Editor fokussieren.');
    impact.notes.push('Abgeleitete Beziehungen ersetzen keine realen Tests, Contract- und Auswirkungsprüfung.');
  }else impact.notes.push('Noch kein Patch vorhanden. Hier erscheinen der konkrete Eingriff und die belegbaren Nachbarn vor der Übernahme.');
  const layers={product,architecture,local,impact};
  const revision=createHash('sha256').update(JSON.stringify({layers,context})).digest('hex');
  return{revision,layers,context};
}
