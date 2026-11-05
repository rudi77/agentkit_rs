export interface Plan { revision: number; goal: string; not_now: string; acceptance: string }
export interface Snapshot { path: string; prefix: string; editable: string; suffix: string }
export interface Scope extends Snapshot { uri: string; version: number }
export interface Patch { path: string; old_text: string; new_text: string; reason: string }
export interface PlanSuggestion { goal: string; not_now: string; acceptance: string; reason: string }
export interface RunResult { answer: string; patch?: Patch; plan_suggestion?: PlanSuggestion; plan_revision: number; scope_suggestion?: ScopeSuggestion }
export interface ScopeSuggestion {reason:string;files:string[];symbols:string[];alternative:string;risk:string}
export interface Proposal { patch: Patch; base: Scope; content: string; revision: number; status: 'ready' | 'stale' | 'applied' | 'discarded' }
export interface Evidence { status: 'running' | 'passed' | 'failed' | 'stale'; command: string; exitCode?: number; at: string; content: string; uri: string; diskHash: string;workspace?:import('./verification').WorkspaceEvidence;validation_error?:string;plan_revision?:number;contract_version?:number }
export interface RecordEntry { at: string; by: 'human' | 'agent' | 'runtime'; action: string; revision: number }
export interface Source { id: string; path: string; sha256: string; start_line: number; end_line: number; kind: string; editor: boolean }
export interface Overlay { path: string; content: string }
export type StepStatus = 'idea' | 'clarifying' | 'ready' | 'preview' | 'applied' | 'verified' | 'accepted' | 'deferred';
export interface StepDetails {
  needs_now: string; not_now: string; smallest_change: string; reuse: string;
  invariants: string; assumptions: string; risks: string;
}
export interface ChangeStep { id: string; title: string; why: string; files: string[]; acceptance: string; source_ids: string[]; details?: StepDetails; status?: StepStatus }
export interface Decision {
  id: string; at: string; by: 'human' | 'agent'; step_id?: string;
  kind: 'need' | 'exclude' | 'decision' | 'assumption' | 'risk' | 'test_idea';
  text: string; reason: string; plan_revision: number; supersedes?: string;
}
export interface PlanRevision { at: string; by: 'human' | 'agent'; reason: string; plan: SharedChangePlan }
export interface StepAcceptance { step_id: string; at: string; plan_revision: number; rationale: string; evidence_at?: string; kind: 'behavior' | 'clarification' }
export interface ChangePlan { goal: string; not_now: string; open_questions: string; steps: ChangeStep[] }
export interface Exploration {
  answer: string; suggested_plan?: ChangePlan; sources: Source[]; instruction_paths: string[];
  catalog_id: string; stale: boolean; warnings: string[]; plan_revision: number;
}
export interface RepositoryAnalysis extends Exploration { root_uri: string; task: string; created_at: string; overlays: Overlay[] }
export interface SharedChangePlan extends ChangePlan { revision: number; stale: boolean; root_uri: string; catalog_id: string; sources: Source[]; basis_created_at?: string }
export interface State {
  sessionId?:string;
  archivedSessions?:{id:string;title:string;at:string}[];
  plan: Plan;
  scope?: Scope;
  proposal?: Proposal;
  suggestion?: PlanSuggestion;
  evidence?: Evidence;
  chat: { role: string; text: string }[];
  audit: RecordEntry[];
  analysisTask?: string;
  exploration?: RepositoryAnalysis;
  changePlan?: SharedChangePlan;
  activePlanStepId?: string;
  decisions?: Decision[];
  planHistory?: PlanRevision[];
  acceptances?: StepAcceptance[];
  simulation?: SimulationSession;
  simulationHistory?: SimulationSession[];
  contract?: import('./contract').ChangeContract;
  contractHistory?: import('./contract').ChangeContract[];
  contractConflict?: import('./contract').ContractConflict;
  proposalHistory?:Proposal[];
  evidenceHistory?:Evidence[];
  scopeSuggestion?:ScopeSuggestion;
  scopeSuggestionHistory?:{at:string;suggestion:ScopeSuggestion;resolution:string}[];
  delegation?:Delegation;
  delegationHistory?:Delegation[];
  evaluations?:import('./artifact').Evaluation[];
  architecture?:import('./architecture').Architecture;
  humanChange?:HumanChange;
  humanChangeHistory?:HumanChange[];
  dismissedPlanProposalAt?:string;
  factoryJob?:import('./factory').FactoryJobRecord;
  factoryHistory?:import('./factory').FactoryJobRecord[];
}
export interface HumanChange {at:string;uri:string;path:string;before:string;after:string;review?:{at:string;answer:string;snapshot_hash:string;stale:boolean}}
export interface Delegation {at:string;status:'running'|'proposed'|'verified'|'stopped'|'needs_attention';plan_revision:number;contract_version:number;max_changed_lines:number;auto_apply:boolean;verification_program:string;verification_args:string[];reason:string}
export interface SimulationSession {
  id:string; at:string; uri:string; path:string; baseline_hash:string; proposed_hash?:string;
  plan_revision:number; step_id?:string; scenario:Scenario; baseline:SimTrace; proposed?:SimTrace;
  divergence?:number; stale:boolean; baseline_content:string; proposed_content?:string;
  duration_ms?:number;
}

