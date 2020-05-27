use url::Url;
use anyhow::Context;


#[derive(Clone)]
pub struct ProducerRequest {
    config: RequestConfig,
    addr: String,
    headers: Vec<String>
}

#[derive(Clone)]
pub struct RequestConfig {
    pub keepalive: bool,
    pub useragent: String
}
impl Default for RequestConfig {
    fn default() -> Self {
        RequestConfig {
            keepalive: false,
            useragent: format!("BenchRs/{}",env!("CARGO_PKG_VERSION"))
        }
    }
}

impl ProducerRequest {
    pub fn new(addr: &str, user_headers: Vec<String>, config: RequestConfig) -> Self {
        ProducerRequest{
            addr: addr.to_string(),
            config,
            headers: user_headers
        }
    }
    pub fn redirect(&mut self, addr: &str) {
       self.addr = addr.to_owned();
       // Ensure we do not override Host header
       self.headers.retain(|h| !h.starts_with("Host:") );
    }
    pub fn get_request(&self) -> String {
        let url: Url = Url::parse(&self.addr).unwrap();
        let host = url.host().context("cannot parse host").unwrap().to_string();
        let path = url.path().to_string();
        let query = match url.query() {
            Some(q) => format!("?{}", q),
            None => String::new(),
        };

        let connection = if self.config.keepalive { "keep-alive" } else { "close" };

        let mut headers = String::new();
        if ! caseless_find(&self.headers, "Host:")   { headers.push_str(&format!("Host: {}\r\n", host)); }
        if ! caseless_find(&self.headers, "Accept:") { headers.push_str(&format!("Accept: {}\r\n", "*/*")); }
        if ! caseless_find(&self.headers, "Connection:") { headers.push_str(&format!("Connection: {}\r\n", connection)); }
        if ! caseless_find(&self.headers, "User-Agent:") { headers.push_str(&format!("User-Agent: {}\r\n", self.config.useragent)); }
        self.headers.iter().for_each(|header| headers.push_str(&format!("{}\r\n",header)));

         // Construct a request.
        format!("GET {}{} HTTP/1.1\r\n{}\r\n",
            path, query, headers)
    }

}

fn caseless_find<T: AsRef<str>>(haystack: &[T], needle: &str) -> bool {
    for item in haystack {
        if (*item).as_ref().to_lowercase().starts_with(&needle.to_lowercase()) {
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
    fn test_redirect() {
        let mut req = ProducerRequest::new("https://www.google.com/", vec!["User-Agent: test_redirect".to_string()], RequestConfig::default());
        let req_str = req.get_request();
        assert!(req_str.contains("Host: www.google.com"), "Could not find the appropriate Host header at: {}", req_str);
        req.redirect("https:///www.yahoo.com");
        let req_str = req.get_request();
        assert!(req_str.contains("Host: www.yahoo.com"), "Could not find the appropriate Host header at: {}", req_str);
    }
}
