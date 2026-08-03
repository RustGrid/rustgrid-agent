// Extracted from the hosted execution composition root.
use super::*;
use reqwest::{blocking::Client, header};

pub(super) fn request_github_oidc(
    http: &Client,
    environment: &GithubActionsEnvironment,
) -> Result<SecretString> {
    let mut url = environment.oidc_request_url.clone();
    url.query_pairs_mut()
        .append_pair("audience", &environment.audience);
    let response = http
        .get(url)
        .bearer_auth(environment.oidc_request_token.expose())
        .header(header::ACCEPT, "application/json")
        .send()
        .context("GitHub OIDC token request failed")?;
    if !response.status().is_success() {
        bail!("GitHub OIDC token request returned {}", response.status());
    }
    let body: GithubOidcResponse = decode_success(response, "GitHub OIDC token response")?;
    validate_github_oidc_token(&body.value)?;
    SecretString::new(body.value, "GitHub OIDC token")
}

pub(super) fn exchange_github_oidc(
    http: &Client,
    environment: &GithubActionsEnvironment,
    execution_id: Uuid,
    oidc_token: &SecretString,
) -> Result<ExchangeResponse> {
    let url = environment
        .api_root
        .join("execution-auth/github-actions/exchange")?;
    let response = http
        .post(url)
        .header(header::ACCEPT, "application/json")
        .json(&json!({
            "execution_id": execution_id,
            "dispatch_nonce": environment.dispatch_nonce.expose(),
            "github_oidc_token": oidc_token.expose()
        }))
        .send()
        .context("RustGrid GitHub OIDC exchange transport failed")?;
    decode_response(response, "execution-auth/github-actions/exchange")
}
