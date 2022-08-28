use anyhow::{anyhow, ensure, Context, Result};
use futures::prelude::*;
use futures::AsyncRead;
use log::*;
use std::cmp::min;
use std::collections::HashMap;
use std::str::from_utf8;

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: HashMap<String, String>,
}
impl Response {
    pub fn is_redirect(&self) -> bool {
        self.status >= 300 && self.status <= 400
    }
}
#[derive(Debug)]
pub struct Parser {
    pub bytes: Vec<u8>,
    pub read_idx: usize,
    pub write_idx: usize,
}

impl Parser {
    pub fn new(size: usize) -> Self {
        Parser {
            bytes: vec![0; size],
            read_idx: 0,
            write_idx: 0,
        }
    }
    pub fn reset(&mut self) {
        self.read_idx = 0;
        self.write_idx = 0;
    }

    fn ensure_space(&mut self, space: usize) {
        while (self.write_idx + space) > self.bytes.len() {
            self.bytes.resize(self.bytes.len() * 2, Default::default());
        }
    }
    fn unparsed(&self) -> &[u8] {
        return &self.bytes[self.read_idx..self.write_idx];
    }
    fn sample(&self, bytes_to_sample: usize) -> &str {
        let upper = std::cmp::min(bytes_to_sample, self.unparsed().len());
        return std::str::from_utf8(&self.unparsed()[0..upper]).unwrap_or("NON-UTF8 DATA SAMPLE");
    }
    fn consume(&mut self, amount: usize) {
        self.read_idx += amount;
    }
    async fn read_exact<T: AsyncRead + Unpin>(
        &mut self,
        stream: &mut T,
        amount: usize,
    ) -> Result<usize> {
        self.ensure_space(amount);
        stream
            .read_exact(&mut self.bytes[self.write_idx..self.write_idx + amount])
            .await?;
        self.write_idx += amount;
        Ok(amount)
    }

    async fn read<T: AsyncRead + Unpin>(&mut self, stream: &mut T) -> Result<usize> {
        let amount = stream.read(&mut self.bytes[self.write_idx..]).await?;
        if amount == 0 {
            std::io::Result::Err(std::io::ErrorKind::UnexpectedEof.into())?;
        }
        self.write_idx += amount;
        Ok(amount)
    }

    pub async fn read_header<T: AsyncRead + Unpin>(&mut self, stream: &mut T) -> Result<Response> {
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut response = httparse::Response::new(&mut headers);
        let mut parsed;

        let mut parse_errors = 0;
        loop {
            trace!("Read header loop");
            self.read(stream).await.context(format!(
                "Reading bytes to complete a Response Header: {:?}...",
                self.sample(self.unparsed().len())
            ))?;

            let res = response
                .parse(self.unparsed())
                .context(format!("Parsing header: {:?}", self.sample(50)));
            if res.is_err() && parse_errors < 2 {
                trace!(
                    "Failed attempt to read header with {} bytes",
                    self.unparsed().len()
                );
                headers = [httparse::EMPTY_HEADER; 32];
                response = httparse::Response::new(&mut headers);
                parse_errors += 1;
                continue;
            }
            parsed = res?;
            if parsed.is_partial() {
                self.bytes.resize(self.bytes.len() * 2, 0);
                headers = [httparse::EMPTY_HEADER; 32];
                response = httparse::Response::new(&mut headers);
            } else {
                break;
            }
        }
        let response = Response {
            status: response.code.unwrap(),
            headers: headers
                .iter()
                .filter(|h| **h != httparse::EMPTY_HEADER)
                .map(|h| (h.name.to_owned(), from_utf8(h.value).unwrap().to_owned()))
                .collect(),
        };
        self.consume(parsed.unwrap());
        Ok(response)
    }

