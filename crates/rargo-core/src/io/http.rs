//! One HTTP request to another server, carried out as it is described.

use std::io::Read;
use std::time::Duration;

/// A request to send. Every choice about it is the caller's.
pub struct Request<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    /// Fields to send as a URL-encoded form body.
    pub form: Option<&'a [(&'a str, &'a str)]>,
    /// How many redirects to follow, or the library's own limit when `None`.
    pub redirects: Option<u32>,
    pub user_agent: Option<&'a str>,
    pub timeout: Duration,
    /// The most bytes of the body to read. Zero reads none of it.
    pub body_limit: u64,
}

/// What the server answered.
pub struct Response {
    pub status: u16,
    /// The library's description of the answer when its status is an error. The body of such an answer is not read.
    pub refused: Option<String>,
    pub location: Option<String>,
    pub body: Vec<u8>,
}

/// Why there is no answer.
pub enum Failure {
    /// The request did not get an answer.
    Send(Box<dyn std::error::Error + Send + Sync>),
    /// The answer's body could not be read.
    Body(std::io::Error),
}

/// Sends `request` and waits for the answer. A simulation has no servers to ask.
pub fn fetch(request: &Request) -> Result<Response, Failure> {
    if super::sim::active() {
        return Err(Failure::Send("a simulation cannot reach another server".into()));
    }
    let mut agent = ureq::AgentBuilder::new().timeout(request.timeout);
    if let Some(n) = request.redirects {
        agent = agent.redirects(n);
    }
    if let Some(name) = request.user_agent {
        agent = agent.user_agent(name);
    }
    let mut call = agent.build().request(request.method, request.url);
    for (name, value) in request.headers {
        call = call.set(name, value);
    }
    let sent = match request.form {
        Some(fields) => call.send_form(fields),
        None => call.call(),
    };
    let (answer, refused) = match sent {
        Ok(answer) => (answer, None),
        Err(err @ ureq::Error::Status(..)) => {
            let said = err.to_string();
            let ureq::Error::Status(_, answer) = err else { unreachable!("the error is a status") };
            (answer, Some(said))
        }
        Err(err) => return Err(Failure::Send(Box::new(err))),
    };
    let status = answer.status();
    let location = answer.header("location").map(str::to_string);
    let mut body = Vec::new();
    if refused.is_none() && request.body_limit > 0 {
        answer.into_reader().take(request.body_limit).read_to_end(&mut body).map_err(Failure::Body)?;
    }
    Ok(Response { status, refused, location, body })
}
