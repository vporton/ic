use std::{fmt, io::Read, sync::Arc, time::Instant};

use anyhow::{anyhow, Context, Error};
use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use axum::{
    body::{Body, Bytes, StreamBody},
    extract::{FromRequestParts, Host, MatchedPath, OriginalUri, Path, RawBody, State},
    http::{uri::PathAndQuery, Request, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, IntoResponseParts, Redirect, Response, ResponseParts},
    Extension, RequestExt, RequestPartsExt,
};
use bytes::Buf;
use candid::Principal;
use futures_util::{StreamExt, TryFutureExt};
use http::{header, request::Parts, HeaderValue};
use ic_types::{
    messages::{
        Blob, HttpQueryContent, HttpRequestEnvelope, HttpStatusResponse, HttpUserQuery,
        ReplicaHealthStatus,
    },
    CanisterId,
};
use rand::seq::SliceRandom;
use reqwest::Response as ReqwestResponse;
use serde::{Deserialize, Serialize};
use serde_cbor::Value as CborValue;
use tokio::sync::RwLock;
use tower_http::request_id::{MakeRequestId, RequestId};
use tracing::{error, info};

use crate::{persist::Routes, snapshot::Node};

// Type of IC request
#[derive(Default, Clone, Copy)]
pub enum RequestType {
    #[default]
    Status,
    Query,
    Call,
    ReadState,
}

impl fmt::Display for RequestType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Status => write!(f, "status"),
            Self::Query => write!(f, "query"),
            Self::Call => write!(f, "call"),
            Self::ReadState => write!(f, "read_state"),
        }
    }
}

// Categorized possible causes for request processing failures
// Use String and not Error since it's not cloneable
#[derive(Default, Clone)]
pub enum ErrorCause {
    #[default]
    NoError,
    UnableToReadBody,
    UnableToParseCBOR(String), // TODO just use MalformedRequest?
    MalformedRequest(String),
    NoRoutingTable,
    SubnetNotFound,
    NoHealthyNodes,
    ReplicaUnreachable(String),
    Other(String),
}