export function validateChangePlan(plan: ChangePlan, sources: Source[], requireEvidence: boolean): void {
  if (!plan || typeof plan.goal !== 'string' || !plan.goal.trim() || typeof plan.not_now !== 'string' || typeof plan.open_questions !== 'string' || !Array.isArray(plan.steps) || !plan.steps.length || plan.steps.length > 12) {
    throw new Error('Ein Änderungsplan benötigt ein Ziel und 1 bis 12 Schritte.');
  }
  const ids = new Set<string>();
  for (const step of plan.steps) {
    if (!step.id?.trim() || ids.has(step.id) || !step.title?.trim() || !step.why?.trim() || !step.acceptance?.trim() || !Array.isArray(step.files) || step.files.some(file => typeof file !== 'string' || /(^[/\\]|:|(^|[/\\])\.\.([/\\]|$))/.test(file))) {
      throw new Error('Jeder Schritt benötigt ID, Titel, Begründung, relative Dateipfade und ein Abnahmekriterium.');
    }
    ids.add(step.id);
    if (step.status && !['idea','clarifying','ready','preview','applied','verified','accepted','deferred'].includes(step.status)) { throw new Error('Unbekannter Schrittstatus.'); }
    if (step.details && ['needs_now','not_now','smallest_change','reuse','invariants','assumptions','risks'].some(key => typeof step.details![key as keyof StepDetails] !== 'string')) { throw new Error('Die Schrittdetails müssen vollständig als Text vorliegen.'); }
    if (!Array.isArray(step.source_ids) || (requireEvidence && !step.source_ids.length) || step.source_ids.some(id => !sources.some(source => source.id === id))) {
      throw new Error('Der Plan verweist auf nicht gelesene Quellen.');
    }
  }
}

export const contentOf = (scope: Snapshot): string => scope.prefix + scope.editable + scope.suffix;

// Dieselbe Grenze wie im Rust-Adapter. Der Editor vertraut keinem Modell- oder
// Prozessresultat genug, um dessen vorgeschlagenen Dateipfad direkt zu öffnen.
export function validatePatch(scope: Scope, plan: Plan, patch: Patch, revision: number): string {
  if (revision !== plan.revision) { throw new Error('Der Change Contract wurde inzwischen geändert.'); }
  if (patch.path !== scope.path) { throw new Error('Die vorgeschlagene Datei liegt außerhalb des Contracts.'); }
  if (typeof patch.old_text !== 'string' || !patch.old_text || scope.editable.indexOf(patch.old_text) < 0 || scope.editable.indexOf(patch.old_text) !== scope.editable.lastIndexOf(patch.old_text)) {
    throw new Error('Der alte Text muss im erlaubten Bereich genau einmal vorkommen.');
  }
  if (typeof patch.new_text !== 'string' || patch.old_text === patch.new_text || !patch.reason?.trim()) {
    throw new Error('Änderung oder Begründung fehlt.');
  }
  return scope.prefix + scope.editable.replace(patch.old_text, () => patch.new_text) + scope.suffix;
}

export function assertCurrent(proposal: Proposal, plan: Plan, text: string): void {
  if (proposal.status !== 'ready' || proposal.revision !== plan.revision || text !== contentOf(proposal.base)) {
    throw new Error('Der Vorschlag ist veraltet oder bereits verarbeitet. Bitte neu vorschlagen lassen.');
  }
}
import type { Scenario, SimTrace } from './simulation';
