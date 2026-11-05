import * as vscode from 'vscode';
import * as path from 'node:path';
import { createHash, randomUUID } from 'node:crypto';
import { AgentClient } from './client';
import { assertCurrent, contentOf, Evidence, Plan, Proposal, RunResult, Scope, State, validatePatch, ChangePlan, Exploration, Overlay, validateChangePlan } from './model';
import { WorkbenchView } from './view';
import { activeStep, addDecision, acceptStep, invalidateWork, rememberPlan, reshapePlan } from './collaboration';
import type { Decision } from './model';
import { simulate, firstDivergence, simulationParameters, Scenario } from './simulation';
import { SimulationView } from './simulation-view';
import { bindContract, enforceContract, ChangeContract } from './contract';
import { CodeMap } from './code-map';
import { handoffArtifact, pilotMetrics, saveEvaluation, Evaluation } from './artifact';
import { validateArchitecture } from './architecture';
import { GraphView } from './graph-view';
import { GraphNode, softwareGraph, SoftwareGraph } from './software-graph';
import { WorkspaceSnapshot, changedWorkspaceFiles } from './verification';
import { assertFactoryCurrent, createFactoryJob, FactoryJobRecord, invalidateFactoryJob, sha256, validateFactoryResult } from './factory';

const STATE_KEY = 'tandem.step.v1';
const PREVIEW_SCHEME = 'tandem-preview';

export class TandemController implements vscode.Disposable {
  private state: State;
  private readonly client: AgentClient;
  private readonly mapClient:AgentClient;
  private readonly output = vscode.window.createOutputChannel('TANDEM');
  private readonly view: WorkbenchView;
  private readonly simulationView: SimulationView;
  private readonly codeMap:CodeMap;
  private readonly graphView:GraphView;
  private codeColumn=vscode.ViewColumn.One;
  private readonly simulationDecoration = vscode.window.createTextEditorDecorationType({isWholeLine:true,backgroundColor:new vscode.ThemeColor('editor.findMatchHighlightBackground'),border:'1px solid',borderColor:new vscode.ThemeColor('editorWarning.foreground')});
  private readonly disposables: vscode.Disposable[] = [];
  private readonly neighborChanges = new vscode.EventEmitter<void>();
  private neighbors: vscode.TreeItem[] = [];
  private previewDocuments = new Map<string, string>();
  private readonly previewChanges=new vscode.EventEmitter<vscode.Uri>();
  private editableDraft?: {uri:string;revision:number;baseline:string};
  private delegationSequence=0;
  private delegationActive=false;
  private delegateVerification?:{program:string;args:string[]};
  private simulationContextScope?:Scope;
  private simulationContext:ReturnType<typeof simulationParameters>={parameters:[],limitation:'Bitte zuerst eine C#-Methode als Arbeitsbereich wählen.'};
  private renderTimer?:ReturnType<typeof setTimeout>;
  private applyingContent?:string;
  private liveCandidate?:Proposal;
  private stopSequence=0;
  private busy = false;
  private verifying = false;
  private stream = '';
  private error = '';
  private generation = 0;
  private verification?: vscode.TaskExecution;
  private persistence: Promise<void> = Promise.resolve();

  constructor(private readonly context: vscode.ExtensionContext, saved?: State) {
    this.state = saved ?? context.workspaceState.get<State>(STATE_KEY) ?? {
      plan: { revision: 1, goal: '', not_now: 'Keine zusätzlichen Dateien, Klassen oder Refactorings.', acceptance: '' },
      chat: [], audit: [],
    };
    this.state.sessionId??=randomUUID();
    invalidateFactoryJob(this.state,'Host wurde neu gestartet; Übergabe bitte neu vorbereiten.');
    // Editorversionen gelten nur innerhalb eines Hosts. Vorschläge nach Reload
    // werden deshalb erneut erstellt; eine alte Prüfung bleibt sichtbar, aber alt.
    if (this.state.proposal?.status === 'ready') { this.state.proposal.status = 'stale'; }
    if (this.state.evidence) { this.state.evidence.status = 'stale'; }
    if (this.state.exploration) { this.state.exploration.stale = true; }
    if (this.state.changePlan) { this.state.changePlan.stale = true; }
    if (this.state.simulation) { this.state.simulation.stale = true; }
    if(this.state.delegation?.status==='running'){this.state.delegation.status='stopped';this.state.delegation.reason='Host wurde neu gestartet; keine automatische Fortsetzung.';}
    if(this.state.architecture)this.state.architecture.stale=true;
    if (this.state.changePlan && !this.state.changePlan.basis_created_at && this.state.exploration) { this.state.changePlan.basis_created_at = this.state.exploration.created_at; }
    this.client = new AgentClient(context.extensionPath, this.output);
    this.mapClient = new AgentClient(context.extensionPath,this.output);
    this.view = new WorkbenchView(message => this.handle(message), context.extensionUri);
    this.simulationView = new SimulationView(context.extensionUri,message=>this.handle(message),()=>this.render());
    this.codeMap=new CodeMap(()=>this.state,async uri=>{const folder=vscode.workspace.getWorkspaceFolder(uri);if(!folder)throw new Error('Kein Projektordner');await this.mapClient.request('check_path',{path:path.relative(folder.uri.fsPath,uri.fsPath).replace(/\\/g,'/')},undefined,folder.uri.fsPath);});
    this.graphView=new GraphView(context.extensionUri,node=>this.openGraphNode(node),id=>this.usePlanStep(id));
    this.disposables.push(
      vscode.window.registerWebviewViewProvider('tandem.workbench', this.view),
      vscode.window.registerWebviewViewProvider('tandem.simulation', this.simulationView),
      vscode.window.registerTreeDataProvider('tandem.map',this.codeMap),
      this.codeMap.onDidChangeTreeData(()=>this.updateGraph()),
      vscode.window.onDidChangeActiveTextEditor(editor=>{if(editor?.document.uri.scheme==='file'&&vscode.workspace.getWorkspaceFolder(editor.document.uri))this.codeColumn=editor.viewColumn??vscode.ViewColumn.One;}),
      vscode.workspace.registerTextDocumentContentProvider(PREVIEW_SCHEME, {
        onDidChange:this.previewChanges.event,
        provideTextDocumentContent: uri => this.previewDocuments.get(uri.toString()) ?? '',
      }),
      vscode.window.registerTreeDataProvider('tandem.neighbors', {
        onDidChangeTreeData: this.neighborChanges.event,
        getTreeItem: item => item,
        getChildren: () => this.neighbors,
      }),
      vscode.workspace.onDidChangeTextDocument(event => {
        this.invalidateRepository(event.document.uri);
        if (event.document.uri.toString() !== this.state.scope?.uri) { return; }
        const before=contentOf(this.state.scope),after=event.document.getText();
        if(this.state.simulation?.uri===event.document.uri.toString()&&this.state.simulation.baseline_content!==after)this.state.simulation.stale=true;
        if(before!==after&&after!==this.applyingContent){
          const previous=this.state.humanChange;
          if(!previous||previous.uri!==event.document.uri.toString())this.record('human','Code direkt im Editor geändert; Agentenvorschläge und Prüfbelege müssen neu bewertet werden');
          this.state.humanChange={at:new Date().toISOString(),uri:event.document.uri.toString(),path:this.state.scope.path,before:previous?.uri===event.document.uri.toString()?previous.before:before,after,
            review:previous?.review?{...previous.review,stale:true}:undefined};
        } else if(before===after&&this.state.humanChange?.uri===event.document.uri.toString()){
          this.state.humanChange.after=after;if(this.state.humanChange.review)this.state.humanChange.review.stale=true;
        }
        if (this.state.proposal?.status === 'ready') { this.state.proposal.status = 'stale'; }
        if (this.state.evidence && this.state.evidence.content !== event.document.getText()) { this.state.evidence.status = 'stale'; }
        void this.persist();
      }),
      vscode.workspace.onDidChangeConfiguration(event => {
        if (event.affectsConfiguration('tandem.provider') || event.affectsConfiguration('tandem.binaryPath')) {
          this.stop();
          this.mapClient.stop();
          this.render();
        }
      }),
    );
    const watcher = vscode.workspace.createFileSystemWatcher('**/*');
    this.disposables.push(watcher, watcher.onDidChange(uri => this.invalidateRepository(uri)), watcher.onDidCreate(uri => this.invalidateRepository(uri)), watcher.onDidDelete(uri => this.invalidateRepository(uri)));
    const commands: Record<string, () => unknown> = {
      'tandem.open': () => vscode.commands.executeCommand('tandem.workbench.focus'),
      'tandem.capture': () => this.capture(),
      'tandem.preview': () => this.preview(),
      'tandem.apply': () => this.apply(),
      'tandem.verify': () => this.verify(),
      'tandem.stop': () => this.stop(),
      'tandem.simulate': () => vscode.commands.executeCommand('tandem.simulation.focus'),
      'tandem.useDraft': () => this.useEditedDraft(),
      'tandem.arrange': () => vscode.commands.executeCommand('workbench.action.moveFocusedView','tandem.workbench'),
      'tandem.refreshMap': () => {const editor=vscode.window.activeTextEditor;if(editor)return this.codeMap.focus(editor);},
      'tandem.export': () => this.exportArtifact(false),
      'tandem.importArchitecture': () => this.importArchitecture(),
      'tandem.newTask': () => this.newTask(),
      'tandem.restoreTask': async()=>{const selected=await vscode.window.showQuickPick((this.state.archivedSessions??[]).map(item=>({label:item.title,description:new Date(item.at).toLocaleString('de-DE'),id:item.id})),{placeHolder:'Welche gespeicherte TANDEM-Aufgabe möchtest du fortsetzen?'});if(selected)return this.restoreTask(selected.id);},
      'tandem.graph': ()=>this.openGraph(),
      'tandem.exploreRepository': () => vscode.commands.executeCommand('tandem.workbench.focus'),
    };
    for (const [name, action] of Object.entries(commands)) {
      this.disposables.push(vscode.commands.registerCommand(name, async () => {
        try { return await action(); }
        catch (error) { this.report(error); }
      }));
    }
    this.disposables.push(vscode.commands.registerCommand('tandem.openRepositorySource', (index: number) => this.openSource(index).catch(error => this.report(error))));
    this.disposables.push(vscode.commands.registerCommand('tandem.mapNavigate',async(uri:string,line:number,character:number)=>{
      const target=vscode.Uri.parse(uri),folder=vscode.workspace.getWorkspaceFolder(target);if(!folder||target.scheme!=='file')throw new Error('Nur lokale Projektquellen können geöffnet werden.');
      await this.mapClient.request('check_path',{path:path.relative(folder.uri.fsPath,target.fsPath).replace(/\\/g,'/')},undefined,folder.uri.fsPath);
      const editor=await vscode.window.showTextDocument(target,{selection:new vscode.Range(line,character,line,character),preview:true});this.codeMap.schedule(editor);
    }));
    this.renderRepositorySources();
    this.render();
  }

  getState(): State { return structuredClone(this.state); }
  isWorkbenchReady(): boolean { return this.view.ready; }
  isRepositoryWorkbenchReady(): boolean { return this.view.repositoryReady; }

  private record(by: 'human' | 'agent' | 'runtime', action: string): void {
    this.state.audit.push({ at: new Date().toISOString(), by, action, revision: this.state.plan.revision });
  }

  private async persist(): Promise<void> {
    this.render();
    const editor=vscode.window.activeTextEditor;if(editor?.document.uri.scheme==='file')this.codeMap.schedule(editor);
    const directory = this.context.storageUri;
    if (!directory) { return; }
    const bytes = Buffer.from(JSON.stringify(this.state), 'utf8');
    // Das Artefakt ist auch im Testhost dauerhaft. Die Schreibkette verhindert,
    // dass ein langsamer älterer Stand eine neuere Entscheidung überschreibt.
    this.persistence = this.persistence.catch(() => undefined).then(async () => {
      await vscode.workspace.fs.createDirectory(directory);
      const temporary = vscode.Uri.joinPath(directory, 'step.next.json');
      await vscode.workspace.fs.writeFile(temporary, bytes);
      await vscode.workspace.fs.rename(temporary, vscode.Uri.joinPath(directory, 'step.json'), { overwrite: true });
    });
    await this.persistence;
  }

