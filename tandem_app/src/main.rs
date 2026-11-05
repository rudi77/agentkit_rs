//! Zeilenbasiertes JSON-RPC. Der Lesethread bleibt während eines Modellaufrufs
//! frei für Cancel; nur der Agent-Lauf braucht einen eigenen Worker.

use agentkit::{agent::new_cancel, Cancel, Llm};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tandem_app::{demo_llm, RunRequest};

type Output = Arc<Mutex<io::Stdout>>;

fn send(out: &Output, value: Value) {
    let mut out = out.lock().unwrap();
    let _ = writeln!(out, "{value}").and_then(|_| out.flush());
}

fn model(provider: &str, request: &RunRequest) -> Result<Arc<dyn Llm>, String> {
    if provider == "demo" {
        return Ok(demo_llm(request));
    }
    configured_model(provider)
}

fn configured_model(provider: &str) -> Result<Arc<dyn Llm>, String> {
    #[cfg(not(feature = "openai"))]
    let _ = provider;
    #[cfg(feature = "openai")]
    {
        let provider = if provider == "auto" {
            match std::env::var("AGENTKIT_PROVIDER").unwrap_or_else(|_| "auto".into()).as_str() {
                "azure" => "azure",
                "openai" => "openai",
                "anthropic" => "anthropic",
                _ if std::env::var("AZURE_OPENAI_API_KEY").is_ok() => "azure",
                _ if std::env::var("OPENAI_API_KEY").is_ok() || std::env::var("OPENAI_BASE_URL").is_ok() => "openai",
                _ if std::env::var("ANTHROPIC_API_KEY").is_ok() => "anthropic",
                _ => return Err("Kein Modell konfiguriert. AgentKit-Konfiguration ergänzen oder explizit Offline-Demo wählen.".into()),
            }
        } else {
            provider
        };
        match provider {
            "azure" => agentkit::azure_from_env().map(|m| Arc::new(m) as Arc<dyn Llm>),
            "openai" => agentkit::openai_from_env().map(|m| Arc::new(m) as Arc<dyn Llm>),
            "anthropic" => agentkit::anthropic_from_env().map(|m| Arc::new(m) as Arc<dyn Llm>),
            _ => Err("Unbekannter Modellanbieter.".into()),
        }
    }
    #[cfg(not(feature = "openai"))]
    Err("Für echte Modelle mit --features openai bauen; Offline-Demo ist verfügbar.".into())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|s| s == "--help") {
        println!("tandem [--provider demo|auto|azure|openai|anthropic] [--workspace ABSOLUTER_PFAD]\ntandem --consume-handoff JOB.json [--provider ...] [--workspace ...]\nJSON-RPC: ping, run, explore, factory, check_path, workspace_snapshot, validate_exploration, cancel. Keine Produkt-Schreib- oder Shell-Werkzeuge.");
        return;
    }
    agentkit::load_user_config();
    let provider = args
        .windows(2)
        .find(|w| w[0] == "--provider")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| "auto".into());
    let workspace = args
        .windows(2)
        .find(|w| w[0] == "--workspace")
        .map(|w| std::path::PathBuf::from(&w[1]));
    if let Some(index) = args.iter().position(|arg| arg == "--consume-handoff") {
        let result = args
            .get(index + 1)
            .ok_or_else(|| "Pfad zur Übergabedatei fehlt.".to_string())
            .and_then(|file| tandem_app::factory::read_job(std::path::Path::new(file)))
            .and_then(|bytes| tandem_app::factory::prepare(&bytes, workspace.as_deref()))
            .and_then(|prepared| {
                model(&provider, &prepared.job.request).and_then(|llm| {
                    tandem_app::factory::execute(prepared, llm, &new_cancel(), |_| {})
                })
            })
            .and_then(|result| serde_json::to_string(&result).map_err(|e| e.to_string()));
        match result {
            Ok(json) => println!("{json}"),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        return;
    }
    let out: Output = Arc::new(Mutex::new(io::stdout()));
    let mut running: Option<(Arc<AtomicBool>, Cancel, std::thread::JoinHandle<()>)> = None;
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let msg: Value = match serde_json::from_str(&line) {
            Ok(msg) => msg,
            Err(_) => {
                send(
                    &out,
                    json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Ungültiges JSON."}}),
                );
                continue;
            }
        };
        if running
            .as_ref()
            .is_some_and(|(done, _, handle)| done.load(Ordering::SeqCst) || handle.is_finished())
        {
            let (_, _, handle) = running.take().unwrap();
            let _ = handle.join();
        }
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        match msg["method"].as_str() {
            Some("ping") => send(
                &out,
                json!({"jsonrpc":"2.0","id":id,"result":{"version":env!("CARGO_PKG_VERSION"),"provider":provider}}),
            ),
            Some("cancel") => {
                if let Some((_, cancel, _)) = &running {
                    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                send(&out, json!({"jsonrpc":"2.0","id":id,"result":true}));
            }
            Some("check_path" | "check_paths" | "validate_exploration" | "workspace_snapshot") => {
                let result = workspace
                    .as_ref()
                    .ok_or_else(|| "Kein Repository an den Adapter gebunden.".to_string())
                    .and_then(|root| {
                        if msg["method"] == "validate_exploration" {
                            tandem_app::exploration::validate_request(root, msg["params"].clone())
                        } else {
                            let repo = tandem_app::repository::Repository::open(root, vec![])?;
                            if msg["method"] == "workspace_snapshot" {
                                serde_json::to_value(repo.workspace_snapshot()?)
                                    .map_err(|e| e.to_string())
                            } else if msg["method"] == "check_paths" {
                                let paths: Vec<String> =
                                    serde_json::from_value(msg["params"]["paths"].clone())
                                        .map_err(|e| e.to_string())?;
                                if paths.len() > 100 {
                                    return Err(
                                        "Höchstens 100 offene Dokumente sind als Kontext erlaubt."
                                            .into(),
                                    );
                                }
                                let allowed: Vec<_> = paths
                                    .into_iter()
                                    .filter(|p| repo.check_path(p).is_ok())
                                    .collect();
                                Ok(json!({"allowed":allowed}))
                            } else {
                                repo.check_path(msg["params"]["path"].as_str().unwrap_or(""))
                                    .map(|_| json!({"allowed":true}))
                            }
                        }
                    });
                match result {
                    Ok(value) => send(&out, json!({"jsonrpc":"2.0","id":id,"result":value})),
                    Err(message) => send(
                        &out,
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":message}}),
                    ),
                }
            }
            Some(method @ ("run" | "explore" | "factory")) if running.is_none() => {
                enum Job {
                    Run(RunRequest),
                    Explore(tandem_app::exploration::ExploreRequest, std::path::PathBuf),
                    Factory(tandem_app::factory::PreparedJob),
                }
                let job = if method == "run" {
                    serde_json::from_value(msg["params"].clone())
                        .map(Job::Run)
                        .map_err(|e| e.to_string())
                } else if method == "factory" {
                    msg["params"]["job_json"]
                        .as_str()
                        .ok_or_else(|| "Der Worker-Job fehlt als UTF-8-JSON.".to_string())
                        .and_then(|raw| {
                            tandem_app::factory::prepare(raw.as_bytes(), workspace.as_deref())
                        })
                        .map(Job::Factory)
                } else {
                    workspace
                        .clone()
                        .ok_or_else(|| "Zuerst ein Repository öffnen.".to_string())
                        .and_then(|root| {
                            serde_json::from_value(msg["params"].clone())
                                .map(|r| Job::Explore(r, root))
                                .map_err(|e| e.to_string())
                        })
                };
                let job = match job {
                    Ok(request) => request,
                    Err(e) => {
                        send(
                            &out,
                            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":e.to_string()}}),
                        );
                        continue;
                    }
                };
                let cancel = new_cancel();
                let worker_cancel = cancel.clone();
                let done = Arc::new(AtomicBool::new(false));
                let worker_done = done.clone();
                let worker_out = out.clone();
                let worker_id = id.clone();
                let provider = provider.clone();
                let handle = std::thread::spawn(move || {
                    let on_event = |event| {
                        send(
                            &worker_out,
                            json!({"jsonrpc":"2.0","method":"event","params":{"run_id":worker_id,"event":event}}),
                        );
                    };
                    let result = match job {
                        Job::Factory(prepared) => model(&provider, &prepared.job.request)
                            .and_then(|llm| {
                                tandem_app::factory::execute(
                                    prepared,
                                    llm,
                                    &worker_cancel,
                                    on_event,
                                )
                            })
                            .and_then(|r| serde_json::to_value(r).map_err(|e| e.to_string())),
                        Job::Run(request) => model(&provider, &request)
                            .and_then(|llm| tandem_app::run(request, llm, &worker_cancel, on_event))
                            .and_then(|r| serde_json::to_value(r).map_err(|e| e.to_string())),
                        Job::Explore(request, root) => {
                            let llm = if provider == "demo" {
                                Ok(tandem_app::exploration::demo_exploration())
                            } else {
                                configured_model(&provider)
                            };
                            llm.and_then(|llm| {
                                tandem_app::exploration::explore(
                                    &root,
                                    request,
                                    llm,
                                    &worker_cancel,
                                    on_event,
                                )
                            })
                            .and_then(|r| serde_json::to_value(r).map_err(|e| e.to_string()))
                        }
                    };
                    // Der Client kann direkt nach der Antwort den nächsten Lauf
                    // senden, noch bevor dieser Thread endgültig zurückkehrt.
                    worker_done.store(true, Ordering::SeqCst);
                    match result {
                        Ok(result) => send(
                            &worker_out,
                            json!({"jsonrpc":"2.0","id":worker_id,"result":result}),
                        ),
                        Err(message) => send(
                            &worker_out,
                            json!({"jsonrpc":"2.0","id":worker_id,"error":{"code":-32000,"message":message}}),
                        ),
                    }
                });
                running = Some((done, cancel, handle));
            }
            Some("run" | "explore" | "factory") => send(
                &out,
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32001,"message":"Ein Lauf ist noch aktiv."}}),
            ),
            _ => send(
                &out,
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Unbekannte Methode."}}),
            ),
        }
    }
    if let Some((_, cancel, _)) = running {
        cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}
