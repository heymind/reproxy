use axum::{
    body::Body,
    extract::{Host, State},
    http::Request,
    response::Response,
    routing::any,
    Router,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};

use argh::FromArgs;

#[derive(FromArgs)]
/// reproxy - REgex (reserve) PROXY
struct CliArgs {
    /// specifies the host IP address
    #[argh(option, short = 'h', default = "String::from(\"127.0.0.1\")")]
    host: String,

    /// specifies the port number
    #[argh(option, short = 'p', default = "3333")]
    port: u16,

    /// specifies the configuration file
    #[argh(option, short = 'c')]
    config: Option<String>,

    /// show current version
    #[argh(switch)]
    version: bool,

    /// specifies the proxy items directly without config file (unimplemented)
    #[argh(positional, greedy)]
    proxy: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Config(HashMap<String, ProxyItemConfig>);

#[derive(Serialize, Deserialize)]
struct ProxyItemConfig {
    r#match: String,
    target: String,
    #[serde(default)]
    follow_redirect: bool,
    #[serde(default)]
    headers: HashMap<String, ProxyHeaderConfig>,
}
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum ProxyHeaderConfig {
    Passthrough,
    Ignore,

    Replace {
        #[serde(default)]
        r#match: String,
        #[serde(default)]
        replace: String,
    },
}

enum HeaderAction {
    Passthrough,
    Ignore,
    Replace { regex: Regex, replace: String },
}

struct ProxyItem {
    name: String,
    regex: Regex,
    replace: String,
    follow_redirect: bool,
    header_actions: HashMap<String, HeaderAction>,
    header_action_fallback: HeaderAction,
}

fn parse_config(config: &Config) -> anyhow::Result<Vec<ProxyItem>> {
    let mut items = Vec::new();
    for (name, item) in config.0.iter() {
        let re = Regex::new(&item.r#match)?;

        let mut actions = HashMap::new();
        let mut header_action_fallback = HeaderAction::Ignore;
        for (header_name, config) in item.headers.iter() {
            let action = match config {
                ProxyHeaderConfig::Passthrough => HeaderAction::Passthrough,
                ProxyHeaderConfig::Ignore => HeaderAction::Ignore,
                ProxyHeaderConfig::Replace { r#match, replace } => HeaderAction::Replace {
                    regex: Regex::new(r#match)?,
                    replace: replace.to_string(),
                },
            };
            if header_name == "$default" {
                header_action_fallback = action;
            } else {
                actions.insert(header_name.to_lowercase().clone(), action);
            }
        }
        items.push(ProxyItem {
            name: name.clone(),
            regex: re,
            replace: item.target.to_string(),
            follow_redirect: item.follow_redirect,
            header_actions: actions,
            header_action_fallback,
        });
    }
    Ok(items)
}

struct AppState {
    proxy_items: Vec<ProxyItem>,
}

#[axum::debug_handler]
async fn handle_request(
    Host(host): Host,
    State(state): State<Arc<AppState>>,
    mut request: Request<Body>,
) -> Response<Body> {
    return handle(&mut request, host, state)
        .await
        .unwrap_or_else(|err| {
            tracing::error!(
                method = ?request.method(),
                requested = request.uri().to_string(),
                error = ?err,
                status = 500
            );
            Response::builder()
                .status(500)
                .body(axum::body::Body::empty())
                .unwrap()
        });

    async fn handle(
        request: &mut Request<Body>,
        host: String,
        state: Arc<AppState>,
    ) -> anyhow::Result<Response<Body>> {
        let url = host + &request.uri().to_string();
        let matched_item = state
            .proxy_items
            .iter()
            .find(|item| item.regex.is_match(&url));
        if let Some(item) = matched_item {
            let target_url = item.regex.replace(&url, &item.replace);
            let client = reqwest::Client::builder()
                .redirect(if item.follow_redirect {
                    reqwest::redirect::Policy::limited(10)
                } else {
                    reqwest::redirect::Policy::none()
                })
                .build()?;
            let mut builder = client.request(request.method().clone(), target_url.as_ref());
            for (header_name, header_value) in request.headers().iter() {
                let name = header_name.as_str().to_lowercase();
                let action = item
                    .header_actions
                    .get(&name)
                    .unwrap_or(&item.header_action_fallback);
                match action {
                    HeaderAction::Passthrough => {
                        builder = builder.header(header_name, header_value)
                    }
                    HeaderAction::Replace { regex: re, replace } => {
                        let value = header_value.to_str()?;
                        if re.is_match(value) {
                            builder =
                                builder.header(header_name, re.replace(value, replace).as_ref());
                        } else {
                            tracing::error!(
                                method = ?request.method(),
                                requested = url,
                                matched = item.name,
                                status = 400,
                                unmatched_header = name
                            );
                            return Ok(Response::builder()
                                .status(400)
                                .body(axum::body::Body::empty())?);
                        }
                    }
                    _ => {}
                }
            }
            let subrequest = builder.body(std::mem::take(request.body_mut())).build()?;
            let mut subresp = client.execute(subrequest).await.map_err(|err| {
                tracing::error!(
                    method = ?request.method(),
                    requested = url,
                    matched = item.name,
                    forwarded = target_url.as_ref(),
                    error = ?err,
                );
                err
            })?;

            tracing::info!(
                method = ?request.method(),
                requested = url,
                matched = item.name,
                forwarded = target_url.as_ref(),
                status = subresp.status().as_u16(),
            );
            let mut builder = Response::builder().status(subresp.status());
            *builder.headers_mut().unwrap() = std::mem::take(subresp.headers_mut());
            Ok(builder.body(axum::body::Body::wrap_stream(subresp.bytes_stream()))?)
        } else {
            tracing::info!(
                method = ?request.method(),
                requested = url,
                status = 404
            );
            return Ok(Response::builder()
                .status(404)
                .body(axum::body::Body::empty())?);
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli_args: CliArgs = argh::from_env();

    if cli_args.version {
        println!("alpha");
        return Ok(())
    }

    let config: Config = serde_yaml::from_reader(std::fs::File::open(cli_args.config.unwrap())?)?;

    let state = AppState {
        proxy_items: parse_config(&config)?,
    };
    let app = Router::new()
        .route("/*_", any(handle_request))
        .with_state(Arc::new(state));
    tracing::info!(host = cli_args.host, port = cli_args.port, "listen");
    axum::Server::bind(
        &format!("{}:{}", cli_args.host, cli_args.port)
            .parse()
            .unwrap(),
    )
    .serve(app.into_make_service())
    .await
    .unwrap();
    Ok(())
}
