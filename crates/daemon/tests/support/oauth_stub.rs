//! A stub MCP server with its own OAuth authorization server, on 127.0.0.1.
//!
//! It serves the protected resource metadata, the authorization server metadata, dynamic client
//! registration, an authorization endpoint that grants at once, a token endpoint that checks PKCE,
//! and an MCP endpoint with one tool that needs the current access token. No real provider is used.
#![allow(dead_code)]

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::{Value, json};
use sha2::Digest;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The tokens that the stub gives out. No test output, setting or log may hold them.
pub const ACCESS: &str = "oauth-access-token-1f7e3a";
pub const REFRESHED: &str = "oauth-access-token-2b8a51";
pub const REFRESH: &str = "oauth-refresh-token-9c2d64";
pub const CODE: &str = "oauth-code-5e0b17";
/// The client ID that dynamic registration gives out.
pub const REGISTERED: &str = "client-registered";

/// One request as the stub received it.
#[derive(Debug, Clone)]
pub struct Seen {
    pub host: String,
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
}

/// What the stub does and what it saw. The tests change the knobs between steps.
#[derive(Default)]
pub struct Knobs {
    pub seen: Vec<Seen>,
    /// The access token that the MCP endpoint accepts now.
    pub valid: Option<String>,
    /// `expires_in` of the token from the code. The refresh gives 3600.
    pub expires_in: u64,
    /// The authorization server refuses the refresh token with `invalid_grant`.
    pub refuse_refresh: bool,
    /// The token from the code comes without a refresh token.
    pub no_refresh_token: bool,
    /// The host name that the metadata gives for the endpoints of the authorization server.
    pub endpoint_host: String,
    /// The token endpoint answers 307 to `/token-moved`.
    pub move_token_endpoint: bool,
    /// The PKCE challenge, the redirect address and the client of the last authorization.
    authorized: Option<(String, String, String)>,
    pub refreshes: u32,
    pub registrations: Vec<Value>,
}

#[derive(Clone)]
pub struct Stub {
    /// `http://127.0.0.1:<port>`.
    pub base: String,
    port: u16,
    pub knobs: Arc<Mutex<Knobs>>,
}

impl Stub {
    /// The address of the MCP endpoint.
    pub fn mcp_url(&self) -> String {
        format!("{}/mcp", self.base)
    }

    pub fn knobs(&self) -> std::sync::MutexGuard<'_, Knobs> {
        self.knobs.lock().unwrap()
    }

    /// The requests to `path`.
    pub fn count(&self, path: &str) -> usize {
        self.knobs().seen.iter().filter(|seen| seen.path == path).count()
    }

    fn endpoint(&self, path: &str) -> String {
        format!("http://{}:{}{path}", self.knobs().endpoint_host, self.port)
    }
}

#[derive(Clone)]
struct Tools;

impl ServerHandler for Tools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let Value::Object(schema) = json!({"type": "object"}) else {
            unreachable!()
        };
        Ok(ListToolsResult::with_all_items(vec![Tool::new(
            "hello",
            "Says hello",
            schema,
        )]))
    }

    async fn call_tool(
        &self,
        _request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        Ok(CallToolResult::success(vec![ContentBlock::text("hello")]).into())
    }
}

fn reply(status: StatusCode, body: Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn form(bytes: &[u8]) -> HashMap<String, String> {
    url::form_urlencoded::parse(bytes).into_owned().collect()
}

fn seen_of(request: &Request) -> Seen {
    let header = |name| {
        let value = request.headers().get(name)?;
        value.to_str().ok().map(str::to_owned)
    };
    Seen {
        host: header(header::HOST).unwrap_or_default(),
        method: request.method().to_string(),
        path: request.uri().path().to_owned(),
        authorization: header(header::AUTHORIZATION),
    }
}

/// Records every request. The MCP endpoint needs the valid access token.
async fn record(State(stub): State<Stub>, request: Request, next: Next) -> Response {
    let seen = seen_of(&request);
    let refused = {
        let mut knobs = stub.knobs();
        knobs.seen.push(seen.clone());
        let valid = knobs.valid.as_ref().map(|token| format!("Bearer {token}"));
        seen.path == "/mcp" && (valid.is_none() || seen.authorization != valid)
    };
    if refused {
        let challenge = format!(
            "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\"",
            stub.base
        );
        return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, challenge)]).into_response();
    }
    next.run(request).await
}

