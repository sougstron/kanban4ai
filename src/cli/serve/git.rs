//! Authenticated code transfer. Bundles are LAN-only; remote preparation also
//! works over tailnet. Git runs on blocking workers; HTTP streams via temporary
//! files so repository size does not determine the service's memory usage.
use super::*;
use crate::core::code;
use axum::{
    body::Body,
    extract::{ConnectInfo, Path as Params, Query},
};
use futures_util::{Stream, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default, Deserialize)]
struct Options {
    have: Option<String>,
    remote: Option<String>,
}
pub(super) fn routes(root: PathBuf) -> Router {
    Router::new()
        .route("/git/{project}/bundle", get(export).post(receive))
        .route("/git/{project}/status", get(status))
        .with_state(root)
}
fn lan(ConnectInfo(addr): ConnectInfo<SocketAddr>) -> bool {
    let ip = addr.ip();
    !["100.64.0.0/10", "fd7a:115c:a1e0::/48"]
        .iter()
        .any(|cidr| cidr.parse::<IpNet>().unwrap().contains(&ip))
        && allowed(ip, &[])
}
fn failure(e: KanbanError) -> Response {
    let message = e.to_string();
    let code = ["SYNC_NO_REMOTE", "RUNNER_REPO_MISSING"]
        .into_iter()
        .find(|c| message.contains(c))
        .unwrap_or("GIT_TRANSFER_FAILED");
    error(StatusCode::BAD_REQUEST, code, &message)
}
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| invalid(e.to_string()))?
}
/// The stream owns the temporary bundle until HTTP finishes or disconnects.
pub(super) async fn bundle_stream(
    bundle: tempfile::NamedTempFile,
) -> Result<impl Stream<Item = std::io::Result<Vec<u8>>> + Send> {
    let file = tokio::fs::File::open(bundle.path()).await?;
    Ok(futures_util::stream::try_unfold(
        (file, bundle),
        |(mut file, bundle)| async move {
            let mut chunk = vec![0; 64 * 1024];
            let count = file.read(&mut chunk).await?;
            if count == 0 {
                return Ok(None);
            }
            chunk.truncate(count);
            Ok(Some((chunk, (file, bundle))))
        },
    ))
}
async fn status(State(root): State<PathBuf>, Params(id): Params<String>) -> Response {
    match blocking(move || code::status(&root, &id)).await {
        Ok(status) => Json(status).into_response(),
        Err(e) => failure(e),
    }
}
async fn export(
    State(root): State<PathBuf>,
    Params(id): Params<String>,
    Query(options): Query<Options>,
    connection: ConnectInfo<SocketAddr>,
) -> Response {
    if !lan(connection) {
        return error(
            StatusCode::FORBIDDEN,
            "NODE_OUT_OF_LAN",
            "Git bundles require LAN",
        );
    }
    let result = blocking(move || {
        let have: Vec<String> = options
            .have
            .map(|s| s.split(',').map(str::to_owned).collect())
            .unwrap_or_default();
        code::export(&root, &id, &have).map(|(bundle, _)| bundle)
    })
    .await;
    match result {
        Ok(bundle) => match bundle_stream(bundle).await {
            Ok(stream) => (
                [(header::CONTENT_TYPE, "application/x-git-bundle")],
                Body::from_stream(stream),
            )
                .into_response(),
            Err(e) => failure(e),
        },
        Err(e) => failure(e),
    }
}
async fn receive(
    State(root): State<PathBuf>,
    Params(id): Params<String>,
    Query(options): Query<Options>,
    connection: ConnectInfo<SocketAddr>,
    body: Body,
) -> Response {
    let is_lan = lan(connection);
    let bundle = match tempfile::NamedTempFile::new() {
        Ok(file) => file,
        Err(e) => return failure(e.into()),
    };
    let mut file = match tokio::fs::File::create(bundle.path()).await {
        Ok(file) => file,
        Err(e) => return failure(e.into()),
    };
    let mut stream = body.into_data_stream();
    let mut empty = true;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(e) => return failure(invalid(e.to_string())),
        };
        if chunk.is_empty() {
            continue;
        }
        if !is_lan {
            return error(
                StatusCode::FORBIDDEN,
                "NODE_OUT_OF_LAN",
                "Git bundles require LAN",
            );
        }
        empty = false;
        if let Err(e) = file.write_all(&chunk).await {
            return failure(e.into());
        }
    }
    if let Err(e) = file.flush().await {
        return failure(e.into());
    }
    drop(file);
    match blocking(move || {
        if empty {
            code::prepare_from_remote(
                &root,
                &id,
                options
                    .remote
                    .as_deref()
                    .ok_or_else(|| invalid("remote is required"))?,
            )
        } else {
            code::receive(&root, &id, bundle.path(), options.remote.as_deref())
        }
    })
    .await
    {
        Ok(status) => Json(status).into_response(),
        Err(e) => failure(e),
    }
}
