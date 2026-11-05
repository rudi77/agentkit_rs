//! Portables Übergabeprotokoll: ein gebundener Snapshot hinein, ein Vorschlag
//! heraus. Der Worker erhält keine Produkt-Schreib-, Shell- oder Testwerkzeuge.

use crate::{
    repository::{fingerprint, protected, Repository},
    run, RunRequest, RunResult,
};
use agentkit::{AgentEvent, Cancel, Llm};
use serde::{Deserialize, Serialize};
use std::{io::Read, path::Path, sync::Arc};

pub const MAX_JOB_BYTES: usize = 1_000_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub plan_revision: u64,
    pub contract_id: String,
    pub contract_version: u64,
    pub baseline_sha256: String,
    pub disk_sha256: String,
    pub relative_path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    pub analysis: String,
    pub code_write: String,
    pub shell: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub id: String,
    pub version: u64,
    pub plan_revision: u64,
    pub step_id: Option<String>,
    pub baseline_hash: String,
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub mode: String,
    pub operations: Vec<String>,
    pub preserve_public_signatures: bool,
    pub allow_new_symbols: bool,
    pub needs_now: String,
    pub not_now: String,
    pub invariants: String,
    pub acceptance: String,
    pub permissions: Permissions,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryJob {
    pub schema: String,
    pub job_id: String,
    pub session_id: String,
    pub created_at: String,
    pub worker_policy: String,
    pub binding: Binding,
    pub contract: Contract,
    pub request: RunRequest,
}

pub struct PreparedJob {
    pub job: FactoryJob,
    pub job_sha256: String,
    pub workspace_check: &'static str,
}

#[derive(Debug, Serialize)]
pub struct FactoryResult {
    pub schema: &'static str,
    pub worker: &'static str,
    pub job_id: String,
    pub session_id: String,
    pub job_sha256: String,
    pub binding: Binding,
    pub workspace_check: &'static str,
    pub execution_authorization: &'static str,
    pub verification: &'static str,
    pub status: &'static str,
    pub result: RunResult,
}

