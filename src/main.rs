mod config;
mod middleware;
mod util;

use crate::config::Config;
use crate::util::{full, BoxBody, Result};
use bytes::Buf;
use cookie::time::{Duration, OffsetDateTime};
use cookie::{Cookie, SameSite};
use http_auth_basic::Credentials;
use http_body_util::BodyExt;
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, COOKIE, LOCATION, SET_COOKIE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{body::Incoming as IncomingBody, Request, Response};
use hyper::{Method, StatusCode};
use hyper_util::rt::TokioIo;
use jwt::{SignWithKey, VerifyWithKey};
use regex::Regex;
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use tokio::fs;
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};
use url::Url;

/* Header Names */
static FORWARDED_HOST: &str = "X-Forwarded-Host";
static FORWARDED_PROTO: &str = "X-Forwarded-Proto";
static FORWARDED_URI: &str = "X-Forwarded-Uri";
static FORWARDED_FOR: &str = "X-Forwarded-For";

/* File Paths */
static INDEX_DOCUMENT: &str = "/public/index.html";
static LOGOUT_DOCUMENT: &str = "/public/logout.html";
static PASSWD_FILE: &str = "/passwd";

/* HTTP Status Responses */
static NOT_FOUND: &[u8] = b"Not Found";
static UNAUTHORIZED: &[u8] = b"Unauthorized";
static AUTHORIZED: &[u8] = b"Authorized";
static LOGGED_OUT: &[u8] = b"Logged Out";

// Route table
async fn api(req: Request<hyper::body::Incoming>) -> Result<Response<BoxBody>> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/") | (&Method::GET, "/forward") => api_forward_auth(req).await,
        (&Method::POST, "/login") => api_login(req).await,
        (&Method::GET, "/login") => api_serve_file(INDEX_DOCUMENT, StatusCode::OK).await,
        (&Method::POST, "/logout") => api_logout().await,
        (&Method::GET, "/logout") => api_serve_file(LOGOUT_DOCUMENT, StatusCode::OK).await,
        _ => {
            api_serve_file(
                format!("/public{}", req.uri().path()).as_str(),
                StatusCode::OK,
            )
            .await
        }
    }
}

// ForwardAuth route
async fn api_forward_auth(req: Request<IncomingBody>) -> Result<Response<BoxBody>> {
    // Get token from request headers and check if cookie exists
    let headers = req.headers();
    if headers.contains_key(COOKIE) {
        // Grab cookies from headers
        let cookies = headers[COOKIE].to_str().unwrap();
        // Find jwt cookie (if exists)
        for cookie in Cookie::split_parse(cookies) {
            let cookie = cookie.unwrap();

            if cookie.name() == Config::global().cookie_name {
                // Found cookie, parse token and validate
                let token_str = cookie.value();
                let claims: BTreeMap<String, String> =
                    token_str.verify_with_key(&Config::global().key).unwrap();
                if claims["authenticated"] == "true" {
                    return api_serve_file(LOGOUT_DOCUMENT, StatusCode::OK).await;
                }
            }
        }
    }

    // Check if basic auth exists
    if headers.contains_key(AUTHORIZATION) {
        // Grab basic auth header and parse credentials
        let basic_auth = headers[AUTHORIZATION].to_str().unwrap();
        let credentials = Credentials::from_header(basic_auth.to_string()).unwrap();

        // Check login against passwd file
        let find_hash = get_user_hash(&credentials.user_id).await?;
        if find_hash.is_some() {
            // User found, verify password with hash
            let hash = find_hash.unwrap();
            let verify = pwhash::unix::verify(&credentials.password, &hash);
            if verify {
                // Correct login
                return api_serve_file(LOGOUT_DOCUMENT, StatusCode::OK).await;
            }
        }
    }

    // No valid cookie/jwt found, create redirect url and return
    let mut location =
        Url::parse(format!("http://{}/login", &Config::global().auth_host).as_str())?;

    if headers.contains_key(FORWARDED_HOST)
        && headers[FORWARDED_HOST].to_str().unwrap() != Config::global().auth_host
    {
        let mut referral_url =
            Url::parse(format!("http://{}", headers[FORWARDED_HOST].to_str().unwrap()).as_str())?;
        if headers.contains_key(FORWARDED_PROTO) {
            if let Err(_e) = referral_url.set_scheme(headers[FORWARDED_PROTO].to_str().unwrap()) {
                println!("Error: Failed setting protocol for referral url.");
            }
        }
        if headers.contains_key(FORWARDED_URI) {
            referral_url.set_path(headers[FORWARDED_URI].to_str().unwrap());
        }
        location.set_query(Some(format!("r={}", referral_url).as_str()));
    }

    Ok(Response::builder()
        .status(StatusCode::TEMPORARY_REDIRECT)
        .header(LOCATION, location.to_string())
        .body(full(UNAUTHORIZED))
        .unwrap())
}