    pub async fn drop_body<T: AsyncRead + Unpin>(
        &mut self,
        stream: &mut T,
        response: &Response,
    ) -> Result<()> {
        let headers = &response.headers;
        if let Some(content_length) = headers.iter().find(|(hname, _)| *hname == "Content-Length") {
            let content_length: usize = content_length.1.parse()?;
            trace!(
                "Read: {}, parsed: {}, content length: {}",
                self.write_idx,
                self.read_idx,
                content_length
            );
            let response_size = content_length + self.read_idx;
            let mut left_to_read = response_size - min(self.write_idx, response_size);
            while left_to_read > 0 {
                let to_read = std::cmp::min(left_to_read, self.bytes.len());
                trace!("Discarding {} bytes", to_read);
                self.read_exact(stream, to_read)
                    .await
                    .context("Discarding bytes according to Content-Length")?;
                left_to_read -= to_read;
                self.consume(to_read);
            }
        } else if let Some(transfer_encoding) = headers
            .iter()
            .find(|(hname, _)| *hname == "Transfer-Encoding")
        {
            trace!("Transfer encoding");
            let transfer_encoding = transfer_encoding.1;
            assert_eq!(transfer_encoding, "chunked");

            loop {
                let chunk_size = match self.unparsed().iter().position(|x| x == &b'\n') {
                    None => {
                        if self.unparsed().len() < 16 {
                            self.read(stream).await?;
                            continue;
                        } else {
                            return Err(anyhow!(
                                "No chunk header found on {} bytes",
                                self.unparsed().len()
                            ));
                        }
                    }
                    Some(position) => {
                        ensure!(
                            position <= 16,
                            "Missed the header chunk: {}",
                            self.sample(20)
                        );
                        ensure!(self.unparsed()[position - 1] == b'\r', "Missing \\r");
                        trace!("Found \\n in position {} of {}", position, self.sample(10));
                        let chunk_size_str =
                            std::str::from_utf8(&self.unparsed()[0..(position - 1)]).unwrap();
                        let chunk_size = usize::from_str_radix(chunk_size_str, 16)
                            .context("Chunk size not in hex")?;
                        trace!("Chunk size: [{}] => {}", chunk_size_str, chunk_size);
                        self.consume(position + 1);
                        chunk_size
                    }
                };

                if chunk_size == 0 {
                    trace!("Last chunk found");
                    self.consume(2);
                    break;
                }

                if chunk_size + 2 <= self.unparsed().len() {
                    trace!("Full chunk in buffer, consuming..");
                    self.consume(chunk_size + 2);
                    continue;
                }

                let left_to_read = chunk_size + 2 - self.unparsed().len();
                self.consume(self.unparsed().len());
                trace!(
                    "Reading {} to end current chunk: {}",
                    left_to_read,
                    left_to_read + self.unparsed().len()
                );
                self.read_exact(stream, left_to_read).await?;
                self.consume(left_to_read)
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use galvanic_test::test_suite;
    use simplelog::*;
    use std::cmp::min;
    use std::io::Read;
    use std::pin::Pin;
    use std::task::Poll;

    struct AsyncBuffer {
        bytes: Vec<u8>,
        read_ptr: usize,
    }
    impl AsyncBuffer {
        fn new(vec: Vec<u8>) -> Self {
            AsyncBuffer {
                bytes: vec,
                read_ptr: 0,
            }
        }
    }

    impl AsyncRead for AsyncBuffer {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut futures::task::Context,
            buf: &mut [u8],
        ) -> Poll<Result<usize, futures::io::Error>> {
            let chunk_size = min(buf.len(), self.bytes.len() - self.read_ptr);
            buf[0..chunk_size]
                .copy_from_slice(&self.bytes[self.read_ptr..self.read_ptr + chunk_size]);
            self.read_ptr += chunk_size;
            return Poll::Ready(Ok(chunk_size));
        }
    }

    const RESPONSE: &[u8] = br#"
HTTP/1.1 200 OK
Content-Type: text/html
Content-Length: 47

<!DOCTYPE html><html><body>Hello!</body></html>"#;

    test_suite! {
            name parser;

            use super::*;

            fixture base() -> (){
                setup(&mut self) {
                    let config = ConfigBuilder::new().set_thread_level(LevelFilter::Info).build();
                    let _ = SimpleLogger::init(log::LevelFilter::Warn, config);
                }
            }

            test test_parse(base) {
                //let _ = SimpleLogger::init(log::LevelFilter::Trace, Config::default());
                let mut stream = AsyncBuffer::new(RESPONSE.to_vec());
                let mut parser = Parser::new(10_000);
                smol::block_on(async {
                    let response = parser.read_header(&mut stream).await?;
                    parser.drop_body(&mut stream, &response).await?;
                    Result::<()>::Ok(())
                }).unwrap();
            }

            test test_parse_chunked(base) {
                //let _ = SimpleLogger::init(log::LevelFilter::Trace, Config::default());
                let mut d: GzDecoder<&[u8]> = GzDecoder::new(include_bytes!("../resources/yahoo.chunked.gz"));
                let mut buffer: Vec<u8> = Vec::new();
                d.read_to_end(&mut buffer).unwrap();
                let mut stream = AsyncBuffer::new(buffer);

                smol::block_on(async {
                    let mut parser = Parser::new(10_000);
                    let response = parser.read_header(&mut stream).await?;
                    parser.drop_body(&mut stream, &response).await?;
                    Result::<()>::Ok(())
                }).unwrap();

            }
    }
}