  private render(): void {
    const config=vscode.workspace.getConfiguration('tandem');
    this.view.post({ type: 'state', state: this.state, busy: this.busy, verifying: this.verifying,
      provider:config.get('provider','auto'),stream:this.stream,error:this.error,draftAvailable:!!this.editableDraft,canPausePreview:!!this.liveCandidate,metrics:pilotMetrics(this.state),verificationCommand:JSON.stringify([config.get('verificationProgram',''),...config.get<string[]>('verificationArgs',[])]) });
    if(this.state.scope!==this.simulationContextScope){this.simulationContextScope=this.state.scope;this.simulationContext=this.state.scope?simulationParameters(contentOf(this.state.scope),this.state.scope.prefix.length):{parameters:[],limitation:'Bitte zuerst eine C#-Methode als Arbeitsbereich wählen.'};}
    this.simulationView.post({type:'simulationState',session:this.state.simulation,canCompare:this.state.proposal?.status==='ready',busy:this.busy||this.verifying,
      context:this.simulationContext});
    this.updateGraph();
  }

  private updateGraph():void{this.graphView.update(softwareGraph(this.state,this.codeMap.getContext()));}
  getGraphSnapshot():SoftwareGraph|undefined{return this.graphView.getSnapshot();}
  isGraphReady():boolean{return this.graphView.ready;}
  openGraph():void {
    const active=vscode.window.activeTextEditor;
    const editor=active?.document.uri.scheme==='file'&&vscode.workspace.getWorkspaceFolder(active.document.uri)?active:vscode.window.visibleTextEditors.find(editor=>editor.document.uri.scheme==='file'&&vscode.workspace.getWorkspaceFolder(editor.document.uri));
    if(editor){this.codeColumn=editor.viewColumn??vscode.ViewColumn.One;this.codeMap.schedule(editor);}
    this.updateGraph();this.graphView.open();
  }
  async openGraphNode(node:GraphNode):Promise<void> {
    if(!node.anchor)return;
    if(!Number.isSafeInteger(node.anchor.line)||node.anchor.line<1)throw new Error('Der Codeanker hat keine gültige Quellzeile.');
    const uri=vscode.Uri.parse(node.anchor.uri),folder=vscode.workspace.getWorkspaceFolder(uri);
    if(uri.scheme!=='file'||!folder)throw new Error('Dieses Kartenelement gehört zu keinem geöffneten lokalen Projekt.');
    await this.mapClient.request('check_path',{path:path.relative(folder.uri.fsPath,uri.fsPath).replace(/\\/g,'/')},undefined,folder.uri.fsPath);
    const doc=await vscode.workspace.openTextDocument(uri);if(node.anchor.line>doc.lineCount)throw new Error('Der Codeanker liegt hinter dem aktuellen Dateiende. Bitte die Karte erneut prüfen.');
    const line=node.anchor.line-1,character=Math.max(0,Math.min(node.anchor.character??0,doc.lineAt(line).text.length));
    let column=vscode.window.visibleTextEditors.find(editor=>editor.document.uri.scheme==='file'&&vscode.workspace.getWorkspaceFolder(editor.document.uri))?.viewColumn??this.codeColumn;
    if(column===this.graphView.getColumn())column=column===vscode.ViewColumn.One?vscode.ViewColumn.Two:vscode.ViewColumn.One;
    const editor=await vscode.window.showTextDocument(doc,{viewColumn:column,selection:new vscode.Range(line,character,line,character),preserveFocus:true,preview:true});this.codeMap.schedule(editor);
  }

  private renderStream():void {
    if(this.renderTimer)return;
    this.renderTimer=setTimeout(()=>{this.renderTimer=undefined;this.render();},60);
  }

  private report(error: unknown): void {
    this.error = error instanceof Error ? error.message : String(error);
    this.output.appendLine(this.error);
    this.render();
    void vscode.window.showErrorMessage(this.error);
  }

  private async handle(message: any): Promise<unknown> {
    if(this.delegationActive&&['send','delegate','apply','verify','capture','useDraft'].includes(message.action))throw new Error('Die Delegation läuft. Stoppe sie zuerst, um selbst den Arbeitsbereich oder Vorschlag zu wechseln.');
    switch (message.action) {
      case 'ready': case 'repositoryReady': return this.render();
      case 'openGraph': return this.openGraph();
      case 'capture': return this.capture();
      case 'save': return this.updatePlan({ goal: message.goal, not_now: message.not_now, acceptance: message.acceptance });
      case 'send': return this.uiRun(()=>this.send(message.prompt, message.mode));
      case 'stop': return this.stop();
      case 'preview': return this.preview();
      case 'apply': return this.apply();
      case 'discard': return this.discard();
      case 'verify': return this.uiRun(()=>this.verify());
      case 'exploreRepository': return this.uiRun(()=>this.exploreRepository(message.task));
      case 'discussRepository': return this.uiRun(()=>this.discussRepository(message.prompt));
      case 'revalidateAnalysis': return this.revalidateAnalysis();
      case 'openPlanFile': return this.openPlanFile(message.path);
      case 'adoptChangePlan': return this.adoptChangePlan();
      case 'saveChangePlan': return this.updateChangePlan(message.plan);
      case 'usePlanStep': return this.usePlanStep(message.id);
      case 'openSource': return this.openSource(message.index);
      case 'saveDecision': return this.saveDecision(message.decision);
      case 'reshapeStep': return this.reshapeStep(message.id, message.operation);
      case 'acceptStep': return this.completeStep(message.kind, message.rationale);
      case 'openSimulation': return vscode.commands.executeCommand('tandem.simulation.focus');
      case 'simulate': return this.runSimulation(message.scenario,message.compare);
      case 'focusSimulation': return this.focusSimulation(message.index,message.side);
      case 'stopSimulation': return this.stopSimulation();
      case 'simulationLearning': return this.saveSimulationLearning(message.id,message.text);
      case 'saveContract': return this.updateContract(message.rules);
      case 'editProposal': return this.editProposal();
      case 'useDraft': return this.useEditedDraft();
      case 'refreshPlanBasis': return this.refreshPlanBasis();
      case 'resolveScope': return this.resolveScope(message.resolution);
      case 'delegate': return this.uiRun(()=>this.delegate(message.prompt,message.policy));
      case 'exportArtifact': return this.exportArtifact(message.includeCode===true);
      case 'prepareFactory': return this.uiRun(()=>this.prepareFactoryJob(message.prompt));
      case 'runFactory': return this.uiRun(()=>this.runFactoryJob());
      case 'importFactory': return this.uiRun(()=>this.importFactoryResult());
      case 'acceptFactory': return this.uiRun(()=>this.importFactoryResult(this.state.factoryJob?.result_uri?vscode.Uri.parse(this.state.factoryJob.result_uri):undefined));
      case 'openFactory': return this.state.factoryJob?vscode.window.showTextDocument(vscode.Uri.parse(this.state.factoryJob.uri),{preview:false}):undefined;
      case 'saveEvaluation': return this.evaluate(message.evaluation);
      case 'arrange': return vscode.commands.executeCommand('workbench.action.moveFocusedView','tandem.workbench');
      case 'newTask': return this.newTask();
      case 'restoreTask': return vscode.commands.executeCommand('tandem.restoreTask');
      case 'reviewHumanChanges': return this.uiRun(()=>this.reviewHumanChanges(message.prompt));
      case 'pausePreview': return this.pausePreview();
      case 'keepPlan': {
        if(!this.state.changePlan||!this.state.exploration)throw new Error('Es gibt keinen bestehenden Plan und neuen Vorschlag.');
        this.state.dismissedPlanProposalAt=this.state.exploration.created_at;this.record('human','Bestehenden Plan gegenüber neuem Agentenvorschlag beibehalten');return this.persist();
      }
      case 'adopt': {
        if (!this.state.suggestion) { return; }
        const { goal, not_now, acceptance } = this.state.suggestion;
        await this.updatePlan({ goal, not_now, acceptance });
        return;
      }
    }
  }

  private async uiRun(action:()=>Promise<unknown>):Promise<unknown> {
    const stopped=this.stopSequence;
    try{return await action();}catch(error){if(stopped!==this.stopSequence)return;throw error;}
  }

  async updatePlan(fields: Omit<Plan, 'revision'>): Promise<void> {
    if (Object.values(fields).some(value => typeof value !== 'string') || !fields.goal.trim()) {
      throw new Error('Bitte ein konkretes Ziel für den gemeinsamen Schritt angeben.');
    }
    this.state.plan = { ...fields, revision: this.state.plan.revision + 1 };
    const step = activeStep(this.state);
    if (step && this.state.changePlan) {
      rememberPlan(this.state,'human','Ziel oder Grenzen des aktiven Schritts geändert');
      step.title = fields.goal; step.acceptance = fields.acceptance;
      step.details = {...(step.details ?? {needs_now:'',smallest_change:'',reuse:'',invariants:'',assumptions:'',risks:''}),not_now:fields.not_now};
      this.state.changePlan.revision++;
    }
    invalidateWork(this.state);
    bindContract(this.state);
    this.record('human', 'Plan und Change Contract geändert');
    this.error = '';
    await this.persist();
  }

  private invalidateRepository(uri: vscode.Uri): void {
    if(this.state.factoryJob?.scope_uri===uri.toString()){
      if(invalidateFactoryJob(this.state,'Code der Worker-Übergabe wurde geändert.'))this.stop();
      void this.persist().catch(error=>this.report(error));
    }
    const architecture=this.state.architecture;
    if(architecture&&(architecture.source_uri===uri.toString()||architecture.nodes.some(node=>vscode.Uri.joinPath(vscode.Uri.parse(architecture.root_uri),...node.path.split('/')).toString()===uri.toString())))architecture.stale=true;
    const roots = [this.state.exploration?.root_uri, this.state.changePlan?.root_uri].filter(Boolean);
    const changed = roots.some(root => {
      const relative = path.relative(vscode.Uri.parse(root!).fsPath, uri.fsPath);
      return relative !== '' && relative !== '..' && !relative.startsWith('..' + path.sep) && !path.isAbsolute(relative)
        && !/(^|[/\\])(\.git|node_modules|bin|obj|target|dist|out)([/\\]|$)/i.test(relative);
    });
    if (!changed) { return; }
    if(this.state.activePlanStepId&&invalidateFactoryJob(this.state,'Quellenstand des gemeinsamen Plans wurde geändert.'))this.stop();
    if (this.state.simulation?.uri === uri.toString()) { this.state.simulation.stale = true; }
    if (this.state.exploration) { this.state.exploration.stale = true; }
    if (this.state.changePlan) { this.state.changePlan.stale = true; }
    void this.persist().catch(error => this.report(error));
  }

  private async repositoryFolder(): Promise<vscode.WorkspaceFolder> {
    const folders = vscode.workspace.workspaceFolders?.filter(folder => folder.uri.scheme === 'file') ?? [];
    if (!folders.length) { throw new Error('Bitte zuerst einen lokalen oder entfernten Projektordner öffnen.'); }
    if (folders.length === 1) { return folders[0]; }
    const selected = await vscode.window.showWorkspaceFolderPick({ placeHolder: 'Welches Repository soll TANDEM lesend untersuchen?' });
    if (!selected || selected.uri.scheme !== 'file') { throw new Error('Kein Repository ausgewählt.'); }
    return selected;
  }

  private folderForRoot(root: string): vscode.WorkspaceFolder {
    const folder = vscode.workspace.workspaceFolders?.find(item => item.uri.toString() === root);
    if (!folder) { throw new Error('Das zugehörige Repository ist nicht mehr geöffnet.'); }
    return folder;
  }

  private async repositoryOverlays(folder: vscode.WorkspaceFolder): Promise<Overlay[]> {
    const documents = vscode.workspace.textDocuments.filter(doc => doc.isDirty && doc.uri.scheme === 'file' && vscode.workspace.getWorkspaceFolder(doc.uri)?.uri.toString() === folder.uri.toString());
    if (!documents.length) { return []; }
    const relative = (doc: vscode.TextDocument) => path.relative(folder.uri.fsPath, doc.uri.fsPath).replace(/\\/g, '/');
    // Zuerst nur Namen prüfen. Gesperrte Dokumentinhalte verlassen den Host nie.
    const allowed = await this.client.request<{ allowed: string[] }>('check_paths', { paths: documents.map(relative) }, undefined, folder.uri.fsPath);
    const overlays = documents.filter(doc => allowed.allowed.includes(relative(doc))).map(doc => ({ path: relative(doc), content: doc.getText() }));
    if (overlays.reduce((sum, item) => sum + Buffer.byteLength(item.content, 'utf8'), 0) > 1_000_000) { throw new Error('Die ungespeicherten Dokumente überschreiten zusammen 1 MB. Bitte den Kontext verkleinern.'); }
    return overlays.sort((a,b) => a.path.localeCompare(b.path));
  }

