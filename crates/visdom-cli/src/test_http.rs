//! A local HTTP server for tests: canned JSON answers per `METHOD /path`, and
//! a record of what was asked.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};

/// One request the server received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recorded {
    /// `GET /api/settings` (the query string included).
    pub route: String,
    pub authorization: Option<String>,
    pub body: String,
}

pub struct MockServer {
    pub url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl MockServer {
    /// Serve `routes` (`("GET /path", status, json body)`) until the test
    /// process ends. A route asked for more often than listed keeps giving
    /// its last answer; listing it twice gives two answers in turn. Anything
    /// else is a 404. `{URL}` in a body stands for this server's address.
    pub fn start(routes: &[(&str, u16, &str)]) -> MockServer {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut answers: Vec<(String, u16, String)> = routes
            .iter()
            .map(|(route, status, body)| (route.to_string(), *status, body.replace("{URL}", &url)))
            .collect();
        let seen = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                let position = answers
                    .iter()
                    .position(|(route, ..)| *route == request.route);
                let (status, body) = match position {
                    Some(i) => {
                        let later = answers[i + 1..]
                            .iter()
                            .any(|(route, ..)| *route == request.route);
                        let (_, status, body) = if later {
                            answers.remove(i)
                        } else {
                            answers[i].clone()
                        };
                        (status, body)
                    }
                    None => (404, r#"{"error":"no such route"}"#.to_string()),
                };
                seen.lock().unwrap().push(request);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        MockServer { url, requests }
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    /// The routes asked for, in order.
    pub fn routes(&self) -> Vec<String> {
        self.requests().into_iter().map(|r| r.route).collect()
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<Recorded> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let route = format!("{} {}", parts.next()?, parts.next()?);
    let mut authorization = None;
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "authorization" => authorization = Some(value.trim().to_string()),
                "content-length" => length = value.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some(Recorded {
        route,
        authorization,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}
