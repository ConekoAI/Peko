//! `POST /v1/files` and `GET /v1/files` (ADR-121 D5): files cross the
//! door as bytes, beside the packets. Both need an `owner` key, as
//! attaching needs the local operator (ADR-088 D1).

use std::path::PathBuf;

use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{HeaderValue, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};
use hyper::{Request, Response, StatusCode};

use super::{json_response, media_type, refuse, Door, DoorBody};
use crate::ipc::uploads::{Refused, PREFIX};
use crate::principal::attachments::{any_stored, is_digest, MAX_FILE_BYTES};
use peko_auth::caller::CallerContext;

fn query(req: &Request<Incoming>) -> Vec<(String, String)> {
    url::form_urlencoded::parse(req.uri().query().unwrap_or("").as_bytes())
        .into_owned()
        .collect()
}

fn param<'a>(query: &'a [(String, String)], key: &str) -> Option<&'a str> {
    query
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn owner_only(caller: &CallerContext) -> Option<Response<DoorBody>> {
    (!caller.is_owner_key()).then(|| refuse(StatusCode::FORBIDDEN, "files need an owner key"))
}

/// `POST /v1/files?name=<file name>`, the body being the file.
pub(super) async fn upload(
    door: &Door,
    req: Request<Incoming>,
    caller: CallerContext,
) -> Response<DoorBody> {
    if let Some(refusal) = owner_only(&caller) {
        return refusal;
    }
    if media_type(req.headers().get(CONTENT_TYPE)).as_deref() != Some("application/octet-stream") {
        return refuse(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "the body must be application/octet-stream",
        );
    }
    let query = query(&req);
    let Some(name) = param(&query, "name").map(str::to_string) else {
        return refuse(StatusCode::BAD_REQUEST, "name the file: ?name=<file name>");
    };
    let declared = req
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let chunks = Box::pin(req.into_body().into_data_stream());
    let result = door.uploads.accept(&name, declared, chunks).await;
    match result {
        Ok(done) => {
            door.host
                .record(
                    &caller,
                    "http.upload",
                    serde_json::json!({
                        "key_id": caller.api_key_id(),
                        "file": format!("{PREFIX}{}", done.id),
                        "name": done.name,
                        "size": done.size,
                    }),
                )
                .await;
            json_response(
                StatusCode::OK,
                serde_json::json!({
                    "file": format!("{PREFIX}{}", done.id),
                    "name": done.name,
                    "size": done.size,
                })
                .to_string(),
            )
        }
        Err(Refused::TooLarge) => refuse(
            StatusCode::PAYLOAD_TOO_LARGE,
            &format!("a file is at most {} MB", MAX_FILE_BYTES / (1024 * 1024)),
        ),
        Err(Refused::Full) => refuse(
            StatusCode::INSUFFICIENT_STORAGE,
            "too many uploads are waiting; send the ones you have, or wait an hour",
        ),
        Err(Refused::BadName) => refuse(StatusCode::BAD_REQUEST, "that is not a file name"),
        Err(Refused::Failed(reason)) => refuse(
            StatusCode::BAD_REQUEST,
            &format!("the upload failed: {reason}"),
        ),
    }
}

/// `GET /v1/files?path=<path a post gave>` or
/// `GET /v1/files?channel=<id>&sha256=<digest>`.
pub(super) async fn download(
    door: &Door,
    req: Request<Incoming>,
    caller: CallerContext,
) -> Response<DoorBody> {
    if let Some(refusal) = owner_only(&caller) {
        return refusal;
    }
    let query = query(&req);
    let not_found = || refuse(StatusCode::NOT_FOUND, "no such file");
    let found: Option<PathBuf> = match (
        param(&query, "path"),
        param(&query, "channel"),
        param(&query, "sha256"),
    ) {
        (Some(path), None, None) => {
            // Links followed, then it must lie in a peko's attachments.
            let Ok(path) = tokio::fs::canonicalize(path).await else {
                return not_found();
            };
            let mut workspaces = Vec::new();
            for ws in door.host.workspaces().await {
                if let Ok(ws) = tokio::fs::canonicalize(ws).await {
                    workspaces.push(ws);
                }
            }
            crate::ipc::uploads::in_attachments(&path, &workspaces).then_some(path)
        }
        (None, Some(channel), Some(digest)) if is_digest(digest) => door
            .host
            .channel_files_dir(channel)
            .await
            .and_then(|dir| any_stored(&dir, digest)),
        _ => {
            return refuse(
                StatusCode::BAD_REQUEST,
                "ask by ?path=<path a post gave>, or by ?channel=<id>&sha256=<digest>",
            )
        }
    };
    let Some(path) = found else {
        return not_found();
    };
    let Ok(file) = tokio::fs::File::open(&path).await else {
        return not_found();
    };
    let Ok(meta) = file.metadata().await else {
        return not_found();
    };
    if !meta.is_file() {
        return not_found();
    }
    door.host
        .record(
            &caller,
            "http.download",
            serde_json::json!({
                "key_id": caller.api_key_id(),
                "path": path.display().to_string(),
                "size": meta.len(),
            }),
        )
        .await;
    let mime = crate::common::media::mime_of(&path).await;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    // A read error ends the body early; the length tells the client.
    let chunks = futures::StreamExt::take_while(tokio_util::io::ReaderStream::new(file), |r| {
        std::future::ready(r.is_ok())
    });
    let frames = futures::StreamExt::map(chunks, |chunk| {
        Ok::<_, std::convert::Infallible>(Frame::data(chunk.unwrap_or_else(|_| Bytes::new())))
    });
    let mut response = Response::new(BodyExt::boxed(StreamBody::new(frames)));
    let headers = response.headers_mut();
    if let Ok(mime) = HeaderValue::from_str(&mime) {
        headers.insert(CONTENT_TYPE, mime);
    }
    headers.insert(CONTENT_LENGTH, HeaderValue::from(meta.len()));
    headers.insert(
        hyper::header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    let disposition = format!(
        "attachment; filename*=UTF-8''{}",
        urlencoding::encode(&name)
    );
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(CONTENT_DISPOSITION, value);
    }
    response
}
