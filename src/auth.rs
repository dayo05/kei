use axum::http::{HeaderMap, StatusCode, Uri, header};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

use crate::config::{Config, ProjectConfig, Visibility};
use crate::error::ApiError;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone)]
pub struct Principal {
    pub account: String,
    pub admin: bool,
}

pub fn authenticate(headers: &HeaderMap, config: &Config) -> Option<Principal> {
    let token = bearer_token(headers).or_else(|| cookie_token(headers))?;
    config
        .auth
        .accounts
        .iter()
        .find(|account| account.token == token)
        .map(|account| Principal {
            account: account.name.clone(),
            admin: account.admin,
        })
}

pub fn authenticate_login(account: &str, token: &str, config: &Config) -> Option<Principal> {
    config
        .auth
        .accounts
        .iter()
        .find(|configured| configured.name == account && configured.token == token)
        .map(|account| Principal {
            account: account.name.clone(),
            admin: account.admin,
        })
}

pub fn can_view_project(principal: Option<&Principal>, project: &ProjectConfig) -> bool {
    match project.visibility {
        Visibility::Public => true,
        Visibility::Restricted => principal.is_some_and(|p| {
            p.admin
                || project
                    .allowed_accounts
                    .iter()
                    .any(|name| name == &p.account)
        }),
        Visibility::Private => principal.is_some_and(|p| p.admin),
    }
}

#[allow(dead_code)]
pub fn require_project_access(
    principal: Option<&Principal>,
    project: &ProjectConfig,
) -> Result<(), ApiError> {
    if can_view_project(principal, project) {
        return Ok(());
    }
    let _ = principal;
    Err(ApiError::new(StatusCode::NOT_FOUND, "not found"))
}

pub fn require_project_or_public_build_access(
    principal: Option<&Principal>,
    project: &ProjectConfig,
    secret: Option<&str>,
    build_id: Uuid,
    token: Option<&str>,
) -> Result<(), ApiError> {
    if can_view_project(principal, project)
        || token.is_some_and(|token| verify_public_build_token(secret, build_id, token))
    {
        return Ok(());
    }
    Err(ApiError::new(StatusCode::NOT_FOUND, "not found"))
}

pub fn require_project_or_public_path_access(
    principal: Option<&Principal>,
    project: &ProjectConfig,
    secret: Option<&str>,
    uri: &Uri,
) -> Result<(), ApiError> {
    if can_view_project(principal, project)
        || query_param(uri.query(), "token")
            .is_some_and(|token| verify_public_path_token(secret, uri.path(), token))
    {
        return Ok(());
    }
    Err(ApiError::new(StatusCode::NOT_FOUND, "not found"))
}

#[cfg_attr(not(feature = "maven"), allow(dead_code))]
pub fn can_view_project_maven(principal: Option<&Principal>, project: &ProjectConfig) -> bool {
    let maven_public = project.maven.public.unwrap_or(true);
    if maven_public {
        return true;
    }
    can_view_project(principal, project)
}

#[cfg_attr(not(feature = "maven"), allow(dead_code))]
pub fn require_project_maven_access(
    principal: Option<&Principal>,
    project: &ProjectConfig,
) -> Result<(), ApiError> {
    if can_view_project_maven(principal, project) {
        return Ok(());
    }
    let _ = principal;
    Err(ApiError::new(StatusCode::NOT_FOUND, "not found"))
}

pub fn visible_projects<'a>(
    principal: Option<&Principal>,
    config: &'a Config,
) -> impl Iterator<Item = &'a ProjectConfig> {
    config
        .projects
        .iter()
        .filter(move |project| can_view_project(principal, project))
}

#[cfg_attr(not(feature = "discord"), allow(dead_code))]
pub fn public_artifact_url(base: &str, secret: &str, artifact_path: &str) -> String {
    let token = public_artifact_token(secret, artifact_path);
    format!("{base}/public/artifacts/{artifact_path}?token={token}")
}

#[cfg_attr(not(feature = "discord"), allow(dead_code))]
pub fn public_build_url(base: &str, secret: &str, build_id: Uuid) -> String {
    let token = public_build_token(secret, build_id);
    format!("{base}/builds/{build_id}?token={token}")
}

#[cfg_attr(not(feature = "discord"), allow(dead_code))]
pub fn public_path_url(base: &str, secret: &str, path: &str) -> String {
    let token = public_path_token(secret, path);
    format!("{base}{path}?token={token}")
}

pub fn verify_public_artifact_token(
    secret: Option<&str>,
    artifact_path: &str,
    token: &str,
) -> bool {
    verify_public_token(secret, &format!("artifact:{artifact_path}"), token)
        || verify_legacy_public_artifact_token(secret, artifact_path, token)
}

pub fn verify_public_build_token(secret: Option<&str>, build_id: Uuid, token: &str) -> bool {
    verify_public_token(secret, &format!("build:{build_id}"), token)
}

pub fn verify_public_path_token(secret: Option<&str>, path: &str, token: &str) -> bool {
    verify_public_token(secret, &format!("path:{path}"), token)
}

pub fn query_param<'a>(query: Option<&'a str>, name: &str) -> Option<&'a str> {
    query?.split('&').find_map(|part| {
        let (key, value) = part.split_once('=')?;
        (key == name).then_some(value)
    })
}

fn verify_public_token(secret: Option<&str>, scope: &str, token: &str) -> bool {
    let Some(secret) = secret else {
        return false;
    };
    let Ok(expected) = hex::decode(token) else {
        return false;
    };
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any size");
    mac.update(scope.as_bytes());
    mac.verify_slice(&expected).is_ok()
}

#[cfg_attr(not(feature = "discord"), allow(dead_code))]
fn public_artifact_token(secret: &str, artifact_path: &str) -> String {
    public_token(secret, &format!("artifact:{artifact_path}"))
}

fn public_build_token(secret: &str, build_id: Uuid) -> String {
    public_token(secret, &format!("build:{build_id}"))
}

fn public_path_token(secret: &str, path: &str) -> String {
    public_token(secret, &format!("path:{path}"))
}

fn public_token(secret: &str, scope: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any size");
    mac.update(scope.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn verify_legacy_public_artifact_token(
    secret: Option<&str>,
    artifact_path: &str,
    token: &str,
) -> bool {
    let Some(secret) = secret else {
        return false;
    };
    let Ok(expected) = hex::decode(token) else {
        return false;
    };
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any size");
    mac.update(artifact_path.as_bytes());
    mac.verify_slice(&expected).is_ok()
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn cookie_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|cookie| {
                let (name, value) = cookie.trim().split_once('=')?;
                (name == "kei_token").then_some(value)
            })
        })
        .filter(|s| !s.is_empty())
}