// Login route
async fn api_login(req: Request<IncomingBody>) -> Result<Response<BoxBody>> {
    // Middleware: rate limiter
    let headers = req.headers();
    if headers.contains_key(FORWARDED_FOR) {
        let x_forwarded_for = headers[FORWARDED_FOR].to_str().unwrap();
        let source_ip = x_forwarded_for.split(',').next().unwrap_or(x_forwarded_for);
        if let Ok(ip) = source_ip.trim().parse::<IpAddr>() {
            // Call rate limiter
        } else {
            println!(
                "Warning: Failed to parse {} header from request.",
                FORWARDED_FOR
            );
        }
    } else {
        println!(
            "Warning: Request without {} header processed.",
            FORWARDED_FOR
        );
    }

    // Aggregate request body
    let body = req.collect().await?.aggregate();
    // Decode JSON
    let data: serde_json::Value = serde_json::from_reader(body.reader())?;
    // Process login and find user in passwd file
    let user = data["username"].as_str().unwrap();
    let find_hash = get_user_hash(user).await?;
    if find_hash.is_some() {
        // User found, verify password with hash
        let hash = find_hash.unwrap();
        let pass = data["password"].as_str().unwrap();
        let verify = pwhash::unix::verify(pass, &hash);
        if verify {
            // Correct login, generate claims and token
            let mut claims = BTreeMap::new();
            claims.insert("authenticated", "true");
            claims.insert("user", user);
            let mut now = OffsetDateTime::now_utc();
            now += Duration::days(7);
            let cookie = Cookie::build(
                &Config::global().cookie_name,
                claims.sign_with_key(&Config::global().key).unwrap(),
            )
            .domain(&Config::global().cookie_domain)
            .http_only(true)
            .secure(Config::global().cookie_secure)
            .same_site(SameSite::Strict)
            .expires(now)
            .finish();

            // Return OK with cookie
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header(SET_COOKIE, cookie.to_string())
                .body(full(AUTHORIZED))
                .unwrap());
        }
    }

    // Incorrect login, respond with unauthorized
    Ok(Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .body(full(UNAUTHORIZED))
        .unwrap())
}

// Logout route
async fn api_logout() -> Result<Response<BoxBody>> {
    // Build cookie in past to expire existing cookies in browser
    let past = OffsetDateTime::now_utc() - Duration::days(1);
    let expired_cookie = Cookie::build(&Config::global().cookie_name, "")
        .domain(&Config::global().cookie_domain)
        .http_only(true)
        .secure(Config::global().cookie_secure)
        .same_site(SameSite::Strict)
        .expires(past)
        .finish();

    // Return OK with cookie
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(SET_COOKIE, expired_cookie.to_string())
        .body(full(LOGGED_OUT))
        .unwrap())
}

// Serve file route
async fn api_serve_file(filename: &str, status_code: StatusCode) -> Result<Response<BoxBody>> {
    if let Ok(contents) = tokio::fs::read(filename).await {
        // Get mimetype of file
        let mimetype = mime_guess::from_path(filename);
        if !mimetype.is_empty() {
            return Ok(Response::builder()
                .header(CONTENT_TYPE, mimetype.first().unwrap().to_string())
                .status(status_code)
                .body(full(contents))
                .unwrap());
        }

        return Ok(Response::builder()
            .status(status_code)
            .body(full(contents))
            .unwrap());
    }

    // 404, not found
    Ok(Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(full(NOT_FOUND))
        .unwrap())
}

async fn get_user_hash(user: &str) -> Result<Option<String>> {
    if let Ok(passwd) = fs::read_to_string(PASSWD_FILE).await {
        let pattern = format!(r"(\n|^){}:(.+)(\n|$)", user);
        let regex = Regex::new(&pattern).unwrap();
        let user_match = regex.captures(&passwd);
        if user_match.is_some() {
            let captures = user_match.unwrap();
            return Ok(Some(captures.get(2).map_or("", |m| m.as_str()).to_string()));
        }
    }
    Ok(None)
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // Initialize config (port, token secret, auth backend url, ...)
    Config::initialize()?;
    println!("Loaded configuration.");

    // Setup signal handling
    let mut term_signal = signal(SignalKind::terminate())?;
    let mut int_signal = signal(SignalKind::interrupt())?;

    // Create TcpListener and bind
    let addr = SocketAddr::from(([0, 0, 0, 0], Config::global().port));
    let listener = TcpListener::bind(addr).await?;
    println!("Listening on http://{}", addr);

    // Create a future to handle server startup
    let server = async {
        // Start loop to continuously accept incoming connections
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let io = TokioIo::new(stream);

                    // Spawn a tokio task to serve multiple connections concurrently
                    tokio::task::spawn(async move {
                        // Finally, bind the incoming connection to our index service
                        if let Err(err) = http1::Builder::new()
                            // Convert function to service
                            .serve_connection(io, service_fn(api))
                            .await
                        {
                            println!("Error: Failed serving connection: {:?}", err);
                        }
                    });
                }
                Err(e) => {
                    println!("Error accepting connection: {:?}", e);
                    continue;
                }
            };
        }
    };

    // Create a future to handle signals
    let shutdown_signal = async {
        tokio::select! {
            _ = term_signal.recv() => {},
            _ = int_signal.recv() => {},
        }
    };

    // Run server with signal handling
    tokio::select! {
        _ = server => {},
        _ = shutdown_signal => {
            println!("Shutdown signal received, shutting down...")
        },
    }
    Ok(())
}
