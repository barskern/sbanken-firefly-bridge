use anyhow::{anyhow, Context, Result};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use secrecy::{ExposeSecret, Secret};
use structopt::StructOpt;

#[derive(StructOpt, Debug)]
#[structopt(about, author)]
struct Opts {
    #[structopt(long, env, hide_env_values = true)]
    sbanken_client_id: Secret<String>,
    #[structopt(long, env, hide_env_values = true)]
    sbanken_client_secret: Secret<String>,
    #[structopt(long, env, hide_env_values = true)]
    sbanken_customer_id: Secret<String>,
    #[structopt(long, env)]
    sbanken_auth_url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opts::from_args();

    let auth_token = get_auth_token(
        &opt.sbanken_auth_url,
        &opt.sbanken_client_id,
        &opt.sbanken_client_secret,
    )
    .await
    .context("unable to get auth token")?;

    dbg!(auth_token);

    Ok(())
}

async fn get_auth_token(
    auth_url: &str,
    client_id: &Secret<String>,
    client_secret: &Secret<String>,
) -> Result<Secret<String>> {
    #[derive(Debug, serde::Deserialize)]
    struct AuthSuccess {
        access_token: Secret<String>,
    }
    #[derive(Debug, serde::Deserialize)]
    struct AuthError {
        error: String,
    }
    #[derive(Debug, serde::Deserialize)]
    #[serde(untagged)]
    enum AuthResponse {
        Success(AuthSuccess),
        Error(AuthError),
    }

    let auth_response: AuthResponse = reqwest::Client::new()
        .post(auth_url)
        .header(reqwest::header::ACCEPT, "application/json")
        .basic_auth(
            utf8_percent_encode(client_id.expose_secret(), NON_ALPHANUMERIC),
            Some(utf8_percent_encode(
                client_secret.expose_secret(),
                NON_ALPHANUMERIC,
            )),
        )
        .form(&[("grant_type", "client_credentials")])
        .send()
        .await?
        .json()
        .await?;

    match auth_response {
        AuthResponse::Success(AuthSuccess { access_token }) => Ok(access_token),
        AuthResponse::Error(AuthError { error }) => {
            Err(anyhow!("received error from api: {}", error))
        }
    }
}
