//! HTTP requests to a rargo server's web endpoint.

use std::time::Duration;

/// How a request failed.
pub enum FailureKind {
    /// The server answered with an error status and this body.
    Status { code: u16, body: String },
    /// The host's name did not resolve.
    Dns,
    /// The connection could not be made or broke.
    Unreachable,
    /// Any other failure before an answer arrived.
    Transport,
    /// The answer's body could not be read.
    Body,
}

/// A failed request: what kind of failure, and the error's own words.
pub struct Failure {
    pub kind: FailureKind,
    pub text: String,
}

fn agent(connect: Duration, whole: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(connect).timeout(whole).build()
}

fn answer(result: Result<ureq::Response, ureq::Error>) -> Result<String, Failure> {
    match result {
        Ok(response) => response.into_string().map_err(|e| Failure { kind: FailureKind::Body, text: e.to_string() }),
        Err(e) => {
            let text = e.to_string();
            let kind = match e {
                ureq::Error::Status(code, response) => FailureKind::Status { code, body: response.into_string().unwrap_or_default() },
                ureq::Error::Transport(t) => match t.kind() {
                    ureq::ErrorKind::Dns => FailureKind::Dns,
                    ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::Io => FailureKind::Unreachable,
                    _ => FailureKind::Transport,
                },
            };
            Err(Failure { kind, text })
        }
    }
}

/// The body of a GET of `url`.
pub fn get(url: &str, connect: Duration, whole: Duration) -> Result<String, Failure> {
    answer(agent(connect, whole).get(url).call())
}

/// The body of the answer to a POST of the JSON text `body` to `url`.
pub fn post_json(url: &str, body: &str, connect: Duration, whole: Duration) -> Result<String, Failure> {
    answer(agent(connect, whole).post(url).set("Content-Type", "application/json").send_string(body))
}