impl ErrorCause {
    fn status_code(&self) -> StatusCode {
        match self {
            Self::NoError => StatusCode::OK,
            Self::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::UnableToReadBody => StatusCode::BAD_REQUEST,
            Self::UnableToParseCBOR(_) => StatusCode::BAD_REQUEST,
            Self::MalformedRequest(_) => StatusCode::BAD_REQUEST,
            Self::NoRoutingTable => StatusCode::INTERNAL_SERVER_ERROR,
            Self::SubnetNotFound => StatusCode::BAD_REQUEST, // TODO change to 404?
            Self::NoHealthyNodes => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ReplicaUnreachable(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn details(&self) -> Option<String> {
        match self {
            Self::Other(x) => Some(x.clone()),
            Self::UnableToParseCBOR(x) => Some(x.clone()),
            Self::MalformedRequest(x) => Some(x.clone()),
            Self::ReplicaUnreachable(x) => Some(x.clone()),
            _ => None,
        }
    }
}

impl fmt::Display for ErrorCause {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::NoError => write!(f, "no_error"),
            Self::Other(_) => write!(f, "general_error"),
            Self::UnableToReadBody => write!(f, "unable_to_read_body"),
            Self::UnableToParseCBOR(_) => write!(f, "unable_to_parse_cbor"),
            Self::MalformedRequest(_) => write!(f, "malformed_request"),
            Self::NoRoutingTable => write!(f, "no_routing_table"),
            Self::SubnetNotFound => write!(f, "subnet_not_found"),
            Self::NoHealthyNodes => write!(f, "no_healthy_nodes"),
            Self::ReplicaUnreachable(_) => write!(f, "replica_unreachable"),
        }
    }
}

// Object that holds per-request information
#[derive(Default, Clone)]
pub struct RequestContext {
    canister_id: Option<Principal>,
    canister_id_cbor: Option<Principal>,
    node: Option<Node>,
    sender: Option<Principal>,
    method_name: Option<String>,
    request_type: RequestType,
    error_cause: ErrorCause,
    request_size: u32,
}

impl RequestContext {
    fn respond(&mut self, cause: ErrorCause) -> Response {
        self.error_cause = cause.clone();
        (Extension(self.clone()), cause.status_code()).into_response()
    }
}

// Generic IC request - subset of fields
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ICRequestContent {
    canister_id: Option<Blob>,
    method_name: Option<String>,
    sender: Option<Blob>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ICRequestEnvelope {
    content: ICRequestContent,
}

// Trait that proxy router should implement
#[async_trait]
pub trait Proxier {
    async fn proxy(
        &self,
        request_type: RequestType,
        request: Request<Body>,
        node: Node,
        canister_id: Principal,
    ) -> Result<Response, ErrorCause>;

    fn lookup_node(&self, canister_id: Principal) -> Result<Node, ErrorCause>;

    fn health(&self) -> ReplicaHealthStatus;
}

// Router that helps handlers do their job by looking up in routing table
// and owning HTTP client for outgoing requests
pub struct ProxyRouter {
    http_client: Arc<reqwest::Client>,
    published_routes: Arc<ArcSwapOption<Routes>>,
}

impl ProxyRouter {
    pub fn new(
        http_client: Arc<reqwest::Client>,
        published_routes: Arc<ArcSwapOption<Routes>>,
    ) -> Self {
        Self {
            http_client,
            published_routes,
        }
    }
}

#[async_trait]
impl Proxier for ProxyRouter {
    async fn proxy(
        &self,
        request_type: RequestType,
        request: Request<Body>,
        node: Node,
        canister_id: Principal,
    ) -> Result<Response, ErrorCause> {
        // Prepare the request
        let url = format!(
            "https://{}:{}/api/v2/canister/{canister_id}/{request_type}",
            node.id, node.port,
        );

        let (parts, body) = request.into_parts();

        let request = self
            .http_client
            .post(url)
            .headers(parts.headers)
            .body(body)
            .build()
            .map_err(|e| ErrorCause::Other(format!("Unable to build request: {e}")))?; // TODO can this even fail?

        // Send the request
        let response = self
            .http_client
            .execute(request)
            .await
            .map_err(|e| ErrorCause::ReplicaUnreachable(format!("HTTP call failed: {e}")))?;

        // Convert Reqwest response into Axum one with body streaming
        let status = response.status();
        let headers = response.headers().clone();

        let mut response = StreamBody::new(response.bytes_stream()).into_response();
        *response.status_mut() = status;
        *response.headers_mut() = headers;

        Ok(response)
    }

    fn lookup_node(&self, canister_id: Principal) -> Result<Node, ErrorCause> {
        let subnet = self
            .published_routes
            .load_full()
            .ok_or(ErrorCause::NoRoutingTable)? // No routing table present
            .lookup(canister_id)
            .ok_or(ErrorCause::SubnetNotFound)?; // Requested canister route wasn't found

        // Pick random node
        let node = subnet
            .nodes
            .choose(&mut rand::thread_rng())
            .ok_or(ErrorCause::NoHealthyNodes)? // No healhy nodes in subnet
            .clone();

        Ok(node)
    }

    fn health(&self) -> ReplicaHealthStatus {
        // Return healthy state if we have at least one healthy replica node
        // TODO increase threshold? change logic?
        let rt = self.published_routes.load_full();

        match rt {
            Some(rt) => {
                if rt.node_count > 0 {
                    ReplicaHealthStatus::Healthy
                } else {
                    // There's no generic "Unhealthy" state it seems, should we use Starting?
                    ReplicaHealthStatus::CertifiedStateBehind
                }
            }

            // Usually this is only for the first 10sec after startup
            None => ReplicaHealthStatus::Starting,
        }
    }
}

#[cfg(feature = "tls")]
pub async fn acme_challenge(
    Extension(token): Extension<Arc<RwLock<Option<String>>>>,
) -> impl IntoResponse {
    token.read().await.clone().unwrap_or_default()
}

#[cfg(feature = "tls")]
pub async fn redirect_to_https(
    Host(host): Host,
    OriginalUri(uri): OriginalUri,
) -> impl IntoResponse {
    let fallback_path = PathAndQuery::from_static("/");
    let pq = uri.path_and_query().unwrap_or(&fallback_path).as_str();

    Redirect::permanent(
        &Uri::builder()
            .scheme("https") // redirect to https
            .authority(host) // re-use the same host
            .path_and_query(pq) // re-use the same path and query
            .build()
            .unwrap()
            .to_string(),
    )
}

// Logs the outcome of the request
pub async fn log_request(request: Request<Body>, next: Next<Body>) -> Result<Response, StatusCode> {
    let request_id = request.extensions().get::<RequestId>().unwrap().clone();
    let request_id = request_id.header_value().to_str().unwrap();

    let now = Instant::now();
    let response = next.run(request).await;
    let duration = now.elapsed();

    let ctx = response
        .extensions()
        .get::<RequestContext>()
        .cloned()
        .unwrap_or_default();

    // TODO easier way to get resp size?
    let response_size = response
        .headers()
        .get(header::CONTENT_LENGTH)
        .unwrap_or(&HeaderValue::from_str("0").unwrap())
        .to_str()
        .unwrap()
        .parse::<u32>()
        .unwrap();

    info!(
        request_id,
        request_type = format!("{}", ctx.request_type),
        error_cause = format!("{}", ctx.error_cause),
        error_details = ctx.error_cause.details(),
        status = response.status().as_u16(),
        subnet_id = ctx.node.as_ref().map(|x| x.subnet_id.to_string()),
        node_id = ctx.node.as_ref().map(|x| x.id.to_string()),
        canister_id = ctx.canister_id.map(|x| x.to_string()),
        canister_id_cbor = ctx.canister_id_cbor.map(|x| x.to_string()),
        sender = ctx.sender.map(|x| x.to_string()),
        method_name = ctx.method_name,
        duration = duration.as_secs_f64(),
        request_size = ctx.request_size,
        response_size,
    );

    Ok(response)
}

// Consumes request and returns it as byte slice
pub async fn read_body(request: Request<Body>) -> Result<(Parts, Vec<u8>), ErrorCause> {
    let (parts, body) = request.into_parts();
    let body = hyper::body::to_bytes(body)
        .await
        .map_err(|_| ErrorCause::UnableToReadBody)?
        .to_vec();

    Ok((parts, body))
}

// Parses body as a generic CBOR request and enriches the context
pub fn parse_body(ctx: &mut RequestContext, body: &[u8]) -> Result<(), Error> {
    let envelope: ICRequestEnvelope = serde_cbor::from_slice(body)?;
    let content = envelope.content;

    if let Some(v) = content.canister_id {
        ctx.canister_id_cbor = Some(Principal::try_from_slice(&v.0)?);
    }

    if let Some(v) = content.sender {
        ctx.sender = Some(Principal::try_from_slice(&v.0)?);
    }

    ctx.method_name = content.method_name;

    Ok(())
}

// Preprocess the request before handing it over to handlers
pub async fn preprocess_request(
    State(state): State<Arc<impl Proxier>>,
    mut request: Request<Body>,
    next: Next<Body>,
) -> Result<Response, Response> {
    let mut ctx = RequestContext::default();

    // Consume body
    let (mut parts, body) = read_body(request).await.map_err(|e| ctx.respond(e))?;
    ctx.request_size = body.len() as u32;

    // Extract & parse canister_id from URL if it's there
    if let Ok(Path(canister_id)) = parts.extract::<Path<String>>().await {
        let canister_id = Principal::from_text(canister_id).map_err(|e| {
            ctx.respond(ErrorCause::MalformedRequest(format!(
                "Unable to decode canister_id from URL: {e}"
            )))
        })?;

        ctx.canister_id = Some(canister_id);

        parse_body(&mut ctx, &body)
            .map_err(|e| ctx.respond(ErrorCause::UnableToParseCBOR(e.to_string())))?;

        // Try to look up a target node using canister id
        ctx.node = Some(state.lookup_node(canister_id).map_err(|e| ctx.respond(e))?);
    }

    // Reconstruct request back from parts
    let mut request = Request::from_parts(parts, hyper::Body::from(body));
    request.extensions_mut().insert(ctx);

    // Pass request to the next processor
    let resp = next.run(request).await;
    Ok(resp)
}

// Handles IC status call
pub async fn status(State(state): State<Arc<impl Proxier>>) -> Response {
    let response = HttpStatusResponse {
        // TODO which one to use?
        ic_api_version: "0.18.0".to_string(),
        root_key: None,
        impl_version: None,
        impl_hash: None,
        replica_health_status: Some(state.health()),
        certified_height: None,
    };

    // This shouldn't fail
    serde_cbor::to_vec(&response).unwrap().into_response()
}

// Handles IC query call
// TODO create generic request handler instead of per-call-type?
pub async fn query(
    State(state): State<Arc<impl Proxier>>,
    Extension(mut ctx): Extension<RequestContext>,
    request: Request<Body>,
) -> Result<Response, Response> {
    ctx.request_type = RequestType::Query;

    // These will be Some() if we got here, otherwise middleware would refuse request earlier
    let canister_id = ctx.canister_id.unwrap();
    let node = ctx.node.clone().unwrap();

    // Proxy the request
    let mut resp = state
        .proxy(RequestType::Query, request, node, canister_id)
        .await
        .map_err(|e| ctx.respond(e))?;

    // Inject context into response
    resp.extensions_mut().insert(ctx);

    Ok(resp)
}

pub async fn call(State(st): State<Arc<impl Proxier>>) -> impl IntoResponse {
    "Hello, World!"
}

pub async fn read_state(State(st): State<Arc<impl Proxier>>) -> impl IntoResponse {
    "Hello, World!"
}

#[cfg(test)]
pub mod test;