  async exploreRepository(task: string, discussion?: string): Promise<Exploration> {
    if (!vscode.workspace.isTrusted) { throw new Error('Die Repository-Erkundung benötigt einen vertrauenswürdigen Workspace.'); }
    if (this.busy || this.verifying) { throw new Error('Ein Lauf oder eine Prüfung ist noch aktiv.'); }
    if (typeof task !== 'string' || !task.trim()) { throw new Error('Bitte das gewünschte Verhalten oder den Ticketinhalt eingeben. Eine Ticketnummer allein enthält noch keine Fehlerbeschreibung.'); }
    const folder = await this.repositoryFolder();
    if (this.busy || this.verifying) { throw new Error('Ein Lauf oder eine Prüfung ist noch aktiv.'); }
    const generation = ++this.generation;
    const plan = { ...this.state.plan };
    const sharedRevision = this.state.changePlan?.revision;
    this.busy = true; this.stream = ''; this.error = '';
    this.state.analysisTask = task;
    this.state.chat.push({ role: 'user', text: discussion || `Repository-Aufgabe: ${task}` });
    try {
      await this.persist();
      const overlays = await this.repositoryOverlays(folder);
      const shared = this.state.changePlan;
      const planContext = shared ? { goal:shared.goal, not_now:shared.not_now, open_questions:shared.open_questions, steps:shared.steps } : null;
      const prompt = discussion ? `${task}\n\nGemeinsam bearbeiteter Plan (Arbeitsstand):\n${JSON.stringify(planContext)}\n\nAktueller Einwand oder Entscheidung des Entwicklers:\n${discussion}\n\nBerücksichtige diese Eingabe. Aktualisiere den Planvorschlag bei Bedarf, ohne den menschlichen Plan selbst zu ersetzen.` : task;
      const result = await this.client.request<Exploration>('explore', { prompt, plan, history: this.state.chat.slice(-8), overlays }, event => {
        if (generation !== this.generation) { return; }
        if (typeof event.data?.text_delta === 'string') { this.stream += event.data.text_delta; }
        if (event.data?.tool_call?.name) { this.stream = `Leseanalyse: ${event.data.tool_call.name}`; }
        this.render();
      }, folder.uri.fsPath);
      if (generation !== this.generation) { throw new Error('Repository-Erkundung wurde gestoppt.'); }
      if (result.suggested_plan) { validateChangePlan(result.suggested_plan, result.sources, true); }
      const currentOverlays = await this.repositoryOverlays(folder);
      const stale = result.stale || this.state.plan.revision !== plan.revision || this.state.changePlan?.revision !== sharedRevision || JSON.stringify(overlays) !== JSON.stringify(currentOverlays);
      this.state.exploration = { ...result, stale, root_uri: folder.uri.toString(), task, overlays, created_at: new Date().toISOString() };
      this.state.chat.push({ role: 'assistant', text: result.answer });
      this.state.chat = this.state.chat.slice(-24);
      this.record('agent', `Repository lesend untersucht: ${result.sources.length} Quellen; ${result.instruction_paths.length} Projektregeln. Keine Tests ausgeführt.`);
      this.renderRepositorySources();
      if (stale) { this.error = 'Der Repository- oder Planstand wurde während der Analyse geändert. Bitte erneut erkunden.'; }
      return result;
    } finally {
      if (generation === this.generation) { this.busy = false; this.stream = ''; await this.persist(); }
    }
  }

  async discussRepository(prompt: string): Promise<Exploration> {
    if (typeof prompt !== 'string' || !prompt.trim()) { throw new Error('Bitte eine Frage, Entscheidung oder Korrektur eingeben.'); }
    if (!this.state.analysisTask) { throw new Error('Bitte zuerst die Aufgabe beschreiben.'); }
    return this.exploreRepository(this.state.analysisTask, prompt);
  }

  async revalidateAnalysis(): Promise<void> {
    if (this.busy || this.verifying) { throw new Error('Bitte den laufenden Vorgang zuerst beenden.'); }
    const analysis = this.state.exploration;
    if (!analysis) { throw new Error('Bitte zuerst die Aufgabe untersuchen.'); }
    const folder = this.folderForRoot(analysis.root_uri);
    const overlays = await this.repositoryOverlays(folder);
    const result = await this.client.request<{current:boolean}>('validate_exploration', {sources:analysis.sources,catalog_id:analysis.catalog_id,overlays},undefined,folder.uri.fsPath);
    analysis.stale = !result.current;
    if (this.state.changePlan?.catalog_id === analysis.catalog_id) { this.state.changePlan.stale = !result.current; }
    this.error = result.current ? '' : 'Dateien oder Editorstände haben sich geändert. Bitte die Analyse aktualisieren.';
    this.record('runtime', result.current ? 'Quellenstand ohne Modellaufruf bestätigt' : 'Quellenstand geändert; neue Analyse erforderlich');
    await this.persist();
  }

  async refreshPlanBasis():Promise<void> {
    if(this.busy||this.verifying)throw new Error('Bitte den laufenden Vorgang zuerst beenden.');
    await this.assertExplorationCurrent();
    const analysis=this.state.exploration!,plan=this.state.changePlan;if(!plan)throw new Error('Es gibt keinen gemeinsamen Plan.');
    const unread=[...new Set(plan.steps.flatMap(step=>step.files))].filter(file=>!analysis.sources.some(source=>source.path===file));
    if(unread.length)throw new Error('Die aktuelle Analyse hat diese Plandateien noch nicht gelesen: '+unread.join(', ')+'. Bitte gezielt untersuchen lassen.');
    rememberPlan(this.state,'human','Plan mit aktueller Analyse erneut verbunden');
    for(const step of plan.steps){step.source_ids=analysis.sources.filter(source=>step.files.includes(source.path)).map(source=>source.id);if(step.status==='accepted'||step.status==='verified')step.status='clarifying';}
    Object.assign(plan,{sources:analysis.sources,catalog_id:analysis.catalog_id,basis_created_at:analysis.created_at,stale:false,revision:plan.revision+1});
    this.state.plan.revision++;invalidateWork(this.state);bindContract(this.state);
    this.record('human','Bestehenden Plan ausdrücklich auf aktuellen Quellenstand gestellt; alte Prüfbelege bleiben historisch');await this.persist();
  }

  async exportArtifact(includeCode=false):Promise<vscode.Uri> {
    const directory=this.context.storageUri;if(!directory)throw new Error('Für diesen Arbeitsplatz ist kein lokaler Speicher verfügbar.');
    this.record('human',includeCode?'Übergabe mit Code-Snapshots exportiert':'Übergabe als Metadaten exportiert');await this.persist();
    const uri=vscode.Uri.joinPath(directory,`handoff-${Date.now()}.json`);
    await vscode.workspace.fs.writeFile(uri,Buffer.from(JSON.stringify(handoffArtifact(this.state,includeCode),null,2)));
    await vscode.window.showTextDocument(uri,{preview:false});return uri;
  }

  private factoryIdle():void {
    if(!vscode.workspace.isTrusted)throw new Error('Bitte zuerst diesem Workspace vertrauen.');
    if(this.busy||this.verifying||this.delegationActive)throw new Error('Bitte den laufenden Vorgang zuerst beenden.');
  }

  private async factoryCheckout(record:FactoryJobRecord):Promise<void> {
    assertFactoryCurrent(this.state,record);
    const uri=vscode.Uri.parse(record.scope_uri),folder=vscode.workspace.getWorkspaceFolder(uri);
    if(!folder||folder.uri.toString()!==record.root_uri)throw new Error('Die Worker-Übergabe gehört zu einem anderen Workspace.');
    await this.mapClient.request('check_path',{path:record.job.binding.relative_path},undefined,folder.uri.fsPath);
    const doc=await vscode.workspace.openTextDocument(uri),bytes=await vscode.workspace.fs.readFile(uri);
    assertFactoryCurrent(this.state,record);
    if(doc.isDirty||doc.getText()!==contentOf(record.job.request.snapshot)||sha256(bytes)!==record.job.binding.disk_sha256)throw new Error('Der aktuelle Code weicht vom gespeicherten Worker-Job ab. Bitte neu vorbereiten.');
  }

  async prepareFactoryJob(prompt=''):Promise<void> {
    this.factoryIdle();
    const scope=this.state.scope,directory=this.context.storageUri;
    if(!scope||!directory)throw new Error('Bitte zuerst einen Codebereich in einem lokalen Workspace wählen.');
    const uri=vscode.Uri.parse(scope.uri),folder=vscode.workspace.getWorkspaceFolder(uri);
    if(!folder)throw new Error('Die ausgewählte Datei gehört zu keinem Workspace.');
    const relative=path.relative(folder.uri.fsPath,uri.fsPath).replace(/\\/g,'/');
    await this.mapClient.request('check_path',{path:relative},undefined,folder.uri.fsPath);
    const doc=await vscode.workspace.openTextDocument(uri),bytes=await vscode.workspace.fs.readFile(uri);
    this.factoryIdle();
    if(this.state.scope!==scope||doc.isDirty||doc.getText()!==contentOf(scope))throw new Error('Bitte den aktuellen Code speichern und den Arbeitsbereich erneut wählen.');
    const job=createFactoryJob(this.state,relative,sha256(bytes),prompt),raw=JSON.stringify(job,null,2);
    const jobs=vscode.Uri.joinPath(directory,'factory-jobs'),jobUri=vscode.Uri.joinPath(jobs,job.job_id+'.json');
    const record:FactoryJobRecord={job,sha256:sha256(raw),uri:jobUri.toString(),root_uri:folder.uri.toString(),scope_uri:scope.uri,status:'prepared',
      command:[this.client.executablePath(),'--consume-handoff',jobUri.fsPath,'--provider',vscode.workspace.getConfiguration('tandem').get<string>('provider','auto'),'--workspace',folder.uri.fsPath]};
    await vscode.workspace.fs.createDirectory(jobs);await vscode.workspace.fs.writeFile(jobUri,Buffer.from(raw));
    this.factoryIdle();
    // Während des Speicherns kann der Entwickler den Plan oder Code wechseln.
    assertFactoryCurrent({...this.state,factoryJob:record},record);
    if(doc.isDirty||doc.getText()!==contentOf(scope))throw new Error('Code wurde während der Vorbereitung geändert. Bitte neu vorbereiten.');
    if(this.state.factoryJob)(this.state.factoryHistory??=[]).push(structuredClone(this.state.factoryJob));
    this.state.factoryJob=record;this.record('human','Worker-Job mit Code-Snapshot vorbereitet; keine Schreibrechte übertragen');await this.persist();
  }

  async runFactoryJob():Promise<void> {
    this.factoryIdle();const record=this.state.factoryJob;
    if(!record||record.status!=='prepared')throw new Error('Bitte zuerst einen aktuellen Worker-Job vorbereiten.');
    await this.factoryCheckout(record);
    const raw=Buffer.from(await vscode.workspace.fs.readFile(vscode.Uri.parse(record.uri))).toString('utf8');
    if(sha256(raw)!==record.sha256)throw new Error('Die gespeicherte Jobdatei wurde verändert. Bitte neu vorbereiten.');
    this.factoryIdle();assertFactoryCurrent(this.state,record);
    const generation=++this.generation;this.busy=true;this.error='';record.status='running';await this.persist();
    try{
      if(generation!==this.generation)throw new Error('Worker wurde gestoppt.');
      const result=await this.client.request<unknown>('factory',{job_json:raw},event=>{
        if(generation===this.generation&&typeof event.data?.text_delta==='string'){this.stream+=event.data.text_delta;this.renderStream();}
      },vscode.Uri.parse(record.root_uri).fsPath);
      if(generation!==this.generation)throw new Error('Worker wurde gestoppt.');
      await this.factoryCheckout(record);
      const validated=validateFactoryResult(result,record),resultUri=vscode.Uri.joinPath(this.context.storageUri!,'factory-jobs',record.job.job_id+'.result.json');
      await vscode.workspace.fs.writeFile(resultUri,Buffer.from(JSON.stringify(validated,null,2)));
      if(generation!==this.generation)throw new Error('Worker wurde gestoppt.');
      assertFactoryCurrent(this.state,record);record.result=validated;record.result_uri=resultUri.toString();record.status='result_ready';
      this.record('agent','Worker-Ergebnis liegt vor; lokale Prüfung und Übernahme als Vorschlag stehen aus');
    }catch(error){if(generation===this.generation&&record.status==='running'){record.status='failed';record.error=error instanceof Error?error.message:String(error);}throw error;}
    finally{if(generation===this.generation){this.busy=false;this.stream='';await this.persist();}}
  }