async fn oauth(State(stub): State<Stub>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    let query = form(request.uri().query().unwrap_or_default().as_bytes());
    let body = axum::body::to_bytes(request.into_body(), 1 << 20).await.unwrap();
    let base = &stub.base;
    match path.as_str() {
        "/.well-known/oauth-protected-resource/mcp" => reply(
            StatusCode::OK,
            json!({"resource": stub.mcp_url(), "authorization_servers": [base]}),
        ),
        "/.well-known/oauth-authorization-server" => reply(
            StatusCode::OK,
            json!({
                "issuer": base,
                "authorization_endpoint": stub.endpoint("/authorize"),
                "token_endpoint": stub.endpoint("/token"),
                "registration_endpoint": stub.endpoint("/register"),
                "response_types_supported": ["code"],
                "code_challenge_methods_supported": ["S256"],
            }),
        ),
        "/register" => {
            let request: Value = serde_json::from_slice(&body).unwrap();
            let uris = request["redirect_uris"].clone();
            stub.knobs().registrations.push(request);
            reply(
                StatusCode::CREATED,
                json!({"client_id": REGISTERED, "redirect_uris": uris}),
            )
        }
        // A browser that is signed in already: the server grants at once.
        "/authorize" => {
            let challenge = query["code_challenge"].clone();
            let redirect = query["redirect_uri"].clone();
            stub.knobs().authorized = Some((challenge, redirect.clone(), query["client_id"].clone()));
            let location = format!("{redirect}?code={CODE}&state={}", query["state"]);
            Response::builder()
                .status(StatusCode::FOUND)
                .header(header::LOCATION, location)
                .body(Body::empty())
                .unwrap()
        }
        "/token" if stub.knobs().move_token_endpoint => Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header(header::LOCATION, format!("{base}/token-moved"))
            .body(Body::empty())
            .unwrap(),
        "/token" => token(&stub, &form(&body)),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

fn token(stub: &Stub, form: &HashMap<String, String>) -> Response {
    let mut knobs = stub.knobs();
    let refused = |error: &str| reply(StatusCode::BAD_REQUEST, json!({"error": error}));
    match form.get("grant_type").map(String::as_str) {
        Some("authorization_code") => {
            let Some((challenge, redirect, client)) = knobs.authorized.take() else {
                return refused("invalid_grant");
            };
            let verifier = form.get("code_verifier").cloned().unwrap_or_default();
            let computed =
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
            let fits = form.get("code").map(String::as_str) == Some(CODE)
                && computed == challenge
                && form.get("redirect_uri") == Some(&redirect)
                && form.get("client_id") == Some(&client);
            if !fits {
                return refused("invalid_grant");
            }
            knobs.valid = Some(ACCESS.into());
            let mut answer = json!({"access_token": ACCESS, "token_type": "Bearer", "expires_in": knobs.expires_in});
            if !knobs.no_refresh_token {
                answer["refresh_token"] = json!(REFRESH);
            }
            reply(StatusCode::OK, answer)
        }
        Some("refresh_token") => {
            knobs.refreshes += 1;
            if knobs.refuse_refresh || form.get("refresh_token").map(String::as_str) != Some(REFRESH) {
                return refused("invalid_grant");
            }
            knobs.valid = Some(REFRESHED.into());
            reply(
                StatusCode::OK,
                json!({"access_token": REFRESHED, "token_type": "Bearer", "expires_in": 3600}),
            )
        }
        _ => refused("unsupported_grant_type"),
    }
}

/// Starts the stub. Its tokens live an hour, and its endpoints are on 127.0.0.1.
pub async fn start() -> Stub {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stub = Stub {
        base: format!("http://127.0.0.1:{port}"),
        port,
        knobs: Arc::new(Mutex::new(Knobs {
            expires_in: 3600,
            endpoint_host: "127.0.0.1".into(),
            ..Knobs::default()
        })),
    };
    let mcp = StreamableHttpService::new(
        || Ok(Tools),
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_allowed_hosts(["127.0.0.1"]),
    );
    let app = axum::Router::new()
        .route_service("/mcp", mcp)
        .fallback(oauth)
        .layer(from_fn_with_state(stub.clone(), record))
        .with_state(stub.clone());
    tokio::spawn(async move { axum::serve(listener, app).await });
    stub
}

/// What a signed-in browser does with the authorization address: it follows the answer of the
/// authorization endpoint. Returns the address the browser goes to next, with the code.
pub async fn browser(authorization_url: &str) -> String {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let answer = client.get(authorization_url).send().await.unwrap();
    assert_eq!(answer.status(), StatusCode::FOUND, "{authorization_url}");
    answer.headers()[header::LOCATION].to_str().unwrap().to_owned()
}
