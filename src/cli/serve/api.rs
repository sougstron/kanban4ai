//! Versioned remote operations. Disk work runs on blocking threads; rules stay in Operations.
use super::*;
use crate::core::operations::{Operations, QuestionRef, TaskPatch};
use crate::core::storage::{NewTask, Storage};
use crate::core::sync::{Journal, NodeRole};
use crate::core::thread::ThreadManager;
use axum::extract::Path as Params;
use serde_json::Value;
use std::collections::BTreeMap;

pub(super) fn routes(root: PathBuf) -> Router {
    Router::new()
        .route("/api/v1/projects", get(projects))
        .route("/api/v1/projects/{project}/board", get(board))
        .route(
            "/api/v1/projects/{project}/tasks",
            axum::routing::post(create),
        )
        .route(
            "/api/v1/projects/{project}/tasks/{task}",
            get(task).patch(edit),
        )
        .route(
            "/api/v1/projects/{project}/tasks/{task}/thread",
            get(thread),
        )
        .route(
            "/api/v1/projects/{project}/tasks/{task}/{action}",
            axum::routing::post(action),
        )
        .route("/events", get(events))
        .with_state(root)
}
fn net_error_code(message: &str) -> Option<&'static str> {
    [
        "NET_TAILSCALE_MISSING",
        "NET_TAILSCALE_LOGIN_REQUIRED",
        "NET_TAILSCALE_SIDECAR_DOWN",
    ]
    .into_iter()
    .find(|code| message.starts_with(code))
}
fn failure(e: KanbanError) -> Response {
    let (status, code) = match &e {
        KanbanError::Invalid(message) if net_error_code(message).is_some() => {
            (StatusCode::BAD_GATEWAY, net_error_code(message).unwrap())
        }
        KanbanError::Permission(_) => (StatusCode::FORBIDDEN, "OPERATION_FORBIDDEN"),
        KanbanError::Invalid(message) if message.contains("already running") => {
            (StatusCode::CONFLICT, "RUN_ALREADY_RUNNING")
        }
        KanbanError::Invalid(message) if message == "Not found" => {
            (StatusCode::NOT_FOUND, "NOT_FOUND")
        }
        KanbanError::Invalid(_) => (StatusCode::BAD_REQUEST, "INVALID_REQUEST"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR"),
    };
    error(status, code, &e.to_string())
}
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| invalid(e.to_string()))?
}
fn found<T>(value: Option<T>) -> Result<T> {
    value.ok_or_else(|| invalid("Not found"))
}
fn project(root: &Path, id: &str) -> Result<crate::core::project::Project> {
    found(ProjectStore::at(root).get(id)?)
}
fn valid_task(id: &str) -> Result<()> {
    if id.strip_prefix("TASK-").is_some_and(|n| {
        !n.is_empty()
            && n.bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'@' || b == b'-')
    }) {
        Ok(())
    } else {
        Err(invalid("Invalid task ID"))
    }
}
fn encode<T: Serialize>(value: T) -> Result<Value> {
    serde_json::to_value(value).map_err(|e| invalid(e.to_string()))
}
fn task_wire(task: crate::core::models::Task) -> Result<Value> {
    let mut value = encode(&task)?;
    value["description"] = Value::String(task.description);
    let map = value.as_object_mut().expect("Task serializes as an object");
    for field in [
        "session",
        "run_phase",
        "crash_restarts",
        "restart_at",
        "auto_resumes",
        "worktree",
        "branch",
        "base_commit",
        "integration",
        "cleanup_retry_at",
        "context_file",
    ] {
        map.remove(field);
    }
    Ok(value)
}
fn respond(result: Result<Value>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(e) => failure(e),
    }
}
async fn projects(State(root): State<PathBuf>) -> Response {
    respond(blocking(move || Ok(json!({"projects": ProjectStore::at(root).list()?.into_iter().map(|p| json!({"id": p.id, "name": p.name})).collect::<Vec<_>>()}))).await)
}
async fn board(State(root): State<PathBuf>, Params(id): Params<String>) -> Response {
    respond(
        blocking(move || {
            let p = project(&root, &id)?;
            let config = crate::core::config::Config::new(&p.data_root).load()?;
            let tasks = Operations::for_project(&p)
                .list_tasks(None, None, "id", "asc")?
                .into_iter()
                .map(task_wire)
                .collect::<Result<Vec<_>>>()?;
            Ok(json!({"columns": config.columns, "tasks": tasks}))
        })
        .await,
    )
}
async fn task(
    State(root): State<PathBuf>,
    Params((id, task)): Params<(String, String)>,
) -> Response {
    respond(
        blocking(move || {
            valid_task(&task)?;
            let p = project(&root, &id)?;
            let value = found(Operations::for_project(&p).get_task(&task)?)?;
            let journal = Journal::open(&root)?;
            let runs = journal.runs(&id, &task)?;
            let state = journal.state()?;
            let uid = state.project_uids.get(&id);
            let conflicts = match fs::read_to_string(root.join("sync/meta/conflicts.jsonl")) {
                Ok(text) => text
                    .lines()
                    .map(|line| {
                        decode::<crate::core::sync::Conflict>(
                            serde_json::from_str(line).map_err(|e| invalid(e.to_string()))?,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(e) => return Err(e.into()),
            };
            let conflicts: Vec<_> = conflicts
                .into_iter()
                .filter(|c| Some(&c.project) == uid && c.id == task)
                .collect();
            Ok(json!({"task": task_wire(value)?, "runs": runs, "conflicts": conflicts}))
        })
        .await,
    )
}
async fn thread(
    State(root): State<PathBuf>,
    Params((id, task)): Params<(String, String)>,
) -> Response {
    respond(
        blocking(move || {
            valid_task(&task)?;
            let p = project(&root, &id)?;
            found(Operations::for_project(&p).get_task(&task)?)?;
            encode(ThreadManager::new(p.data_root)?.load(&task)?)
        })
        .await,
    )
}
fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|e| invalid(e.to_string()))
}
fn fields(value: Value) -> Result<serde_json::Map<String, Value>> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| invalid("Expected a JSON object"))
}
fn patch(value: Value) -> Result<TaskPatch> {
    let mut patch = TaskPatch::default();
    for (key, value) in fields(value)? {
        match key.as_str() {
            "title" => patch.title = Some(decode(value)?),
            "description" => patch.description = Some(decode(value)?),
            "ai_model" => patch.ai_model = Some(decode(value)?),
            "ai_effort" => patch.ai_effort = Some(decode(value)?),
            "agent_backend" => patch.agent_backend = Some(decode(value)?),
            "agent_name" => patch.agent_name = Some(decode(value)?),
            "interactive" => patch.interactive = Some(decode(value)?),
            "readonly" => patch.readonly = Some(decode(value)?),
            "silence" => patch.silence = Some(decode(value)?),
            "use_designer" => patch.use_designer = Some(decode(value)?),
            "use_reviewer" => patch.use_reviewer = Some(decode(value)?),
            "use_orchestrator" => patch.use_orchestrator = Some(decode(value)?),
            "chained_to" => patch.chained_to = Some(decode(value)?),
            "needs" => patch.needs = Some(decode(value)?),
            "role_profile" => patch.role_profile = Some(decode(value)?),
            "runner" => patch.runner = Some(decode(value)?),
            // Column changes have side effects and role checks; never use update_task.status.
            _ => return Err(invalid(format!("Unsupported field: {key}"))),
        }
    }
    Ok(patch)
}
async fn create(
    State(root): State<PathBuf>,
    Params(id): Params<String>,
    body: std::result::Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(value) = match body {
        Ok(body) => body,
        Err(e) => return error(StatusCode::BAD_REQUEST, "INVALID_REQUEST", &e.body_text()),
    };
    respond(
        blocking(move || {
            crate::core::sync::with_actor("human", || {
                let p = project(&root, &id)?;
                let mut map = fields(value)?;
                let title: String = decode(
                    map.remove("title")
                        .ok_or_else(|| invalid("title is required"))?,
                )?;
                if title.trim().is_empty() {
                    return Err(invalid("title must not be empty"));
                }
                let patch = patch(Value::Object(map))?;
                let new = NewTask {
                    title,
                    description: patch.description.unwrap_or_default(),
                    ai_model: patch.ai_model.flatten(),
                    ai_effort: patch.ai_effort.flatten(),
                    agent_backend: patch.agent_backend.flatten(),
                    agent_name: patch.agent_name.flatten(),
                    interactive: patch.interactive.unwrap_or(false),
                    readonly: patch.readonly.unwrap_or(false),
                    silence: patch.silence.unwrap_or(false),
                    use_designer: patch.use_designer.unwrap_or(false),
                    use_reviewer: patch.use_reviewer.unwrap_or(false),
                    use_orchestrator: patch.use_orchestrator.unwrap_or(false),
                    chained_to: patch.chained_to.flatten(),
                    needs: patch.needs.flatten(),
                    role_profile: patch.role_profile.flatten(),
                    runner: patch.runner.flatten(),
                    ..Default::default()
                };
                task_wire(Operations::for_project(&p).create_task(new)?)
            })
        })
        .await,
    )
}
async fn edit(
    State(root): State<PathBuf>,
    Params((id, task)): Params<(String, String)>,
    body: std::result::Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(value) = match body {
        Ok(body) => body,
        Err(e) => return error(StatusCode::BAD_REQUEST, "INVALID_REQUEST", &e.body_text()),
    };
    respond(
        blocking(move || {
            crate::core::sync::with_actor("human", || {
                valid_task(&task)?;
                let p = project(&root, &id)?;
                task_wire(found(
                    Operations::for_project(&p).update_task(&task, patch(value)?)?,
                )?)
            })
        })
        .await,
    )
}
#[derive(Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct Action {
    status: Option<String>,
    message: Option<String>,
    text: Option<String>,
    variants: Vec<String>,
    runner: Option<NodeRole>,
    agent: bool,
}
async fn action(
    State(root): State<PathBuf>,
    Params((id, task, action)): Params<(String, String, String)>,
    body: std::result::Result<Json<Action>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(body) => body,
        Err(e) => return error(StatusCode::BAD_REQUEST, "INVALID_REQUEST", &e.body_text()),
    };
    if let Err(e) = valid_task(&task) {
        return failure(e);
    }
    if matches!(action.as_str(), "launch" | "stop") {
        match target(&root, &id, &task, request.runner, &action).await {
            Ok(Some(role)) => return proxy(&root, role, &id, &task, &action, &request).await,
            Ok(None) => {}
            Err(e) => return failure(e),
        }
    }
    respond(blocking(move || crate::core::sync::with_actor(if request.agent { "agent" } else { "human" }, || {
        let p = project(&root, &id)?;
        let ops = Operations::for_project(&p);
        match action.as_str() {
            "move" => task_wire(found(ops.move_task(&task, request.status.as_deref().ok_or_else(|| invalid("status is required"))?, request.agent)?)?),
            "ask" => task_wire(found(ops.ask_question(&task, request.text.as_deref().ok_or_else(|| invalid("text is required"))?, if request.agent { "agent" } else { "human" }, request.variants)?)?),
            "answer" => {
                let outcome = found(ops.answer_question(&task, QuestionRef::MsgId(request.message.ok_or_else(|| invalid("message is required"))?), request.text.as_deref().ok_or_else(|| invalid("text is required"))?)?)?;
                Ok(json!({"task": task_wire(outcome.task)?, "remaining": outcome.remaining, "resumed_session": outcome.resumed_session, "queued": outcome.queued}))
            },
            "launch" => Ok(json!({"session": found(ops.start_task(&task)?)?})),
            "stop" => task_wire(found(ops.stop_task(&task)?)?),
            _ => Err(invalid("Unknown action")),
        }
    })).await)
}
async fn target(
    root: &Path,
    id: &str,
    task: &str,
    requested: Option<NodeRole>,
    action: &str,
) -> Result<Option<NodeRole>> {
    let (root, id, task, action) = (
        root.to_path_buf(),
        id.to_owned(),
        task.to_owned(),
        action.to_owned(),
    );
    blocking(move || {
        let p = project(&root, &id)?;
        let task = found(Operations::for_project(&p).get_task(&task)?)?;
        let journal = Journal::open(&root)?;
        let active = journal
            .runs(&id, &task.id)?
            .into_values()
            .find(|run| run.active());
        let role = if action == "stop" {
            active.map(|run| run.role).or(task.runner)
        } else {
            requested.or(task.runner)
        };
        Ok(role.filter(|role| *role != journal.node.role))
    })
    .await
}
async fn proxy(
    root: &Path,
    role: NodeRole,
    id: &str,
    task: &str,
    action: &str,
    request: &Action,
) -> Response {
    let config = match discovery::config(root) {
        Ok(c) => c,
        Err(e) => return failure(e),
    };
    let available = match discovery::network_available(root, &config) {
        Ok(available) => available,
        Err(e) => return failure(e),
    };
    if !available {
        return error(
            StatusCode::CONFLICT,
            "NODE_OUT_OF_LAN",
            "This node has neither home LAN nor tailnet enabled",
        );
    }
    let Some(peer) = config.peers.iter().find(|p| p.node.role == role) else {
        return error(
            StatusCode::CONFLICT,
            "NODE_UNREACHABLE",
            "Runner is not paired",
        );
    };
    let credentials: Vec<discovery::Credential> =
        match discovery::load(&root.join("sync/peers.yaml")) {
            Ok(c) => c,
            Err(e) => return failure(e),
        };
    let Some(secret) = credentials.iter().find(|c| c.node_id == peer.node.node_id) else {
        return error(
            StatusCode::BAD_GATEWAY,
            "NODE_AUTH_FAILED",
            "Runner credentials are missing",
        );
    };
    let presence: BTreeMap<String, discovery::Presence> =
        match discovery::load(&root.join("sync/presence.yaml")) {
            Ok(p) => p,
            Err(e) => return failure(e),
        };
    let presence = presence
        .get(&peer.node.node_id)
        .cloned()
        .unwrap_or_default();
    if presence.bye.is_some() {
        return error(
            StatusCode::CONFLICT,
            "NODE_POWERED_OFF",
            "Runner has announced shutdown or sleep",
        );
    }
    let addresses = match discovery::peer_addresses(root, peer, Some(secret), &presence, &config) {
        Ok(addresses) => addresses,
        Err(e) => return failure(e),
    };
    let mut last: Option<String> = None;
    for address in addresses {
        let timeout = Duration::from_secs(config.probe_timeout);
        let http = match discovery::peer_client(root, &address, timeout, Some(timeout)) {
            Ok(http) => http,
            Err(e) => {
                last = Some(e.to_string());
                continue;
            }
        };
        let probe = match http
            .get(format!("http://{address}/node"))
            .bearer_auth(&secret.token)
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => {
                last = Some(e.to_string());
                continue;
            }
        };
        if probe.status() == StatusCode::UNAUTHORIZED || probe.status() == StatusCode::FORBIDDEN {
            return error(
                StatusCode::BAD_GATEWAY,
                "NODE_AUTH_FAILED",
                "Runner rejected its node token",
            );
        }
        let card = match probe.json::<Value>().await {
            Ok(card) => card,
            Err(e) => return error(StatusCode::BAD_GATEWAY, "NODE_SERVICE_DOWN", &e.to_string()),
        };
        if card["node_id"] != peer.node.node_id || card["store_id"] != peer.node.store_id {
            return error(
                StatusCode::CONFLICT,
                "SYNC_STORE_MISMATCH",
                "Runner identity or store does not match the paired node",
            );
        }
        if card["proto"]
            .as_u64()
            .is_none_or(|p| p < u64::from(MIN_PROTO))
            || card["min_proto"]
                .as_u64()
                .is_none_or(|p| p > u64::from(PROTO))
        {
            return error(
                StatusCode::UPGRADE_REQUIRED,
                "NODE_VERSION_MISMATCH",
                "Runner protocol is incompatible",
            );
        }
        let url = format!("http://{address}/api/v1/projects/{id}/tasks/{task}/{action}");
        match http
            .post(url)
            .bearer_auth(&secret.token)
            .json(request)
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();
                return match response.json::<Value>().await {
                    Ok(value) => (status, Json(value)).into_response(),
                    Err(e) => error(StatusCode::BAD_GATEWAY, "NODE_SERVICE_DOWN", &e.to_string()),
                };
            }
            // A response timeout may follow a committed launch: never retry a mutation.
            Err(e) if e.is_connect() => last = Some(e.to_string()),
            Err(e) => {
                return error(
                    StatusCode::GATEWAY_TIMEOUT,
                    "NODE_UNREACHABLE",
                    &e.to_string(),
                );
            }
        }
    }
    let message = last.unwrap_or_else(|| "No LAN or tailnet runner address".into());
    error(
        StatusCode::BAD_GATEWAY,
        net_error_code(&message).unwrap_or("NODE_UNREACHABLE"),
        &message,
    )
}
fn event_state(root: &Path, selected: Option<&str>) -> Result<Value> {
    let projects = ProjectStore::at(root).list()?;
    let mut boards = BTreeMap::new();
    for p in projects
        .into_iter()
        .filter(|p| selected.is_none_or(|id| id == p.id))
    {
        boards.insert(p.id, (p.name, Storage::new(p.data_root).tui_fingerprint()));
    }
    let sync = fs::metadata(root.join("sync/meta/state.json"))
        .ok()
        .and_then(|m| m.modified().ok());
    let presence = fs::read_to_string(root.join("sync/presence.yaml")).unwrap_or_default();
    Ok(json!({"boards": boards, "sync": format!("{sync:?}"), "presence": presence}))
}
async fn events(State(root): State<PathBuf>, uri: axum::http::Uri) -> Response {
    let project = uri
        .query()
        .and_then(|q| q.split('&').find_map(|p| p.strip_prefix("project=")))
        .map(str::to_owned);
    let initial_root = root.clone();
    let selected = project.clone();
    let initial = match blocking(move || event_state(&initial_root, selected.as_deref())).await {
        Ok(v) => v,
        Err(e) => return failure(e),
    };
    let stream = futures_util::stream::unfold(
        (root, project, initial, true, 0u32),
        |(root, project, mut previous, first, mut idle)| async move {
            if first {
                return Some((
                    Ok::<_, std::convert::Infallible>("event: ready\ndata: {}\n\n".to_owned()),
                    (root, project, previous, false, 0),
                ));
            }
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let read = root.clone();
                let selected = project.clone();
                let current = blocking(move || event_state(&read, selected.as_deref()))
                    .await
                    .ok()?;
                idle += 1;
                if current != previous {
                    previous = current;
                    return Some((
                        Ok("event: board\ndata: {\"refresh\":true}\n\n".to_owned()),
                        (root, project, previous, false, 0),
                    ));
                }
                if idle == 30 {
                    return Some((
                        Ok(": keepalive\n\n".to_owned()),
                        (root, project, previous, false, 0),
                    ));
                }
            }
        },
    );
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn http_operations_rules_and_external_sse() {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("store");
        let work = temp.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let p = ProjectStore::at(&store).add(&work, None).unwrap().project;
        Storage::new(&p.data_root).init_board().unwrap();
        let client_token = issue_token(&store, TokenKind::Node).unwrap();
        let node_token = issue_token(&store, TokenKind::Node).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = super::super::router(store.join("sync/tokens.yaml"));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let http = reqwest::Client::new();
        let api = format!("{base}/api/v1/projects/{}/tasks", p.id);
        assert_eq!(
            http.get(format!("{base}/api/v1/projects"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let response = http
            .post(&api)
            .bearer_auth(&client_token)
            .json(&json!({"title":"Remote", "description":"Markdown body", "runner":"laptop"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let created: Value = response.json().await.unwrap();
        assert_eq!(created["description"], "Markdown body");
        assert_eq!(created["runner"], "laptop");
        assert!(created.get("session").is_none());
        let task_id = created["id"].as_str().unwrap();
        let url = format!("{api}/{task_id}");
        let result = http
            .patch(&url)
            .bearer_auth(&client_token)
            .json(&json!({"status":"done"}))
            .send()
            .await
            .unwrap();
        assert_eq!(result.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            Operations::for_project(&p)
                .get_task(task_id)
                .unwrap()
                .unwrap()
                .status
                .as_str(),
            "todo"
        );
        let result = http
            .patch(&url)
            .bearer_auth(&client_token)
            .json(&json!({"description":"Edited"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            result.json::<Value>().await.unwrap()["description"],
            "Edited"
        );
        let result = http
            .post(format!("{url}/move"))
            .bearer_auth(&node_token)
            .json(&json!({"status":"done", "agent":true}))
            .send()
            .await
            .unwrap();
        assert_eq!(result.status(), StatusCode::FORBIDDEN);
        let result = http
            .post(format!("{url}/move"))
            .bearer_auth(&client_token)
            .json(&json!({"status":"done", "agent":true}))
            .send()
            .await
            .unwrap();
        assert_eq!(result.status(), StatusCode::FORBIDDEN);
        let result = http
            .post(format!("{url}/ask"))
            .bearer_auth(&client_token)
            .json(&json!({"text":"Choose", "variants":["yes","no"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(result.status(), StatusCode::OK);
        let thread: Value = http
            .get(format!("{url}/thread"))
            .bearer_auth(&client_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let message = thread["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["kind"] == "question")
            .unwrap()["id"]
            .as_str()
            .unwrap();
        let result: Value = http
            .post(format!("{url}/answer"))
            .bearer_auth(&client_token)
            .json(&json!({"message":message, "text":"yes"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(result["remaining"], 0);
        let thread: Value = http
            .get(format!("{url}/thread"))
            .bearer_auth(&client_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            thread["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["id"] == message)
                .unwrap()["answer"],
            "yes"
        );
        let mut stream = http
            .get(format!("{base}/events?project={}", p.id))
            .bearer_auth(&client_token)
            .send()
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&stream.chunk().await.unwrap().unwrap())
                .contains("event: ready")
        );
        Operations::for_project(&p)
            .update_task(
                task_id,
                TaskPatch {
                    title: Some("External write".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(3), stream.chunk())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&event).contains("event: board"));
        let result: Value = http
            .get(&url)
            .bearer_auth(&client_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(result["task"]["title"], "External write");
        assert_eq!(result["task"]["description"], "Edited");
        let journal = Journal::open(&store).unwrap();
        assert!(
            journal
                .state()
                .unwrap()
                .operations
                .values()
                .any(|op| op.id == task_id
                    && op.actor == "human"
                    && op.set.get("title") == Some(&json!("Remote")))
        );
        let missing = format!("{api}/TASK-999999");
        for action in ["launch", "stop"] {
            assert_eq!(
                http.post(format!("{missing}/{action}"))
                    .bearer_auth(&client_token)
                    .json(&json!({}))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::NOT_FOUND
            );
        }
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn remote_stop_closes_only_runner_session() {
        let temp = tempfile::tempdir().unwrap();
        let laptop = temp.path().join("laptop");
        let runner = temp.path().join("runner");
        let work = temp.path().join("work");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&runner).unwrap();
        atomic_write_text_raw(
            &runner.join("config.yaml"),
            "node:\n  name: server\n  role: server\n",
        )
        .unwrap();
        let a = ProjectStore::at(&laptop).add(&work, None).unwrap().project;
        let b = ProjectStore::at(&runner).add(&work, None).unwrap().project;
        Storage::new(&a.data_root).init_board().unwrap();
        Storage::new(&b.data_root).init_board().unwrap();
        let local = Operations::for_project(&a)
            .create_task(NewTask {
                runner: Some(NodeRole::Server),
                ..NewTask::titled("Remote run")
            })
            .unwrap();
        let remote = Operations::for_project(&b)
            .create_task(NewTask::titled("Remote run"))
            .unwrap();
        assert_eq!(local.id, remote.id);
        let session_id = "ses-task414-proxy";
        let sessions = crate::core::session::SessionManager::new(&b.data_root);
        sessions.link_session(&remote.id, session_id).unwrap();
        Operations::for_project(&b)
            .update_task(
                &remote.id,
                TaskPatch {
                    session: Some(Some(session_id.into())),
                    ..Default::default()
                },
            )
            .unwrap();
        let caller_node = Journal::open(&laptop).unwrap().node;
        let mut runner_node = Journal::open(&runner).unwrap().node;
        runner_node.store_id = caller_node.store_id.clone();
        atomic_write_text_raw(
            &runner.join("sync/node.yaml"),
            &serde_yaml_ng::to_string(&runner_node).unwrap(),
        )
        .unwrap();
        let secret = issue_token(&runner, TokenKind::Node).unwrap();
        let client = issue_token(&laptop, TokenKind::Node).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let runner_router = super::super::router(runner.join("sync/tokens.yaml"));
        let server = tokio::spawn(async move {
            axum::serve(listener, runner_router).await.unwrap();
        });
        atomic_write_text_raw(
            &laptop.join("config.yaml"),
            &serde_yaml_ng::to_string(&json!({
                "sync": { "home_networks": ["0.0.0.0/0"], "peers": [{
                    "node_id":runner_node.node_id, "name":runner_node.name, "role":"server",
                    "store_id":runner_node.store_id, "address":address.to_string()
                }]}
            }))
            .unwrap(),
        )
        .unwrap();
        atomic_write_text_raw(
            &laptop.join("sync/peers.yaml"),
            &serde_yaml_ng::to_string(&json!([{
                "node_id":runner_node.node_id, "token":secret, "inbound_sha256":"unused"
            }]))
            .unwrap(),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let laptop_router = super::super::router(laptop.join("sync/tokens.yaml"));
        let frontend = tokio::spawn(async move {
            axum::serve(listener, laptop_router).await.unwrap();
        });
        let http = reqwest::Client::new();
        let url = format!("{base}/api/v1/projects/{}/tasks/{}", a.id, local.id);
        let conflict = http
            .post(format!("{url}/launch"))
            .bearer_auth(&client)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        assert_eq!(
            conflict.json::<Value>().await.unwrap()["error"]["code"],
            "RUN_ALREADY_RUNNING"
        );
        let stopped = http
            .post(format!("{url}/stop"))
            .bearer_auth(&client)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            stopped.status(),
            StatusCode::OK,
            "{}",
            stopped.text().await.unwrap()
        );
        assert_eq!(
            sessions.load_session(session_id).unwrap().status,
            crate::core::models::SessionStatus::Closed
        );
        assert!(
            Operations::for_project(&a)
                .get_task(&local.id)
                .unwrap()
                .unwrap()
                .session
                .is_none()
        );
        frontend.abort();
        server.abort();
        let _ = frontend.await;
        let _ = server.await;
    }
}