  async importFactoryResult(uri?:vscode.Uri):Promise<void> {
    this.factoryIdle();const record=this.state.factoryJob;
    if(!record)throw new Error('Bitte zuerst einen Worker-Job für diesen Schritt vorbereiten.');
    assertFactoryCurrent(this.state,record);
    if(!uri){uri=(await vscode.window.showOpenDialog({canSelectMany:false,filters:{'Worker-Ergebnis':['json']},openLabel:'Worker-Ergebnis prüfen'}))?.[0];if(!uri)return;}
    if(uri.scheme!=='file'||(await vscode.workspace.fs.stat(uri)).size>1_000_000)throw new Error('Bitte eine lokale Ergebnisdatei mit höchstens 1 MB wählen.');
    const bytes=await vscode.workspace.fs.readFile(uri);
    if(bytes.length>1_000_000)throw new Error('Das Worker-Ergebnis überschreitet 1 MB.');
    const envelope=validateFactoryResult(JSON.parse(Buffer.from(bytes).toString('utf8')),record);
    await this.factoryCheckout(record);this.factoryIdle();assertFactoryCurrent(this.state,record);
    const result=envelope.result,scope=this.state.scope!;
    const content=result.patch?validatePatch(scope,this.state.plan,result.patch,result.plan_revision):undefined;
    if(result.patch)this.checkContract(result.patch);
    if(this.state.proposal)(this.state.proposalHistory??=[]).push(structuredClone(this.state.proposal));
    if(result.patch){this.state.proposal={patch:result.patch,base:structuredClone(scope),content:content!,revision:result.plan_revision,status:'ready'};const step=activeStep(this.state);if(step)step.status='preview';}
    else if(this.state.proposal?.status==='ready')this.state.proposal.status='stale';
    this.state.suggestion=result.plan_suggestion??undefined;this.state.scopeSuggestion=result.scope_suggestion??undefined;
    this.state.chat.push({role:'assistant',text:'Worker-Ergebnis (lokal geprüft; keine reale Verifikation):\n\n'+result.answer});
    record.result=envelope;record.result_uri=uri.toString();record.status='imported';record.error=undefined;
    this.record('human','Worker-Ergebnis gegen aktuellen Code und Contract geprüft; noch nicht im Produktcode übernommen');await this.persist();
    if(result.patch)await this.preview();
  }

  private async archiveTask():Promise<NonNullable<State['archivedSessions']>> {
    const directory=this.context.storageUri;if(!directory)throw new Error('Für diesen Arbeitsplatz ist kein lokaler Speicher verfügbar.');
    const id=this.state.sessionId??randomUUID(),archive=vscode.Uri.joinPath(directory,'sessions');
    await vscode.workspace.fs.createDirectory(archive);await vscode.workspace.fs.writeFile(vscode.Uri.joinPath(archive,id+'.json'),Buffer.from(JSON.stringify(this.state)));
    return[...(this.state.archivedSessions??[]).filter(item=>item.id!==id),{id,title:this.state.analysisTask||this.state.plan.goal||'Ohne Titel',at:new Date().toISOString()}];
  }

  async newTask():Promise<void> {
    this.stop();if(this.verifying)throw new Error('Die laufende Prüfung wird gestoppt. Bitte danach eine neue Aufgabe beginnen.');
    const archivedSessions=await this.archiveTask();
    this.state={sessionId:randomUUID(),archivedSessions,plan:{revision:1,goal:'',not_now:'Keine zusätzlichen Dateien, Klassen oder Refactorings.',acceptance:''},chat:[],audit:[]};
    this.editableDraft=undefined;this.previewDocuments.clear();this.record('human','Neue Aufgabe begonnen; vorherigen Ablauf lokal archiviert');await this.persist();this.renderRepositorySources();
  }

  async restoreTask(id:string):Promise<void> {
    if(!/^[a-f0-9-]{36}$/.test(id)||!this.state.archivedSessions?.some(item=>item.id===id)||!this.context.storageUri)throw new Error('Diese gespeicherte Aufgabe ist nicht verfügbar.');
    this.stop();if(this.verifying)throw new Error('Die laufende Prüfung wird gestoppt. Bitte danach die Aufgabe wechseln.');
    const bytes=await vscode.workspace.fs.readFile(vscode.Uri.joinPath(this.context.storageUri,'sessions',id+'.json'));
    const restored=JSON.parse(Buffer.from(bytes).toString('utf8')) as State;
    if(restored.sessionId!==id||!restored.plan||!Array.isArray(restored.chat)||!Array.isArray(restored.audit))throw new Error('Das gespeicherte Aufgabenformat ist ungültig.');
    const archivedSessions=await this.archiveTask();this.state={...restored,archivedSessions};
    invalidateFactoryJob(this.state,'Aufgabe wiederhergestellt; Übergabe bitte neu vorbereiten.');
    if(this.state.proposal?.status==='ready')this.state.proposal.status='stale';
    if(this.state.evidence)this.state.evidence.status='stale';
    if(this.state.exploration)this.state.exploration.stale=true;
    if(this.state.changePlan)this.state.changePlan.stale=true;
    if(this.state.simulation)this.state.simulation.stale=true;
    if(this.state.architecture)this.state.architecture.stale=true;
    if(this.state.delegation?.status==='running')this.state.delegation.status='stopped';
    this.editableDraft=undefined;this.record('human','Gespeicherte Aufgabe fortgesetzt; Codebindungen müssen erneut geprüft werden');await this.persist();this.renderRepositorySources();
  }

  async importArchitecture():Promise<void> {
    if(this.busy||this.verifying||this.delegationActive)throw new Error('Bitte den laufenden Vorgang zuerst beenden.');
    const folder=await this.repositoryFolder(),uri=vscode.Uri.joinPath(folder.uri,'.tandem','architecture.json');
    await this.mapClient.request('check_path',{path:'.tandem/architecture.json'},undefined,folder.uri.fsPath);
    const bytes=await vscode.workspace.fs.readFile(uri);if(bytes.length>256_000)throw new Error('Die Architekturdatei darf höchstens 256 KB groß sein.');
    const imported=validateArchitecture(JSON.parse(Buffer.from(bytes).toString('utf8')));
    for(const node of imported.nodes){await this.mapClient.request('check_path',{path:node.path},undefined,folder.uri.fsPath);const doc=await vscode.workspace.openTextDocument(vscode.Uri.joinPath(folder.uri,...node.path.split('/')));if(node.line>doc.lineCount)throw new Error('Der Codeanker liegt hinter dem Dateiende: '+node.path);}
    this.state.architecture={...imported,source_uri:uri.toString(),root_uri:folder.uri.toString(),sha256:createHash('sha256').update(bytes).digest('hex'),imported_at:new Date().toISOString(),stale:false};
    this.record('human','Dokumentierte Architektur und ausdrücklich markierte Hypothesen importiert');await this.persist();
    const editor=vscode.window.activeTextEditor;if(editor)this.codeMap.schedule(editor);
  }

  async evaluate(input:Omit<Evaluation,'at'|'step_id'>):Promise<void> {
    saveEvaluation(this.state,input);this.record('human','Lokale Pilotevaluation und eigene Erklärung gespeichert');await this.persist();
  }

  async reviewHumanChanges(prompt=''):Promise<void> {
    if(this.busy||this.verifying||this.delegationActive)throw new Error('Bitte den laufenden Vorgang zuerst beenden.');
    const change=this.state.humanChange;
    if(!change||change.before===change.after)throw new Error('Es gibt keine eigenen Änderungen im aktuellen Arbeitsbereich zu begutachten.');
    const doc=await vscode.workspace.openTextDocument(vscode.Uri.parse(change.uri)),folder=vscode.workspace.getWorkspaceFolder(doc.uri);
    if(!folder)throw new Error('Der Projektordner ist nicht mehr geöffnet.');
    await this.client.request('check_path',{path:path.relative(folder.uri.fsPath,doc.uri.fsPath).replace(/\\/g,'/')},undefined,folder.uri.fsPath);
    const after=doc.getText();if(after!==change.after)throw new Error('Der Code wurde inzwischen geändert. Bitte erneut begutachten lassen.');
    if(Buffer.byteLength(after,'utf8')>500_000)throw new Error('Für das Review bitte eine Datei unter 500 KB verwenden.');
    let start=0;while(start<change.before.length&&start<after.length&&change.before[start]===after[start])start++;
    let end=0;while(end<change.before.length-start&&end<after.length-start&&change.before[change.before.length-1-end]===after[after.length-1-end])end++;
    const oldRegion=change.before.slice(Math.max(0,start-300),change.before.length-end+300);
    const plan={...this.state.plan},generation=++this.generation;
    this.busy=true;this.error='';this.stream='';
    const reviewPrompt='Begutachte die menschliche Codeänderung vollständig lesend. Ersetze sie nicht. Prüfe Zieltreue, Invarianten, Risiken, kleinere Alternativen und notwendige echte Tests. Eine KI-Begutachtung ist keine reale Verifikation.\n'+
      'Ursprünglicher Ausschnitt (gegebenenfalls auf 12 KB gekürzt):\n'+oldRegion.slice(0,12_000)+'\nAktueller Code steht im Snapshot.\nEntwicklerfrage: '+(typeof prompt==='string'?prompt.slice(0,4000):'');
    const previousUri=vscode.Uri.parse(`${PREVIEW_SCHEME}:/human-${generation}/${path.basename(doc.fileName)}`);this.previewDocuments.set(previousUri.toString(),change.before);
    try{
      await vscode.commands.executeCommand('vscode.diff',previousUri,doc.uri,'TANDEM · Deine Änderungen (rechts direkt bearbeitbar)',{preview:true,preserveFocus:true});await this.persist();
      const result=await this.client.request<RunResult>('run',{plan,snapshot:{path:change.path,prefix:'',editable:after,suffix:''},mode:'explore',prompt:reviewPrompt,history:this.state.chat.slice(-8)},event=>{if(generation!==this.generation)return;if(typeof event.data?.text_delta==='string'){this.stream+=event.data.text_delta;this.renderStream();}},folder.uri.fsPath);
      if(generation!==this.generation)throw new Error('Begutachtung gestoppt.');
      if(result.patch)throw new Error('Eine lesende Begutachtung darf keinen Patch liefern.');
      const stale=doc.getText()!==after||this.state.plan.revision!==plan.revision;
      if(this.state.humanChange?.uri===change.uri)this.state.humanChange.review={at:new Date().toISOString(),answer:result.answer,snapshot_hash:createHash('sha256').update(after).digest('hex'),stale};
      this.state.chat.push({role:'user',text:'Bitte meine direkten Codeänderungen begutachten.'},{role:'assistant',text:result.answer});
      if(!stale)this.state.suggestion=result.plan_suggestion;
      this.record('agent',stale?'KI-Begutachtung durch weitere Edits veraltet':'Menschliche Änderung lesend begutachtet; keine reale Prüfung');
    }finally{if(generation===this.generation){this.busy=false;this.stream='';await this.persist();}}
  }

  async openPlanFile(file: string): Promise<void> {
    const plan = this.state.changePlan;
    if (!plan || !plan.steps.some(step => step.files.includes(file))) { throw new Error('Die Datei gehört nicht zum aktuellen Plan.'); }
    const folder = this.folderForRoot(plan.root_uri);
    await this.client.request('check_path',{path:file},undefined,folder.uri.fsPath);
    await vscode.window.showTextDocument(vscode.Uri.joinPath(folder.uri,...file.split('/')), {preview:true});
  }

