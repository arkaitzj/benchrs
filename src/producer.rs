use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fmt::Write;
use url::Url;

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum RequestMethod {
    Get,
    Post,
    Head,
}

impl std::str::FromStr for RequestMethod {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_uppercase().as_str() {
            "GET" => Ok(RequestMethod::Get),
            "POST" => Ok(RequestMethod::Post),
            "HEAD" => Ok(RequestMethod::Head),
            _ => Err("Invalid method"),
        }
    }
}

impl std::fmt::Display for RequestMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                RequestMethod::Get => "GET",
                RequestMethod::Post => "POST",
                RequestMethod::Head => "HEAD",
            }
        )
    }
}

#[derive(Clone)]
pub struct ProducerRequest {
    config: RequestConfig,
    pub addr: String,
    host: String,
    path: String,
    pub method: RequestMethod,
    headers: Vec<String>,
    body: Option<Vec<u8>>,
}

#[derive(Clone)]
pub struct RequestConfig {
    pub keepalive: bool,
    pub useragent: String,
}
impl Default for RequestConfig {
    fn default() -> Self {
        RequestConfig {
            keepalive: false,
            useragent: format!("BenchRs/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

const LOCALHOST: url::Host<&str> = url::Host::Domain("localhost");
impl ProducerRequest {
    pub fn new(
        addr: &str,
        method: RequestMethod,
        user_headers: Vec<String>,
        body: Option<Vec<u8>>,
        config: RequestConfig,
    ) -> Result<Self> {
        let (host, path) = url_to_hostpath(addr).context("Converting url to host and path")?;

        Ok(ProducerRequest {
            addr: addr.to_string(),
            path,
            host,
            method,
            config,
            body,
            headers: user_headers,
        })
    }
    pub fn redirect(&mut self, addr: &str) -> Result<()> {
        let (host, path) = url_to_hostpath(addr)?;
        self.addr = addr.to_string();
        self.host = host;
        self.path = path;
        // Ensure we do not override Host header with user supplied host
        self.headers.retain(|h| !h.starts_with("Host:"));
        Ok(())
    }

    pub fn get_request(&self) -> Vec<u8> {
        let connection = if self.config.keepalive {
            "keep-alive"
        } else {
            "close"
        };
        let body_len = if let Some(ref body) = self.body {
            body.len()
        } else {
            0
        };
        let default_headers = HashMap::from([
            ("Host:", &*self.host),
            ("Accept:", "*/*"),
            ("Connection:", connection),
            ("User-Agent:", &*self.config.useragent),
        ]);
        let mut headers = String::new();
        for (key, value) in default_headers {
            if !caseless_find(&self.headers, key) {
                write!(headers, "{} {}\r\n", key, value).expect("Infallible")
            }
        }
        if body_len > 0 {
            write!(headers, "Content-Length: {}\r\n", body_len).expect("Infallible")
        }
        self.headers
            .iter()
            .for_each(|header| write!(headers, "{}\r\n", header).expect("Infallible"));

        // Construct a request.
        let header = format!("{} {} HTTP/1.1\r\n{}\r\n", self.method, self.path, headers);
        let mut request_buf = Vec::new();
        request_buf.reserve_exact(header.len() + body_len);
        request_buf.extend_from_slice(header.as_bytes());
        if body_len > 0 {
            request_buf.extend_from_slice(self.body.as_ref().unwrap());
        }
        request_buf
    }
}

fn url_to_hostpath(addr: &str) -> Result<(String, String)> {
    let url: Url = Url::parse(addr).context(format!("Cannot parse [{}] as url", addr))?;
    let path = url.path().to_string();
    let query = match url.query() {
        Some(q) => format!("?{}", q),
        None => String::new(),
    };
    let host = url.host().unwrap_or(LOCALHOST).to_string();
    let path = format!("{}{}", path, query);
    Ok((host, path))
}

fn caseless_find<T: AsRef<str>>(haystack: &[T], needle: &str) -> bool {
    for item in haystack {
        if (*item)
            .as_ref()
            .to_lowercase()
            .starts_with(&needle.to_lowercase())
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_caseless_find() {
        assert!(caseless_find(&["hOsT: one"], "Host:"));
        assert!(!caseless_find(&["hOsTo: one"], "Host:"));
        assert!(caseless_find(&["User-AGENT: one"], "User-Agent:"));
    }

    #[test]
    fn test_redirect() -> Result<()> {
        let mut req = ProducerRequest::new(
            "https://www.google.com/",
            RequestMethod::Get,
            vec!["User-Agent: test_redirect".to_string()],
            None,
            RequestConfig::default(),
        )?;
        let req_str = req.get_request();
        let req_str = std::string::String::from_utf8(req_str).unwrap();
        assert!(
            req_str.contains("Host: www.google.com"),
            "Could not find the appropriate Host header at: {}",
            req_str
        );
        req.redirect("https:///www.yahoo.com")?;
        let req_str = req.get_request();
        let req_str = std::string::String::from_utf8(req_str).unwrap();
        assert!(
            req_str.contains("Host: www.yahoo.com"),
            "Could not find the appropriate Host header at: {}",
            req_str
        );

        Ok(())
    }
}
