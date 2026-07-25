//! Wer was sehen und schreiben darf — und wer der Schreiber ist.
//!
//! Der [`GraphAccess`] ist das einzige Stück Laufzeit-Identität im Crate. Alle
//! Tools bekommen ihn als geschlossene Closure-Variable: **das Modell kann weder
//! seinen Autor noch seinen Ziel-Scope wählen**, es liefert nur Inhalt. Damit ist
//! die Anforderung „keine Identitätsfälschung" strukturell erfüllt, genau wie in
//! agentkit_swarm, wo `from` immer aus dem Tool-Kontext kommt.

use crate::model::{GraphLayer, GraphScope, GraphTarget, GraphView};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphAccess {
    /// Autor jeder Mutation (`created_by`) — Agent-ID oder `"runtime"`.
    pub principal: String,
    /// Lauf-ID; landet in der Quelle jeder Mutation.
    pub run_id: Option<String>,
    /// Was gelesen werden darf (Reihenfolge = Priorität).
    pub view: GraphView,
    /// Wohin geschrieben wird. `None` = nur lesen; dann registriert
    /// [`crate::register_graph_tools`] auch keine Schreib-Tools.
    pub write: Option<GraphTarget>,
    /// Wohin promotet werden darf. `None` = keine Promotion.
    pub promote: Option<GraphTarget>,
}

impl GraphAccess {
    /// Nur-Lese-Zugriff auf eine Sicht.
    pub fn read_only(principal: &str, view: GraphView) -> Self {
        GraphAccess {
            principal: principal.to_string(),
            run_id: None,
            view,
            write: None,
            promote: None,
        }
    }

    /// Der Normalfall: lesen wie in `view`, schreiben in einen Working-Scope.
    ///
    /// Der Schreib-Scope wird der Sicht **vorangestellt**, falls er noch nicht
    /// darin vorkommt — ein Agent muss seine eigenen Schreibvorgänge sehen
    /// (Read-your-writes), und die eigene Arbeit hat die höchste Priorität.
    pub fn working(principal: &str, view: GraphView, write: GraphTarget) -> Self {
        let mut view = view;
        if !view.sees(write.layer, &write.scope) {
            view.targets.insert(0, write.clone());
        }
        GraphAccess {
            principal: principal.to_string(),
            run_id: None,
            view,
            write: Some(write),
            promote: None,
        }
    }

    /// Erlaubt Promotion in ein Ziel — üblicherweise `canonical@workspace:…`.
    /// Das Ziel wird der Sicht angehängt, falls es fehlt: was man kanonisieren
    /// darf, muss man danach auch lesen können.
    pub fn with_promotion(mut self, target: GraphTarget) -> Self {
        if !self.view.sees(target.layer, &target.scope) {
            self.view.targets.push(target.clone());
        }
        self.promote = Some(target);
        self
    }

    /// Derselbe Zugriff unter anderem Autor.
    ///
    /// Für Schwarm-Mitglieder: sie teilen sich Sicht und Arbeits-Scope (das IST
    /// das gemeinsame Gedächtnis), schreiben aber jeder unter eigenem Namen —
    /// sonst stünde in der Provenance nur „der Schwarm".
    pub fn as_principal(&self, principal: &str) -> Self {
        GraphAccess {
            principal: principal.to_string(),
            ..self.clone()
        }
    }

    pub fn with_run_id(mut self, run_id: &str) -> Self {
        self.run_id = Some(run_id.to_string());
        self
    }

    pub fn can_write(&self) -> bool {
        self.write.is_some()
    }

    pub fn can_promote(&self) -> bool {
        self.promote.is_some()
    }

    /// Bequemer Standardaufbau für einen einzelnen Agenten: vorläufig in die
    /// Session schreiben, kanonisches Workspace-Wissen mitlesen und dorthin
    /// promotieren dürfen.
    pub fn session(principal: &str, workspace: &str, session_id: &str) -> Self {
        let canonical = GraphTarget::canonical(GraphScope::workspace(workspace));
        let working = GraphTarget::new(GraphLayer::Working, GraphScope::session(session_id));
        GraphAccess::working(principal, GraphView::new(vec![canonical.clone()]), working)
            .with_promotion(canonical)
            .with_run_id(session_id)
    }

    /// Standardaufbau für ein Schwarm-Mitglied: gemeinsamer Working Graph des
    /// Laufs plus kanonisches Workspace-Wissen. Alle Mitglieder bekommen
    /// denselben Schreib-Scope — das IST das geteilte Arbeitsgedächtnis.
    pub fn swarm(principal: &str, workspace: &str, run_id: &str) -> Self {
        let canonical = GraphTarget::canonical(GraphScope::workspace(workspace));
        let shared = GraphTarget::new(GraphLayer::Working, GraphScope::swarm(run_id));
        GraphAccess::working(principal, GraphView::new(vec![canonical.clone()]), shared)
            .with_promotion(canonical)
            .with_run_id(run_id)
    }
}