  private async assertExplorationCurrent(): Promise<void> {
    const analysis = this.state.exploration;
    if (!analysis || analysis.stale) { throw new Error('Die Repository-Analyse fehlt oder ist veraltet. Bitte erneut erkunden.'); }
    const folder = this.folderForRoot(analysis.root_uri);
    const overlays = await this.repositoryOverlays(folder);
    const result = await this.client.request<{ current: boolean }>('validate_exploration', { sources: analysis.sources, catalog_id: analysis.catalog_id, overlays }, undefined, folder.uri.fsPath);
    if (!result.current) {
      analysis.stale = true;
      if (this.state.changePlan) { this.state.changePlan.stale = true; }
      await this.persist();
      throw new Error('Die Quellen der Repository-Analyse haben sich geändert. Bitte erneut erkunden.');
    }
  }

  async adoptChangePlan(): Promise<void> {
    if (this.busy || this.verifying) { throw new Error('Bitte den laufenden Vorgang zuerst beenden.'); }
    await this.assertExplorationCurrent();
    const analysis = this.state.exploration!;
    if (!analysis.suggested_plan) { throw new Error('Der Agent hat noch keinen belegten Änderungsplan vorgeschlagen.'); }
    validateChangePlan(analysis.suggested_plan, analysis.sources, true);
    rememberPlan(this.state,'agent','Neuen Planvorschlag gemeinsam übernommen');
    this.state.changePlan = { ...structuredClone(analysis.suggested_plan), revision: (this.state.changePlan?.revision ?? 0) + 1,
      stale: false, root_uri: analysis.root_uri, catalog_id: analysis.catalog_id, sources: analysis.sources, basis_created_at: analysis.created_at };
    this.state.activePlanStepId = undefined;
    this.record('human', 'Belegten Änderungsplan übernommen; keine Schreibrechte erweitert');
    await this.persist();
  }

  async updateChangePlan(plan: ChangePlan): Promise<void> {
    const previous = this.state.changePlan;
    if (!previous) { throw new Error('Zuerst den vorgeschlagenen Änderungsplan übernehmen.'); }
    validateChangePlan(plan, previous.sources, false);
    // Abschluss und reale Prüfung entstehen ausschließlich über ihre eigenen
    // Prüfpfade, nie durch ein Statusfeld aus dem Webview oder Modell.
    for (const step of plan.steps) {
      const old = previous.steps.find(item => item.id === step.id);
      if (['verified','accepted','applied','preview'].includes(step.status ?? '') && step.status !== old?.status) { throw new Error('Dieser Status benötigt den zugehörigen Prüf- oder Übernahmepfad.'); }
      if(old&&JSON.stringify({...step,status:undefined})!==JSON.stringify({...old,status:undefined})&&['accepted','verified','applied','preview'].includes(step.status??''))step.status='clarifying';
    }
    rememberPlan(this.state,'human','Gemeinsamen Plan bearbeitet');
    const { goal, not_now, open_questions, steps } = structuredClone(plan);
    this.state.changePlan = { ...previous, goal, not_now, open_questions, steps, revision: previous.revision + 1 };
    if (this.state.activePlanStepId) {
      const active = steps.find(step => step.id === this.state.activePlanStepId);
      if (active) { this.state.plan = {revision:this.state.plan.revision+1,goal:active.title,not_now,acceptance:active.acceptance}; }
      else { this.state.activePlanStepId = undefined; this.state.scope = undefined; }
    }
    invalidateWork(this.state);
    bindContract(this.state);
    this.record('human', 'Gemeinsamen Änderungsplan bearbeitet');
    await this.persist();
  }

  async saveDecision(input: Pick<Decision,'kind'|'text'|'reason'> & {supersedes?:string}): Promise<void> {
    if (!input) { throw new Error('Bitte eine Erkenntnis eingeben.'); }
    if (this.busy) { this.stop(); }
    const decision = addDecision(this.state,input);
    bindContract(this.state);
    this.record('human', `Erkenntnis festgehalten: ${decision.kind} · ${decision.text}`);
    await this.persist();
  }

  async reshapeStep(id: string, operation: 'split'|'merge'|'defer'): Promise<void> {
    if (!this.state.changePlan) { throw new Error('Bitte zuerst einen gemeinsamen Plan anlegen.'); }
    if (this.busy) { this.stop(); }
    const plan = reshapePlan(this.state.changePlan,id,operation);
    await this.updateChangePlan(plan);
    this.record('human',`Planschritt ${id}: ${operation}`);
    await this.persist();
  }

  async completeStep(kind: 'behavior'|'clarification', rationale: string): Promise<void> {
    if (this.busy || this.verifying) { throw new Error('Bitte den laufenden Vorgang zuerst beenden.'); }
    if (kind === 'behavior' && this.state.evidence) {
      const evidence = this.state.evidence;
      const doc = await vscode.workspace.openTextDocument(vscode.Uri.parse(evidence.uri));
      const hash = createHash('sha256').update(await vscode.workspace.fs.readFile(doc.uri)).digest('hex');
      if (doc.isDirty || doc.getText() !== evidence.content || hash !== evidence.diskHash) { evidence.status = 'stale'; await this.persist(); }
      if(evidence.status==='passed'&&evidence.workspace){
        const folder=this.folderForRoot(evidence.workspace.root_uri);
        try{const snapshot=await this.client.request<WorkspaceSnapshot>('workspace_snapshot',{},undefined,folder.uri.fsPath);if(snapshot.fingerprint!==evidence.workspace.after_hash||(await this.unsavedSources(folder,doc.uri)).length)evidence.status='stale';}
        catch(error){evidence.status='stale';evidence.validation_error=error instanceof Error?error.message:String(error);}
        await this.persist();
      }
    }
    acceptStep(this.state,kind,rationale);
    this.record('human',`Fachlich abgeschlossen (${kind}): ${rationale}`);
    await this.persist();
  }

  async runSimulation(input: Scenario, compare = false): Promise<void> {
    if (this.busy || this.verifying) { throw new Error('Bitte den laufenden Vorgang zuerst beenden.'); }
    const scope=this.state.scope;
    if (!scope) { throw new Error('Bitte zuerst die Methode im Editor als Arbeitsbereich wählen.'); }
    if (!scope.path.toLowerCase().endsWith('.cs')) { throw new Error('Der Gedanken-Debugger unterstützt derzeit C#-Dateien.'); }
    const doc=await vscode.workspace.openTextDocument(vscode.Uri.parse(scope.uri));
    if (doc.getText()!==contentOf(scope)) { throw new Error('Der Arbeitsbereich ist veraltet. Bitte die Methode erneut wählen.'); }
    const proposal=this.state.proposal;
    if (compare) {
      if (!proposal) { throw new Error('Es gibt keinen nicht übernommenen Vorschlag.'); }
      assertCurrent(proposal,this.state.plan,doc.getText());
    }
    const scenario={...input,engine_version:2 as const,version:(this.state.simulation?.scenario.version??0)+1};
    const started=performance.now(),code=doc.getText(),baseline=simulate(code,scope.prefix.length,scenario);
    const proposed=compare?simulate(proposal!.content,scope.prefix.length,scenario):undefined;
    if (this.state.simulation) { (this.state.simulationHistory??=[]).push(structuredClone(this.state.simulation)); }
    const hash=(text:string)=>createHash('sha256').update(text).digest('hex');
    this.state.simulation={id:randomUUID(),at:new Date().toISOString(),uri:scope.uri,path:scope.path,baseline_hash:hash(code),
      proposed_hash:compare?hash(proposal!.content):undefined,plan_revision:this.state.plan.revision,step_id:this.state.activePlanStepId,
      scenario:baseline.scenario,baseline,proposed,divergence:proposed?firstDivergence(baseline,proposed):undefined,
      stale:false,baseline_content:code,proposed_content:compare?proposal!.content:undefined,duration_ms:Math.round((performance.now()-started)*100)/100};
    this.record('human',`Szenario „${scenario.name}“ v${scenario.version} simuliert; keine reale Ausführung`);
    await this.persist();
  }

  async updateContract(rules:Pick<ChangeContract,'mode'|'preserve_public_signatures'|'allow_new_symbols'|'invariants'>):Promise<void> {
    if(!this.state.scope)throw new Error('Bitte zuerst einen Codebereich auswählen.');
    if(!rules||typeof rules.preserve_public_signatures!=='boolean'||typeof rules.allow_new_symbols!=='boolean'||typeof rules.invariants!=='string'||!['pair','explore','delegate'].includes(rules.mode))throw new Error('Ungültige Änderungsrechte.');
    if(this.busy)this.stop();
    this.state.plan.revision++;invalidateWork(this.state);bindContract(this.state,rules);
    this.record('human','Änderungsvertrag und Rechte ausdrücklich geändert');await this.persist();
  }

  async resolveScope(resolution:'exclude'|'separate'):Promise<void> {
    const suggestion=this.state.scopeSuggestion;
    if(!suggestion||!['exclude','separate'].includes(resolution))throw new Error('Es gibt keinen passenden Scope-Vorschlag.');
    if(this.busy||this.verifying)throw new Error('Bitte den laufenden Vorgang zuerst beenden.');
    if(resolution==='exclude') {
      await this.saveDecision({kind:'exclude',text:'Keine Scope-Erweiterung für '+[...suggestion.files,...suggestion.symbols].join(', '),reason:'Kleinere Alternative: '+suggestion.alternative});
    } else {
      const plan=this.state.changePlan;if(!plan)throw new Error('Bitte zuerst einen gemeinsamen Repository-Plan anlegen.');
      await this.updateChangePlan({...plan,steps:[...plan.steps,{id:'human-'+randomUUID(),title:'Zusätzlichen Bedarf getrennt klären',why:suggestion.reason,files:suggestion.files,source_ids:activeStep(this.state)?.source_ids??[],acceptance:'Scope, Risiko und kleinste Alternative gemeinsam entscheiden',status:'clarifying',details:{needs_now:suggestion.reason,not_now:'Keine automatische Implementierung',smallest_change:'Zuerst die Notwendigkeit prüfen',reuse:suggestion.alternative,invariants:'',assumptions:'',risks:suggestion.risk}}]});
    }
    (this.state.scopeSuggestionHistory??=[]).push({at:new Date().toISOString(),suggestion:structuredClone(suggestion),resolution});this.state.scopeSuggestion=undefined;
    this.record('human',resolution==='exclude'?'Zusätzlichen Scope ausgeschlossen':'Zusätzlichen Scope als eigenen Klärungsschritt geplant; keine Schreibrechte erweitert');await this.persist();
  }

