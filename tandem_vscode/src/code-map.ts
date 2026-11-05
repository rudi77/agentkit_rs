import * as vscode from 'vscode';
import { State } from './model';
import { createHash } from 'node:crypto';
import type { CodeContext, CodeEndpoint } from './software-graph';

export class MapNode extends vscode.TreeItem {
  children:MapNode[]=[];
  constructor(label:string,public readonly provenance:'analysis'|'intent'|'catalog'|'unknown'|'documented'|'hypothesis',public readonly uri?:vscode.Uri,range?:vscode.Range) {
    super(label);this.description=({analysis:'Sprachdienst',intent:'Gemeinsamer Plan',catalog:'Gelesene Quellen · unvollständig',unknown:'Nicht verfügbar',documented:'Importierte Dokumentation',hypothesis:'Hypothese · nicht belegt'})[provenance];
    this.tooltip=label+'\nHerkunft: '+this.description;
    if(uri)this.command={command:'tandem.mapNavigate',title:'Code öffnen',arguments:[uri.toString(),range?.start.line??0,range?.start.character??0]};
  }
  group(children:MapNode[]):this{this.children=children;this.collapsibleState=vscode.TreeItemCollapsibleState.Expanded;return this;}
}

export class CodeMap implements vscode.TreeDataProvider<MapNode>,vscode.Disposable {
  private readonly changes=new vscode.EventEmitter<MapNode|undefined>();readonly onDidChangeTreeData=this.changes.event;
  private roots:MapNode[]=[];private generation=0;private timer?:ReturnType<typeof setTimeout>;
  private pendingFocus?:{key:string;promise:Promise<void>};
  private lastFileRoots:MapNode[]=[];
  private context?:CodeContext;
  private readonly subscriptions:vscode.Disposable[]=[];
  constructor(private readonly state:()=>State,private readonly check:(uri:vscode.Uri)=>Promise<void>) {
    this.subscriptions.push(vscode.window.onDidChangeTextEditorSelection(event=>this.schedule(event.textEditor)),vscode.window.onDidChangeActiveTextEditor(editor=>{if(editor)this.schedule(editor);}),vscode.workspace.onDidChangeTextDocument(event=>{const editor=vscode.window.activeTextEditor;if(editor?.document.uri.toString()===event.document.uri.toString())this.schedule(editor);}));
  }
  getTreeItem(node:MapNode):vscode.TreeItem{return node;}
  getChildren(node?:MapNode):MapNode[]{return node?.children??this.roots;}
  getContext():CodeContext|undefined{return this.context;}
  schedule(editor:vscode.TextEditor):void {clearTimeout(this.timer);if(this.pendingFocus?.key===editorKey(editor))return;this.timer=setTimeout(()=>void this.focus(editor).catch(()=>undefined),200);}
  private async bounded<T>(command:string,...args:unknown[]):Promise<T|undefined>{let timeout:ReturnType<typeof setTimeout>|undefined;try{return await Promise.race([vscode.commands.executeCommand<T>(command,...args),new Promise<undefined>(resolve=>{timeout=setTimeout(()=>resolve(undefined),1500);})]);}catch{return undefined;}finally{clearTimeout(timeout);}}
  async focus(editor:vscode.TextEditor):Promise<void> {
    clearTimeout(this.timer);
    const key=editorKey(editor);
    if(this.pendingFocus?.key===key)return this.pendingFocus.promise;
    // Auswahlereignis und explizites Aktualisieren können denselben Snapshot
    // gleichzeitig anfragen. Beide warten auf dieselbe vollständige Analyse.
    const promise=this.loadFocus(editor);this.pendingFocus={key,promise};
    try{await promise;}finally{if(this.pendingFocus?.promise===promise)this.pendingFocus=undefined;}
  }
  private async loadFocus(editor:vscode.TextEditor):Promise<void> {
    if(editor.document.uri.scheme!=='file'){this.generation++;if(this.context)this.context={...this.context,pending:false,note:'Entwurf/Vorschau geöffnet. Die Beziehungen gehören zum letzten echten Code-Snapshot.'};this.roots=[new MapNode('Entwurf/Vorschau · Beziehungen unten gehören zum letzten echten Codekontext.','unknown'),...this.lastFileRoots];this.changes.fire(undefined);return;}
    const generation=++this.generation,doc=editor.document,version=doc.version,selection=editor.selection.active;
    if(this.context)this.context={...this.context,pending:true,note:undefined};
    this.roots=[new MapNode(`Codekontext wird ermittelt · ${vscode.workspace.asRelativePath(doc.uri)}`,'unknown')];this.changes.fire(undefined);
    try{await this.check(doc.uri);}catch{if(generation===this.generation){this.context=undefined;this.roots=[new MapNode('Diese Datei ist außerhalb der freigegebenen Projektquellen.','unknown')];this.changes.fire(undefined);}return;}
    const symbols=await this.bounded<(vscode.DocumentSymbol|vscode.SymbolInformation)[]>('vscode.executeDocumentSymbolProvider',doc.uri);
    const matches:{name:string;range:vscode.Range;focus:vscode.Position}[]=[];
    const visit=(symbol:vscode.DocumentSymbol|vscode.SymbolInformation)=>{const range='range'in symbol?symbol.range:symbol.location.range;if(range.contains(selection))matches.push({name:symbol.name,range,focus:'selectionRange'in symbol?symbol.selectionRange.start:range.start});if('children'in symbol)symbol.children.forEach(visit);};
    symbols?.forEach(visit);matches.sort((a,b)=>doc.offsetAt(a.range.end)-doc.offsetAt(a.range.start)-(doc.offsetAt(b.range.end)-doc.offsetAt(b.range.start)));
    const selected=matches[0],position=selected?.focus??selection;
    const [references,items]=await Promise.all([this.bounded<vscode.Location[]>('vscode.executeReferenceProvider',doc.uri,position),this.bounded<vscode.CallHierarchyItem[]>('vscode.prepareCallHierarchy',doc.uri,position)]);
    const [incoming,outgoing]=items?.length?await Promise.all([this.bounded<vscode.CallHierarchyIncomingCall[]>('vscode.provideIncomingCalls',items[0]),this.bounded<vscode.CallHierarchyOutgoingCall[]>('vscode.provideOutgoingCalls',items[0])]):[undefined,undefined];
    if(generation!==this.generation||doc.version!==version)return;
    const state=this.state(),folder=vscode.workspace.getWorkspaceFolder(doc.uri);
    const local=(uri:vscode.Uri)=>uri.scheme==='file'&&!!vscode.workspace.getWorkspaceFolder(uri);
    const location=(label:string,uri:vscode.Uri,range:vscode.Range)=>new MapNode(label,'analysis',uri,range);
    const calls=(list:{name:string;uri:vscode.Uri;selectionRange:vscode.Range}[]|undefined)=>list?.filter(item=>local(item.uri)).slice(0,40).map(item=>location(item.name,item.uri,item.selectionRange))??[];
    const callers=calls(incoming?.map(call=>call.from)),callees=calls(outgoing?.map(call=>call.to));
    const refs=(references??[]).filter(ref=>local(ref.uri)).slice(0,60).map(ref=>location(`${vscode.workspace.asRelativePath(ref.uri)}:${ref.range.start.line+1}`,ref.uri,ref.range));
    const tests=refs.filter(node=>/test|spec/i.test(node.uri?.path??''));
    const endpoint=(name:string,uri:vscode.Uri,range:vscode.Range):CodeEndpoint=>({name,uri:uri.toString(),path:vscode.workspace.asRelativePath(uri),line:range.start.line+1,character:range.start.character});
    const endpoints=(list:{name:string;uri:vscode.Uri;selectionRange:vscode.Range}[]|undefined)=>list?.filter(item=>local(item.uri)).slice(0,40).map(item=>endpoint(item.name,item.uri,item.selectionRange))??[];
    this.context={focus:endpoint(selected?.name||pathLabel(doc.uri,position),doc.uri,selected?new vscode.Range(selected.focus,selected.focus):new vscode.Range(position,position)),version,sha256:createHash('sha256').update(doc.getText()).digest('hex'),
      callers:endpoints(incoming?.map(call=>call.from)),callees:endpoints(outgoing?.map(call=>call.to)),references:(references??[]).filter(ref=>local(ref.uri)).slice(0,60).map(ref=>endpoint(`${vscode.workspace.asRelativePath(ref.uri)}:${ref.range.start.line+1}`,ref.uri,ref.range)),
      incoming_available:!!incoming,outgoing_available:!!outgoing,references_available:!!references};
    const sourceUri=(file:string,root?:string)=>root?vscode.Uri.joinPath(vscode.Uri.parse(root),...file.split('/')):folder?vscode.Uri.joinPath(folder.uri,...file.split('/')):undefined;
    const product=new MapNode(state.changePlan?.goal||'Noch kein gemeinsames Produktziel','intent').group((state.changePlan?.steps??[]).map(step=>new MapNode(step.title,'intent').group(step.files.map(file=>new MapNode(file,'intent',sourceUri(file,state.changePlan?.root_uri))))));
    const groups=new Map<string,MapNode[]>();for(const source of state.exploration?.sources??[]){const directory=source.path.includes('/')?source.path.slice(0,source.path.lastIndexOf('/')):'.';const list=groups.get(directory)??[];list.push(new MapNode(source.path,'catalog',sourceUri(source.path,state.exploration?.root_uri),new vscode.Range(source.start_line-1,0,source.start_line-1,0)));groups.set(directory,list);}
    const architecture=state.architecture;
    const imported=architecture?[new MapNode(architecture.stale?'Importierte Architektur · Quellen geändert':'Importierte Architektur','documented').group(architecture.nodes.map(node=>{
      const uri=vscode.Uri.joinPath(vscode.Uri.parse(architecture.root_uri),...node.path.split('/'));
      const children=architecture.edges.filter(edge=>edge.from===node.id).map(edge=>{const target=architecture.nodes.find(item=>item.id===edge.to)!;const child=new MapNode('→ '+target.label,edge.kind,vscode.Uri.joinPath(vscode.Uri.parse(architecture.root_uri),...target.path.split('/')),new vscode.Range(target.line-1,0,target.line-1,0));child.tooltip=edge.reason+'\nQuelle: '+architecture.source_uri+(architecture.stale?'\nQuellenstand geändert; neu prüfen.':'');return child;});
      return new MapNode(node.label,'documented',uri,new vscode.Range(node.line-1,0,node.line-1,0)).group(children);
    }))]:[];
    const unavailable=(label:string,nodes:MapNode[],available:boolean|undefined)=>new MapNode(label,available?'analysis':'unknown').group(nodes.length?nodes:[new MapNode(available?'Keine lokalen Beziehungen gemeldet · keine Vollständigkeitsgarantie':'Sprachdienst liefert hierzu keine Daten.','unknown')]);
    this.roots=[new MapNode('Produkt und Arbeitsschritte','intent').group([product]),new MapNode('Architektur · gelesener Ausschnitt','catalog').group([...imported,...[...groups].map(([name,children])=>new MapNode(name,'catalog').group(children))]),
      new MapNode(selected?.name||`${doc.fileName.split(/[\\/]/).pop()}:${position.line+1}`,selected?'analysis':'unknown',doc.uri,selected?.range).group([
        unavailable('Aufrufer',callers,!!incoming),unavailable('Aufgerufene Methoden',callees,!!outgoing),unavailable('Referenzen / mögliche Auswirkung',refs,!!references),unavailable('Testkandidaten nach Dateiname · nicht ausgeführt',tests,!!references)])];
    if(state.proposal?.status==='ready'){
      const proposal=state.proposal,start=proposal.base.prefix.split('\n').length;
      this.roots.push(new MapNode('Patchauswirkung · vor Übernahme','intent',vscode.Uri.parse(proposal.base.uri),new vscode.Range(start-1,0,start-1,0)).group([
        new MapNode(`${proposal.patch.old_text.split('\n').length} alte / ${proposal.patch.new_text.split('\n').length} neue Zeilen in ${proposal.patch.path}`,'intent'),
        new MapNode('Nachgelagerte Verhaltenswirkung durch Tests und Review belegen.','unknown')]));
    }
    this.lastFileRoots=this.roots;this.changes.fire(undefined);
  }
  dispose():void{clearTimeout(this.timer);this.generation++;this.changes.dispose();this.subscriptions.forEach(item=>item.dispose());}
}

function pathLabel(uri:vscode.Uri,position:vscode.Position):string{return`${vscode.workspace.asRelativePath(uri)}:${position.line+1}`;}
function editorKey(editor:vscode.TextEditor):string{return`${editor.document.uri}:${editor.document.version}:${editor.selection.active.line}:${editor.selection.active.character}`;}