/// Liest ausschließlich die ausdrücklich angegebene Übergabedatei begrenzt ein.
pub fn read_job(path: &Path) -> Result<Vec<u8>, String> {
    let file =
        std::fs::File::open(path).map_err(|e| format!("Übergabedatei nicht verfügbar: {e}"))?;
    let mut bytes = Vec::new();
    file.take(MAX_JOB_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_JOB_BYTES {
        return Err("Der Worker-Job überschreitet 1 MB.".into());
    }
    Ok(bytes)
}

fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && !protected(path)
        && !path.contains(':')
        && !path.starts_with('/')
        && !path.starts_with('\\')
        && !path.replace('\\', "/").split('/').any(|part| part == "..")
}
fn hash_shape(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Prüft Übergabe und optional den vom Aufrufer gebundenen Checkout vor jeder
/// Modellanfrage. Ein Pfad aus dem Job kann die Workspace-Wurzel nicht ersetzen.
pub fn prepare(bytes: &[u8], workspace: Option<&Path>) -> Result<PreparedJob, String> {
    if bytes.len() > MAX_JOB_BYTES {
        return Err("Der Worker-Job überschreitet 1 MB.".into());
    }
    let job: FactoryJob =
        serde_json::from_slice(bytes).map_err(|e| format!("Ungültiger Worker-Job: {e}"))?;
    if job.schema != "tandem.factory-job/v1"
        || job.worker_policy != "proposal_only"
        || job.job_id.trim().is_empty()
        || job.session_id.trim().is_empty()
    {
        return Err("Unbekanntes Übergabeformat oder unzulässige Worker-Rechte.".into());
    }
    let binding = &job.binding;
    let contract = &job.contract;
    let request = &job.request;
    if binding.plan_revision == 0
        || binding.contract_version == 0
        || binding.contract_id.is_empty()
        || binding.plan_revision != request.plan.revision
        || binding.plan_revision != contract.plan_revision
        || binding.contract_id != contract.id
        || binding.contract_version != contract.version
        || binding.baseline_sha256 != contract.baseline_hash
        || contract.path != request.snapshot.path
    {
        return Err(
            "Plan, Änderungsvertrag und Snapshot der Übergabe sind nicht konsistent gebunden."
                .into(),
        );
    }
    if !safe_path(&binding.relative_path)
        || !safe_path(&request.snapshot.path)
        || !hash_shape(&binding.baseline_sha256)
        || !hash_shape(&binding.disk_sha256)
        || fingerprint(request.snapshot.content().as_bytes()) != binding.baseline_sha256
    {
        return Err("Pfad oder Code-Hash des Worker-Jobs ist ungültig.".into());
    }
    let (mode, write) = match contract.mode.as_str() {
        "explore" => ("explore", "none"),
        "pair" => ("pair", "propose_only"),
        "delegate" => ("pair", "bounded_apply"),
        _ => return Err("Unbekannter Arbeitsmodus in der Übergabe.".into()),
    };
    if request.mode != mode
        || contract.permissions.analysis != "read_only"
        || contract.permissions.shell != "none"
        || contract.permissions.code_write != write
        || contract.operations != ["modify"]
        || contract.start_line != request.snapshot.prefix.split('\n').count()
        || contract.end_line
            != format!("{}{}", request.snapshot.prefix, request.snapshot.editable)
                .split('\n')
                .count()
    {
        return Err(
            "Die Übergabe verändert den erlaubten Bereich oder erweitert Worker-Befugnisse.".into(),
        );
    }
    if request.plan.goal.trim().is_empty()
        || request.snapshot.editable.is_empty()
        || request.snapshot.content().len() > 500_000
        || request.prompt.len() > 32_000
        || contract.acceptance != request.plan.acceptance
        || (mode == "pair" && request.plan.acceptance.trim().is_empty())
    {
        return Err("Ziel, Akzeptanzkriterium oder unterstützte Snapshot-/Eingabegröße der Übergabe ist ungültig.".into());
    }
    let workspace_check = if let Some(root) = workspace {
        let repo = Repository::open(root, vec![])?;
        let file = repo.check_path(&binding.relative_path)?;
        let mut disk = Vec::new();
        std::fs::File::open(file)
            .map_err(|e| e.to_string())?
            .take(1_000_001)
            .read_to_end(&mut disk)
            .map_err(|e| e.to_string())?;
        if disk.len() > 1_000_000 || fingerprint(&disk) != binding.disk_sha256 {
            return Err("Der Worker-Checkout weicht vom gespeicherten Übergabestand ab.".into());
        }
        "disk_hash_matched"
    } else {
        "not_requested"
    };
    Ok(PreparedJob {
        job,
        job_sha256: fingerprint(bytes),
        workspace_check,
    })
}

/// Verwendet den normalen TANDEM-Loop mit dessen begrenzter Registry. Auch ein
/// Delegate-Contract verleiht diesem externen Worker keine Apply-Berechtigung.
pub fn execute(
    prepared: PreparedJob,
    llm: Arc<dyn Llm>,
    cancel: &Cancel,
    on_event: impl FnMut(AgentEvent),
) -> Result<FactoryResult, String> {
    let FactoryJob {
        job_id,
        session_id,
        binding,
        contract,
        mut request,
        ..
    } = prepared.job;
    request
        .prompt
        .push_str("\n\nGebundener Änderungsvertrag (Kontext, keine zusätzlichen Werkzeuge):\n");
    request
        .prompt
        .push_str(&serde_json::to_string(&contract).map_err(|e| e.to_string())?);
    if request.prompt.len() > 32_000 {
        return Err("Eingabe und Änderungsvertrag überschreiten zusammen 32 KB.".into());
    }
    let result = run(request, llm, cancel, on_event)?;
    let status = if result.scope_suggestion.is_some() {
        "decision_required"
    } else if result.patch.is_some() {
        "proposal"
    } else {
        "no_change"
    };
    Ok(FactoryResult {
        schema: "tandem.factory-result/v1",
        worker: "tandem-agentkit",
        job_id,
        session_id,
        job_sha256: prepared.job_sha256,
        binding,
        workspace_check: prepared.workspace_check,
        execution_authorization: "none",
        verification: "not_run",
        status,
        result,
    })
}