  async delegate(prompt:string,policy:{max_changed_lines:number;auto_apply:boolean;low_risk:boolean}):Promise<void> {
    if(this.busy||this.verifying||this.delegationActive)throw new Error('Bitte den laufenden Vorgang zuerst beenden.');
    const contract=this.state.contract,scope=this.state.scope;
    if(!contract||contract.mode!=='delegate'||!scope)throw new Error('Bitte zuerst einen Codebereich und ausdrücklich Delegate wählen.');
    if(!policy||!policy.low_risk||!Number.isInteger(policy.max_changed_lines)||policy.max_changed_lines<1||policy.max_changed_lines>200||typeof policy.auto_apply!=='boolean')throw new Error('Delegation benötigt einen als risikoarm bestätigten Schritt und ein Zeilenbudget von 1 bis 200.');
    if(!this.state.plan.acceptance.trim()||!contract.preserve_public_signatures||contract.allow_new_symbols)throw new Error('Für diese risikoarme Delegation sind Akzeptanzkriterien, geschützte Signaturen und keine neuen Symbole erforderlich.');
    if(this.state.scopeSuggestion)throw new Error('Bitte zuerst den offenen Scope-Konflikt entscheiden.');
    const config=vscode.workspace.getConfiguration('tandem',vscode.Uri.parse(scope.uri)),program=config.get<string>('verificationProgram','').trim(),args=config.get<string[]>('verificationArgs',[]);
    if(policy.auto_apply&&!program)throw new Error('Automatische Übernahme benötigt ein festgelegtes reales Prüfprogramm.');
    if(this.state.delegation)(this.state.delegationHistory??=[]).push(structuredClone(this.state.delegation));
    const delegation:NonNullable<State['delegation']>={at:new Date().toISOString(),status:'running',plan_revision:this.state.plan.revision,contract_version:contract.version,
      max_changed_lines:policy.max_changed_lines,auto_apply:policy.auto_apply,verification_program:program,verification_args:[...args],reason:'Innerhalb des vereinbarten Contracts gestartet.'};
    this.state.delegation=delegation;this.delegationActive=true;const sequence=++this.delegationSequence;
    this.record('human',`Risikoarmen Schritt delegiert; ${policy.max_changed_lines} geänderte Zeilen, ${policy.auto_apply?'begrenzte Übernahme und festgelegte Prüfung':'Vorschlag ohne Übernahme'}`);await this.persist();
    try {
      const result=await this.send(prompt.trim()||'Erledige den klar abgegrenzten Schritt innerhalb des Änderungsvertrags. Bei Unklarheit oder Scopebedarf stoppe mit einem begründeten Konflikt.','pair');
      if(sequence!==this.delegationSequence||delegation.status==='stopped')return;
      if(this.state.plan.revision!==delegation.plan_revision||this.state.contract?.version!==delegation.contract_version)throw new Error('Der Contract wurde während der Delegation geändert.');
      if(result.scope_suggestion)throw new Error('Der Agent benötigt eine gemeinsame Scope-Entscheidung.');
      const proposal=this.state.proposal;
      if(!result.patch||!proposal||proposal.status!=='ready')throw new Error('Es liegt kein aktueller, eindeutig anwendbarer Vorschlag vor. Bitte gemeinsam klären.');
      const lines=proposal.patch.old_text.split('\n').length+proposal.patch.new_text.split('\n').length;
      if(lines>policy.max_changed_lines)throw new Error(`Der Vorschlag überschreitet das Zeilenbudget (${lines} > ${policy.max_changed_lines}). Bitte verkleinern oder neu vereinbaren.`);
      await this.preview();
      if(sequence!==this.delegationSequence)return;
      if(this.state.plan.revision!==delegation.plan_revision||this.state.contract?.version!==delegation.contract_version)throw new Error('Der Contract wurde vor der Übernahme geändert.');
      if(!policy.auto_apply){delegation.status='proposed';delegation.reason='Begrenzter Vorschlag liegt zur gemeinsamen Arbeit bereit.';return;}
      await this.apply();
      if(sequence!==this.delegationSequence)return;
      this.delegateVerification={program,args:[...args]};
      const evidence=await this.verify();
      if(sequence!==this.delegationSequence)return;
      if(evidence.status!=='passed')throw new Error('Die reale Prüfung ist fehlgeschlagen oder veraltet. Code und Prüfausgabe bleiben zur gemeinsamen Korrektur sichtbar.');
      delegation.status='verified';delegation.reason='Im vereinbarten Scope übernommen und real geprüft. Fachliche Abnahme bleibt ein eigener Schritt.';
      this.record('runtime','Delegierter Schritt real geprüft; keine automatische fachliche Abnahme');
    }catch(error){if(delegation.status!=='stopped'){delegation.status='needs_attention';delegation.reason=error instanceof Error?error.message:String(error);this.record('runtime','Delegation zur gemeinsamen Entscheidung gestoppt: '+delegation.reason);}throw error;}
    finally{this.delegationActive=false;this.delegateVerification=undefined;await this.persist();}
  }

  private checkContract(patch:Proposal['patch']):void {
    try{enforceContract(this.state,patch);this.state.contractConflict=undefined;}
    catch(error){this.state.contractConflict={at:new Date().toISOString(),reason:error instanceof Error?error.message:String(error),path:patch.path,patch_reason:patch.reason,plan_revision:this.state.plan.revision};this.record('runtime','Contract-Abweichung vor Anwendung abgewehrt: '+this.state.contractConflict.reason);throw error;}
  }

  async editProposal():Promise<void> {
    const proposal=this.state.proposal;if(!proposal)throw new Error('Es gibt noch keinen Vorschlag.');
    const doc=await vscode.workspace.openTextDocument(vscode.Uri.parse(proposal.base.uri));assertCurrent(proposal,this.state.plan,doc.getText());
    const draft=await vscode.workspace.openTextDocument({language:doc.languageId,content:proposal.patch.new_text});
    this.editableDraft={uri:draft.uri.toString(),revision:proposal.revision,baseline:contentOf(proposal.base)};
    await vscode.window.showTextDocument(draft,{preview:false});
    this.record('human','Isolierten Vorschlag zum Bearbeiten geöffnet; Produktcode unverändert');await this.persist();
  }

  async useEditedDraft():Promise<void> {
    const draft=this.editableDraft,proposal=this.state.proposal;
    if(!draft||!proposal)throw new Error('Bitte zuerst „Vorschlag bearbeiten“ öffnen.');
    if(this.busy||this.verifying)throw new Error('Bitte den laufenden Vorgang zuerst beenden.');
    const doc=await vscode.workspace.openTextDocument(vscode.Uri.parse(proposal.base.uri));assertCurrent(proposal,this.state.plan,doc.getText());
    if(draft.revision!==proposal.revision||draft.baseline!==contentOf(proposal.base))throw new Error('Dieser Entwurf gehört zu einem älteren Vorschlag.');
    const edited=vscode.workspace.textDocuments.find(item=>item.uri.toString()===draft.uri);if(!edited)throw new Error('Der bearbeitete Entwurf ist nicht mehr geöffnet.');
    const patch={...proposal.patch,new_text:edited.getText(),reason:'Vom Entwickler bearbeiteter Vorschlag. '+proposal.patch.reason};
    const content=validatePatch(proposal.base,this.state.plan,patch,proposal.revision);this.checkContract(patch);
    (this.state.proposalHistory??=[]).push(structuredClone(proposal));
    this.state.proposal={...proposal,patch,content};if(this.state.simulation)this.state.simulation.stale=true;
    this.record('human','Bearbeiteten Entwurf erneut gegen den Contract geprüft; noch nicht übernommen');await this.persist();await this.preview();
  }

  async focusSimulation(index:number,side:'baseline'|'proposed'):Promise<void> {
    const session=this.state.simulation;
    if (!session||session.stale||!Number.isInteger(index)||!['baseline','proposed'].includes(side)) { throw new Error('Bitte zuerst einen aktuellen Simulationsschritt wählen.'); }
    const trace=side==='baseline'?session.baseline:session.proposed,step=trace?.steps[index];
    if (!step) { throw new Error('Dieser Simulationsschritt existiert nicht.'); }
    let uri=vscode.Uri.parse(session.uri);
    if(side==='proposed') {
      const key=`${PREVIEW_SCHEME}:/simulation-${session.id}/${path.basename(uri.fsPath)}`;
      this.previewDocuments.set(key,session.proposed_content!);uri=vscode.Uri.parse(key);
    } else {
      const doc=await vscode.workspace.openTextDocument(uri);
      if(doc.getText()!==session.baseline_content){session.stale=true;await this.persist();throw new Error('Der Code hat sich seit der Simulation geändert.');}
    }
    const range=new vscode.Range(step.line-1,0,step.line-1,0);
    if(this.state.simulation!==session||session.stale)throw new Error('Das Szenario ist inzwischen veraltet. Bitte neu simulieren.');
    const editor=await vscode.window.showTextDocument(uri,{preserveFocus:true,preview:true,selection:range});
    if(this.state.simulation!==session||session.stale)throw new Error('Das Szenario ist inzwischen veraltet. Bitte neu simulieren.');
    if(side==='baseline'&&editor.document.getText()!==session.baseline_content){session.stale=true;await this.persist();throw new Error('Der Code hat sich seit der Simulation geändert.');}
    editor.setDecorations(this.simulationDecoration,[range]);editor.revealRange(range,vscode.TextEditorRevealType.InCenterIfOutsideViewport);
  }

  async stopSimulation():Promise<void> {
    for(const editor of vscode.window.visibleTextEditors)editor.setDecorations(this.simulationDecoration,[]);
    this.record('human','Simulation gestoppt');await this.persist();
  }

  async saveSimulationLearning(id:string,text:string):Promise<void> {
    const session=this.state.simulation;
    if(!session||session.id!==id||session.stale||session.step_id!==this.state.activePlanStepId)throw new Error('Bitte ein aktuelles, zum Planschritt gehörendes Szenario verwenden.');
    await this.saveDecision({kind:'test_idea',text,reason:`Simulation ${session.id}, Code ${session.baseline_hash}, Szenario v${session.scenario.version}. Noch kein realer Test.`});
  }

  async usePlanStep(id: string): Promise<void> {
    const changePlan = this.state.changePlan;
    if (!changePlan || changePlan.stale) { throw new Error('Der Änderungsplan fehlt oder ist veraltet.'); }
    await this.assertExplorationCurrent();
    const step = changePlan.steps.find(item => item.id === id);
    if (!step) { throw new Error('Dieser Planschritt existiert nicht mehr.'); }
    // Erst den alten Fokus lösen, damit ein Schrittwechsel den vorherigen
    // Schritt nicht mit dem Ziel des nächsten überschreibt.
    this.state.activePlanStepId = undefined;
    await this.updatePlan({ goal: step.title, not_now: [changePlan.not_now,step.details?.not_now].filter(Boolean).join('\n'), acceptance: step.acceptance });
    // Ein ausgewählter Planschritt ist noch kein freigegebener Codebereich.
    this.state.scope = undefined;
    if(this.state.contract)(this.state.contractHistory??=[]).push(structuredClone(this.state.contract));
    this.state.contract = undefined;
    this.state.activePlanStepId = id;
    this.record('human', `Planschritt ${id} aktiviert; Codebereich muss separat gewählt werden`);
    await this.persist();
  }

  async openSource(index: number): Promise<void> {
    const analysis = this.state.exploration;
    const source = analysis?.sources[index];
    if (!analysis || !source || !Number.isInteger(index)) { throw new Error('Quelle nicht verfügbar.'); }
    const folder = this.folderForRoot(analysis.root_uri);
    await this.client.request('check_path', { path: source.path }, undefined, folder.uri.fsPath);
    const uri = vscode.Uri.joinPath(folder.uri, ...source.path.split('/'));
    await vscode.window.showTextDocument(uri, { selection: new vscode.Range(Math.max(0,source.start_line-1),0,Math.max(0,source.start_line-1),0), preview: true });
  }

  private renderRepositorySources(): void {
    this.neighbors = this.state.exploration?.sources.map((source, index) => {
      const item = new vscode.TreeItem(`${source.path}:${source.start_line}`);
      item.description = `${source.kind === 'instruction' ? 'Projektregel' : source.kind === 'test' ? 'Testquelle · nicht ausgeführt' : 'Quelltext'} · ${source.sha256.slice(0,10)}${source.editor ? ' · Editorstand' : ''}`;
      item.command = { title: 'Belegte Quelle öffnen', command: 'tandem.openRepositorySource', arguments: [index] };
      return item;
    }) ?? [];
    this.neighborChanges.fire();
  }

  async capture(): Promise<void> {
    const editor = vscode.window.activeTextEditor;
    if (!editor || editor.document.uri.scheme !== 'file') { throw new Error('Bitte eine echte Codedatei im Editor auswählen.'); }
    const folder = vscode.workspace.getWorkspaceFolder(editor.document.uri);
    if (!folder) { throw new Error('Bitte die Datei in einem geöffneten Projektordner verwenden.'); }
    const doc = editor.document;
    await this.client.request('check_path', { path: path.relative(folder.uri.fsPath, doc.uri.fsPath).replace(/\\/g, '/') }, undefined, folder.uri.fsPath);
    let range: vscode.Range = editor.selection;
    let label = 'Ausgewählter Code';
    if (range.isEmpty) {
      const symbols = await vscode.commands.executeCommand<(vscode.DocumentSymbol | vscode.SymbolInformation)[]>('vscode.executeDocumentSymbolProvider', doc.uri) ?? [];
      const matching: { name: string; range: vscode.Range }[] = [];
      const visit = (symbol: vscode.DocumentSymbol | vscode.SymbolInformation) => {
        const symbolRange = 'range' in symbol ? symbol.range : symbol.location.range;
        if (symbolRange.contains(editor.selection.active)) { matching.push({ name: symbol.name, range: symbolRange }); }
        if ('children' in symbol) { symbol.children.forEach(visit); }
      };
      symbols.forEach(visit);
      matching.sort((a, b) => (doc.offsetAt(a.range.end) - doc.offsetAt(a.range.start)) - (doc.offsetAt(b.range.end) - doc.offsetAt(b.range.start)));
      if (!matching.length) { throw new Error('Bitte den erlaubten Codebereich markieren. Ein Sprachdienst kann alternativ die Methode erkennen.'); }
      range = matching[0].range;
      label = matching[0].name;
    }
    const text = doc.getText();
    if (Buffer.byteLength(text, 'utf8') > 500_000) { throw new Error('Für den Prototyp bitte eine Datei unter 500 KB wählen.'); }
    const start = doc.offsetAt(range.start), end = doc.offsetAt(range.end);
    this.state.scope = { uri: doc.uri.toString(), path: vscode.workspace.asRelativePath(doc.uri, true),
      prefix: text.slice(0, start), editable: text.slice(start, end), suffix: text.slice(end), version: doc.version };
    if(this.state.humanChange){(this.state.humanChangeHistory??=[]).push(structuredClone(this.state.humanChange));this.state.humanChange=undefined;}
    if(this.state.simulation){this.state.simulation.stale=true;}
    this.state.plan = { ...this.state.plan, revision: this.state.plan.revision + 1,
      goal: this.state.plan.goal || `Gewünschtes Verhalten in ${label} klären` };
    if(this.state.contract)(this.state.contractHistory??=[]).push(structuredClone(this.state.contract));
    this.state.contract=undefined;bindContract(this.state);
    if (this.state.proposal?.status === 'ready') { this.state.proposal.status = 'stale'; }
    this.state.suggestion = undefined;
    if (this.state.evidence) { this.state.evidence.status = 'stale'; }
    this.record('human', `Erlaubten Bereich gewählt: ${this.state.scope.path}:${range.start.line + 1}–${range.end.line + 1}`);
    this.error = '';
    await this.persist();
    await vscode.commands.executeCommand('tandem.workbench.focus');
    await this.loadNeighbors(doc, range.start, label);
  }

  private async loadNeighbors(doc: vscode.TextDocument, position: vscode.Position, label: string): Promise<void> {
    const item = new vscode.TreeItem(`${label} · ${path.basename(doc.fileName)}`);
    item.description = 'Aktueller Codebereich';
    item.command = { title: 'Code öffnen', command: 'vscode.open', arguments: [doc.uri, { selection: new vscode.Range(position, position) }] };
    this.neighbors = [item];
    this.neighborChanges.fire();
    // Ein nicht antwortender Sprachdienst darf den gemeinsamen Schritt nicht blockieren.
    const refs = await Promise.race([
      vscode.commands.executeCommand<vscode.Location[]>('vscode.executeReferenceProvider', doc.uri, position),
      new Promise<undefined>(resolve => setTimeout(() => resolve(undefined), 1500)),
    ]).catch(() => undefined);
    if (this.state.scope?.uri !== doc.uri.toString()) { return; }
    for (const ref of (refs ?? []).slice(0, 20)) {
      const node = new vscode.TreeItem(`${vscode.workspace.asRelativePath(ref.uri)}:${ref.range.start.line + 1}`);
      node.description = 'Referenz · Sprachdienst';
      node.command = { title: 'Referenz öffnen', command: 'vscode.open', arguments: [ref.uri, { selection: ref.range }] };
      this.neighbors.push(node);
    }
    if (!refs?.length) { const info = new vscode.TreeItem('Keine Referenzen vom Sprachdienst verfügbar'); info.description = 'Keine Vollständigkeitsgarantie'; this.neighbors.push(info); }
    this.neighborChanges.fire();
  }

  async send(prompt: string, mode: 'pair' | 'explore' = 'pair'): Promise<RunResult> {
    if(mode==='pair'&&this.state.scopeSuggestion)throw new Error('Bitte zuerst die vorgeschlagene Scope-Erweiterung gemeinsam entscheiden.');
    if(mode==='pair'&&this.state.contract?.mode==='delegate'&&!this.delegationActive)throw new Error('Bitte Delegate mit den vereinbarten Regeln starten oder zu Pair wechseln.');
    if(mode==='pair'&&this.state.contract?.mode==='explore')throw new Error('Der aktuelle Contract erlaubt nur lesende Analyse. Bitte ausdrücklich Pair wählen.');
    if(mode==='pair'&&!this.state.plan.acceptance.trim())throw new Error('Bitte vor dem Patch festlegen, woran der Erfolg dieses Schritts erkennbar ist.');
    if (this.state.activePlanStepId && this.state.changePlan?.stale) { throw new Error('Bitte zuerst den Quellenstand des aktiven Planschritts prüfen.'); }
    if (!vscode.workspace.isTrusted) { throw new Error('TANDEM benötigt einen vertrauenswürdigen Workspace.'); }
    if (this.busy || this.verifying) { throw new Error('Ein Lauf oder eine Prüfung ist noch aktiv.'); }
    if (typeof prompt !== 'string' || !prompt.trim()) { throw new Error('Bitte eine Frage oder einen Auftrag eingeben.'); }
    const scope = this.state.scope;
    if (!scope) { throw new Error('Zuerst einen erlaubten Codebereich wählen.'); }
    const doc = await vscode.workspace.openTextDocument(vscode.Uri.parse(scope.uri));
    const folder = vscode.workspace.getWorkspaceFolder(doc.uri);
    if (!folder) { throw new Error('Das zugehörige Repository ist nicht mehr geöffnet.'); }
    await this.client.request('check_path', { path: path.relative(folder.uri.fsPath, doc.uri.fsPath).replace(/\\/g, '/') }, undefined, folder.uri.fsPath);
    if (doc.getText() !== contentOf(scope)) { throw new Error('Der Code wurde geändert. Bitte den erlaubten Bereich erneut auswählen.'); }
    if (this.busy || this.verifying) { throw new Error('Ein Lauf oder eine Prüfung ist noch aktiv.'); }
    const plan = { ...this.state.plan };
    const generation = ++this.generation;
    const history = this.state.chat.slice(-12);
    this.busy = true; this.stream = ''; this.error = '';
    this.state.chat.push({ role: 'user', text: prompt });
    if(mode==='pair'&&this.state.proposal?.status==='ready'){(this.state.proposalHistory??=[]).push(structuredClone(this.state.proposal));this.state.proposal.status='stale';}
    try {
      await this.persist();
      const snapshot = { path: scope.path, prefix: scope.prefix, editable: scope.editable, suffix: scope.suffix };
      const superseded=new Set(this.state.decisions?.map(item=>item.supersedes).filter(Boolean));
      const contractContext = {contract:this.state.contract,step:activeStep(this.state),decisions:this.state.decisions?.filter(item => !superseded.has(item.id)&&(!item.step_id || item.step_id === this.state.activePlanStepId))};
      const explanation=vscode.workspace.getConfiguration('tandem').get('explanationLevel','normal');
      const boundedPrompt = prompt + '\n\nGewünschte Erklärungstiefe: '+explanation+'. Nenne Risiken und eine kleinere Alternative, wenn relevant.\nVereinbarter Schritt und dokumentierte Entscheidungen (keine Erweiterung der Schreibrechte):\n' + JSON.stringify(contractContext);
      const rejectedCalls=new Set<string>();
      const result = await this.client.request<RunResult>('run', { plan, snapshot, mode, prompt:boundedPrompt, history }, event => {
        if (generation !== this.generation) { return; }
        const call=event.data?.tool_call;
        if(call?.name==='propose_patch') {
          try{
            const content=validatePatch(scope,plan,call.args,plan.revision);this.checkContract(call.args);
            if(plan.revision===this.state.plan.revision){this.liveCandidate={patch:structuredClone(call.args),base:scope,content,revision:plan.revision,status:'ready'};this.render();}
            if(vscode.workspace.getConfiguration('tandem').get('liveDiff',true))void this.showLivePreview(scope,plan,content,generation).catch(error=>{this.error=error instanceof Error?error.message:String(error);this.render();});
          }catch(error){rejectedCalls.add(event.call_id);this.record('runtime','Contract-Abweichung im laufenden Vorschlag abgewehrt: '+(error instanceof Error?error.message:String(error)));this.render();}
        }
        const toolError=event.data?.error;
        if(toolError?.name==='propose_patch'&&!rejectedCalls.has(event.call_id)){rejectedCalls.add(event.call_id);this.record('runtime','Contract-Abweichung vom Adapter abgewehrt: '+toolError.error);}
        if (typeof event.data?.text_delta === 'string') { this.stream += event.data.text_delta; this.renderStream(); }
      }, folder.uri.fsPath);
      if (generation !== this.generation) { throw new Error('Lauf wurde gestoppt.'); }
      const current = await vscode.workspace.openTextDocument(vscode.Uri.parse(scope.uri));
      const stale = this.state.plan.revision !== plan.revision || current.getText() !== contentOf(scope);
      if (result.patch) {
        const content = validatePatch(scope, plan, result.patch, result.plan_revision);
        if(!stale)this.checkContract(result.patch);
        if(this.state.proposal)(this.state.proposalHistory??=[]).push(structuredClone(this.state.proposal));
        this.state.proposal = { patch: result.patch, base: scope, content, revision: plan.revision, status: stale ? 'stale' : 'ready' };
        const step = activeStep(this.state); if (step && !stale) { step.status = 'preview'; }
        this.record('agent', stale ? 'Patch eingetroffen, aber inzwischen veraltet' : 'Patch vorgeschlagen, noch nicht übernommen');
      }
      this.state.suggestion = stale ? undefined : result.plan_suggestion;
      if(result.scope_suggestion&&!stale)this.state.scopeSuggestion=result.scope_suggestion;
      if(result.scope_suggestion&&!stale){if(this.state.proposal?.status==='ready')this.state.proposal.status='stale';this.record('agent','Scope-Erweiterung begründet vorgeschlagen; Codearbeit gestoppt');}
      this.state.chat.push({ role: 'assistant', text: result.answer });
      this.state.chat = this.state.chat.slice(-24);
      if (stale) { this.error = 'Plan oder Code wurde während des Laufs geändert. Der Vorschlag ist veraltet.'; }
      return result;
    } finally {
      if (generation === this.generation) { this.busy = false; this.stream = '';this.liveCandidate=undefined; await this.persist(); }
    }
  }

  stop(): void {
    if(this.state.factoryJob?.status==='running'){this.state.factoryJob.status='stopped';this.state.factoryJob.error='Worker gestoppt; verspätete Ergebnisse werden verworfen.';}
    this.stopSequence++;
    this.liveCandidate=undefined;
    this.delegationSequence++;
    if(this.state.delegation?.status==='running'){this.state.delegation.status='stopped';this.state.delegation.reason='Vom Entwickler gestoppt.';}
    ++this.generation;
    this.busy = false; this.stream = '';
    this.client.stop();
    this.verification?.terminate();
    if(this.state.evidence?.status==='running')this.state.evidence.status='stale';
    this.record('human', 'Agentenlauf gestoppt');
    void this.persist();
  }

  async pausePreview():Promise<void> {
    const candidate=this.liveCandidate;if(!candidate)throw new Error('Es gibt noch keinen vollständig vorliegenden, geprüften Zwischenstand.');
    this.stop();const doc=await vscode.workspace.openTextDocument(vscode.Uri.parse(candidate.base.uri));assertCurrent(candidate,this.state.plan,doc.getText());this.checkContract(candidate.patch);
    if(this.state.proposal)(this.state.proposalHistory??=[]).push(structuredClone(this.state.proposal));this.state.proposal=candidate;
    this.record('human','Lauf pausiert und geprüften Zwischenstand als Vorschlag festgehalten; nichts übernommen');await this.persist();
  }

  private async showLivePreview(scope:Scope,plan:Plan,content:string,generation:number):Promise<void> {
    const doc=await vscode.workspace.openTextDocument(vscode.Uri.parse(scope.uri));
    if(generation!==this.generation||this.state.plan.revision!==plan.revision||doc.getText()!==contentOf(scope))return;
    const uri=vscode.Uri.parse(`${PREVIEW_SCHEME}:/live-${generation}/${path.basename(doc.fileName)}`),exists=this.previewDocuments.has(uri.toString());
    this.previewDocuments.set(uri.toString(),content);this.previewChanges.fire(uri);
    if(!exists)await vscode.commands.executeCommand('vscode.diff',doc.uri,uri,'TANDEM · Vorschau (nicht übernommen; Contract beachten)',{preview:true,preserveFocus:true});
  }

  async preview(): Promise<void> {
    const proposal = this.state.proposal;
    if (!proposal) { throw new Error('Noch kein Patch vorhanden.'); }
    const doc = await vscode.workspace.openTextDocument(vscode.Uri.parse(proposal.base.uri));
    assertCurrent(proposal, this.state.plan, doc.getText());
    const key = `${PREVIEW_SCHEME}:/r${proposal.revision}-${Date.now()}/${path.basename(doc.fileName)}`;
    this.previewDocuments.set(key, proposal.content);
    // Der echte Code bleibt neben dem Diff sichtbar und direkt bearbeitbar.
    const sourceColumn=vscode.window.visibleTextEditors.find(editor=>editor.document.uri.toString()===doc.uri.toString())?.viewColumn??this.codeColumn;
    await vscode.window.showTextDocument(doc,{viewColumn:sourceColumn,preview:false,preserveFocus:true});
    await vscode.commands.executeCommand('vscode.diff', doc.uri, vscode.Uri.parse(key), 'TANDEM · Aktuell ↔ Vorschlag (nicht übernommen)',{viewColumn:sourceColumn===vscode.ViewColumn.One?vscode.ViewColumn.Two:vscode.ViewColumn.One,preview:true});
  }

  async apply(): Promise<void> {
    const applyGeneration=this.generation;
    if (this.state.activePlanStepId && this.state.changePlan?.stale) { throw new Error('Bitte zuerst den Quellenstand des aktiven Planschritts prüfen.'); }
    if (this.busy || this.verifying) { throw new Error('Bitte den laufenden Vorgang zuerst beenden.'); }
    const proposal = this.state.proposal;
    if (!proposal) { throw new Error('Noch kein Patch vorhanden.'); }
    const doc = await vscode.workspace.openTextDocument(vscode.Uri.parse(proposal.base.uri));
    const folder = vscode.workspace.getWorkspaceFolder(doc.uri);
    if (!folder) { throw new Error('Das zugehörige Repository ist nicht mehr geöffnet.'); }
    await this.client.request('check_path', { path: path.relative(folder.uri.fsPath, doc.uri.fsPath).replace(/\\/g, '/') }, undefined, folder.uri.fsPath);
    assertCurrent(proposal, this.state.plan, doc.getText());
    validatePatch(proposal.base, this.state.plan, proposal.patch, proposal.revision);
    this.checkContract(proposal.patch);
    const editor = await vscode.window.showTextDocument(doc);
    // Nach dem asynchronen Öffnen erneut prüfen. TextEditor.edit bindet die
    // Änderung an die Dokumentversion des Editors und unterstützt natives Undo.
    assertCurrent(proposal, this.state.plan, doc.getText());
    const start = proposal.base.prefix.length + proposal.base.editable.indexOf(proposal.patch.old_text);
    if(applyGeneration!==this.generation)throw new Error('Die Übernahme wurde vor dem Schreiben gestoppt.');
    this.applyingContent=proposal.content;
    let applied:boolean;
    try{applied = await editor.edit(builder => builder.replace(new vscode.Range(doc.positionAt(start), doc.positionAt(start + proposal.patch.old_text.length)), proposal.patch.new_text), { undoStopBefore: true, undoStopAfter: true });}
    finally{this.applyingContent=undefined;}
    if (!applied || doc.getText() !== proposal.content) {
      proposal.status = 'stale'; await this.persist();
      throw new Error('Übernahme nicht eindeutig abgeschlossen. Bitte den aktuellen Code prüfen; kein automatisches Zurücksetzen.');
    }
    proposal.status = 'applied';
    const step = activeStep(this.state); if (step) { step.status = 'applied'; }
    this.state.scope = { ...proposal.base, editable: proposal.base.editable.replace(proposal.patch.old_text, () => proposal.patch.new_text), version: doc.version };
    bindContract(this.state);
    if(this.state.evidence)(this.state.evidenceHistory??=[]).push(structuredClone(this.state.evidence));
    this.state.evidence = undefined;
    this.record('human', 'Patch im Editor übernommen; noch nicht real verifiziert');
    await this.persist();
  }

  async discard(): Promise<void> {
    if (this.state.proposal?.status !== 'ready') { throw new Error('Kein aktueller Vorschlag zum Verwerfen.'); }
    this.state.proposal.status = 'discarded';
    this.record('human', 'Vorschlag verworfen');
    await this.persist();
  }

  async verify(): Promise<Evidence> {
    if (this.busy || this.verifying) { throw new Error('Ein Vorgang ist noch aktiv.'); }
    this.verifying = true;
    this.render();
    try { return await this.runVerification(); }
    finally {
      this.verifying = false; this.verification = undefined;
      await this.persist();
    }
  }

  private async runVerification(): Promise<Evidence> {
    const generation=this.generation,planRevision=this.state.plan.revision,contractVersion=this.state.contract?.version;
    const assertActive=()=>{if(generation!==this.generation)throw new Error('Die reale Prüfung wurde gestoppt.');};
    if (!vscode.workspace.isTrusted) { throw new Error('Prüfprogramme benötigen einen vertrauenswürdigen Workspace.'); }
    const scope = this.state.scope;
    if (!scope) { throw new Error('Zuerst einen Codebereich wählen.'); }
    const uri = vscode.Uri.parse(scope.uri);
    const folder = vscode.workspace.getWorkspaceFolder(uri);
    if (!folder) { throw new Error('Projektordner nicht mehr geöffnet.'); }
    const config = vscode.workspace.getConfiguration('tandem', uri);
    const program = this.delegateVerification?.program??config.get<string>('verificationProgram', '').trim();
    const args = this.delegateVerification?.args??config.get<string[]>('verificationArgs', []);
    if (!program) {
      await vscode.commands.executeCommand('workbench.action.openSettings', 'tandem.verification');
      throw new Error('Prüfprogramm und Argumente in den TANDEM-Einstellungen festlegen.');
    }
    await this.client.request('check_path',{path:path.relative(folder.uri.fsPath,uri.fsPath).replace(/\\/g,'/')},undefined,folder.uri.fsPath);assertActive();
    const doc = await vscode.workspace.openTextDocument(uri);assertActive();
    if (!await doc.save()) { throw new Error('Die Datei konnte vor der Prüfung nicht gespeichert werden.'); }
    assertActive();
    const unsaved=await this.unsavedSources(folder,uri);if(unsaved.length){if(this.state.evidence){this.state.evidence.status='stale';this.state.evidence.validation_error='Weitere Quelldateien sind ungespeichert: '+unsaved.join(', ');}throw new Error('Weitere Quelldateien sind ungespeichert: '+unsaved.join(', ')+'. Bitte selbst speichern, damit der reale Test den sichtbaren Code prüft.');}
    const before=await this.client.request<WorkspaceSnapshot>('workspace_snapshot',{},undefined,folder.uri.fsPath);
    assertActive();
    const hash = (bytes: Uint8Array) => createHash('sha256').update(bytes).digest('hex');
    // Editorinhalt und gespeicherte Bytes sind bei BOM oder UTF-16 verschieden.
    // Deshalb die gespeicherte Revision unabhängig von ihrer Kodierung binden.
    const diskHash = hash(await vscode.workspace.fs.readFile(uri));
    const evidence: Evidence = { status: 'running', command: JSON.stringify([program, ...args]), at: new Date().toISOString(), content: doc.getText(), uri: scope.uri, diskHash };
    evidence.plan_revision=planRevision;evidence.contract_version=contractVersion;
    evidence.workspace={root_uri:folder.uri.toString(),before_hash:before.fingerprint,file_count:Object.keys(before.files).length,byte_count:before.bytes,changed_paths:[],coverage:before.coverage};
    if(this.state.evidence)(this.state.evidenceHistory??=[]).push(structuredClone(this.state.evidence));
    this.state.evidence = evidence;
    this.record('human', `Reale Prüfung gestartet: ${evidence.command}`);
    await this.persist();
    try {
      if(generation!==this.generation)throw new Error('Die Prüfung wurde vor dem Programmstart gestoppt.');
      const task = new vscode.Task({ type: 'tandem', revision: this.state.plan.revision }, folder, 'TANDEM · reale Prüfung', 'TANDEM',
        new vscode.ProcessExecution(program, args, { cwd: folder.uri.fsPath }));
      task.presentationOptions = { reveal: vscode.TaskRevealKind.Always, panel: vscode.TaskPanelKind.Dedicated };
      const exitCode = await new Promise<number | undefined>(async (resolve, reject) => {
        let execution: vscode.TaskExecution | undefined;
        const subscription = vscode.tasks.onDidEndTaskProcess(event => {
          if (event.execution === execution) { subscription.dispose(); clearTimeout(timeout); resolve(event.exitCode); }
        });
        const timeout = setTimeout(() => { subscription.dispose(); execution?.terminate(); reject(new Error('Prüfung nach fünf Minuten abgebrochen.')); }, 300_000);
        try { execution = await vscode.tasks.executeTask(task); this.verification = execution;if(generation!==this.generation)execution.terminate(); }
        catch (error) { subscription.dispose(); clearTimeout(timeout); reject(error); }
      });
      evidence.exitCode = exitCode;
      const currentHash = hash(await vscode.workspace.fs.readFile(uri));
      const after=await this.client.request<WorkspaceSnapshot>('workspace_snapshot',{},undefined,folder.uri.fsPath),dirty=await this.unsavedSources(folder,uri);
      evidence.workspace.after_hash=after.fingerprint;evidence.workspace.changed_paths=changedWorkspaceFiles(before,after);
      evidence.status = this.state.plan.revision!==planRevision||this.state.contract?.version!==contractVersion||before.fingerprint!==after.fingerprint||dirty.length>0||currentHash !== diskHash || doc.getText() !== evidence.content || doc.isDirty || this.state.evidence !== evidence || evidence.status === 'stale' ? 'stale' : exitCode === 0 ? 'passed' : 'failed';
      if(dirty.length)evidence.validation_error='Während der Prüfung ungespeicherte Quelldateien geändert: '+dirty.join(', ');
      evidence.at = new Date().toISOString();
      const step = activeStep(this.state); if (step && evidence.status === 'passed') { step.status = 'verified'; }
      this.record('runtime', `Prüfprogramm beendet: ${evidence.status}, Exit-Code ${exitCode ?? 'unbekannt'}`);
      return evidence;
    } catch (error) {
      evidence.status = evidence.exitCode===undefined?'failed':'stale';evidence.validation_error=error instanceof Error?error.message:String(error);
      this.record('runtime', 'Reale Prüfung konnte nicht abgeschlossen werden');
      throw error;
    }
  }

  private async unsavedSources(folder:vscode.WorkspaceFolder,except:vscode.Uri):Promise<string[]> {
    const paths=vscode.workspace.textDocuments.filter(doc=>doc.isDirty&&doc.uri.scheme==='file'&&doc.uri.toString()!==except.toString()&&vscode.workspace.getWorkspaceFolder(doc.uri)?.uri.toString()===folder.uri.toString()).map(doc=>path.relative(folder.uri.fsPath,doc.uri.fsPath).replace(/\\/g,'/'));
    if(!paths.length)return[];
    if(paths.length>100)throw new Error('Mehr als 100 ungespeicherte Projektdateien. Bitte vor der realen Prüfung selbst speichern.');
    const checked=await this.client.request<{allowed:string[]}>('check_paths',{paths},undefined,folder.uri.fsPath);return checked.allowed;
  }

  dispose(): void {
    clearTimeout(this.renderTimer);
    this.client.dispose(); this.mapClient.dispose();this.verification?.terminate();
    this.output.dispose(); this.neighborChanges.dispose();
    this.previewChanges.dispose();
    this.simulationDecoration.dispose();
    this.codeMap.dispose();
    this.graphView.dispose();
    this.disposables.forEach(disposable => disposable.dispose());
  }
}

export async function activate(context: vscode.ExtensionContext): Promise<TandemController> {
  let saved: State | undefined;
  if (context.storageUri) {
    try {
      saved = JSON.parse(Buffer.from(await vscode.workspace.fs.readFile(vscode.Uri.joinPath(context.storageUri, 'step.json'))).toString('utf8'));
    } catch (error) {
      if (!(error instanceof vscode.FileSystemError && error.code === 'FileNotFound')) { throw error; }
    }
  }
  const controller = new TandemController(context, saved);
  context.subscriptions.push(controller);
  return controller;
}
