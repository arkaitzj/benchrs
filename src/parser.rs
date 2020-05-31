use log::*;
use futures::AsyncRead;
use anyhow::{Result, Context, ensure};
use futures::prelude::*;
use std::collections::HashMap;
use std::str::from_utf8;
use std::cmp::min;


#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: HashMap<String,String>
}
impl Response {
    pub fn is_redirect(&self) -> bool {
        self.status >= 300 && self.status <= 400
    }
}
#[derive(Debug)]
pub struct ParseContext {
    pub bytes: Vec<u8>,
    pub read_idx: usize,
    pub write_idx: usize
}

impl ParseContext {
    pub fn new(size: usize) -> Self {
        ParseContext {
            bytes: vec![0;size],
            read_idx: 0,
            write_idx: 0
        }
    }
    pub fn reset(&mut self) {
        self.read_idx = 0;
        self.write_idx = 0;
    }
}

pub async fn read_header<T: AsyncRead + Unpin>(stream: &mut T, buffer: &mut ParseContext) -> Result<Response> {

    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut response = httparse::Response::new(&mut headers);

    let mut read_amount = 0;
    let mut parsed;

    let mut parse_errors = 0;
    loop {
        read_amount += stream.read(&mut buffer.bytes[read_amount..]).await.context("Reading bytes to complete a Response Header")?;
        if read_amount == 0 {
            return Err(anyhow::Error::msg("Connection closed"));
        }
        let res = response.parse(&buffer.bytes[0..read_amount]).context(format!("Parsing header: {:?}",from_utf8(&buffer.bytes[0..read_amount])));
        if res.is_err() && parse_errors < 2 {
            headers = [httparse::EMPTY_HEADER; 32];
            response = httparse::Response::new(&mut headers);
            parse_errors += 1;
            continue;
        }
        parsed = res?;
        if parsed.is_partial() {
            buffer.bytes.resize(buffer.bytes.len()*2, 0);
            headers = [httparse::EMPTY_HEADER; 32];
            response = httparse::Response::new(&mut headers);
        } else {
            break;
        }
    }
    let response = Response{
        status: response.code.unwrap(),
        headers: headers.iter().filter(|h| **h !=httparse::EMPTY_HEADER).map(|h| (h.name.to_owned(), from_utf8(h.value).unwrap().to_owned())).collect()
    };
    buffer.read_idx = parsed.unwrap();
    buffer.write_idx = read_amount;
    Ok(response)
}

pub async fn drop_body<T: AsyncRead + Unpin>(stream: &mut T, parse_context:&mut ParseContext, response: &Response) -> Result<()> {
    let headers = &response.headers;
    let mut buffer = &mut parse_context.bytes;
    let read_amount = &mut parse_context.write_idx;
    let parsed_len  = &mut parse_context.read_idx;

    if let Some(content_length) = headers.iter().find(|(hname,_)| *hname == "Content-Length") {
        let content_length: usize = content_length.1.parse()?;
        trace!("Read: {}, parsed: {}, content length: {}", read_amount, parsed_len, content_length);
        let response_size = content_length + *parsed_len;
        let mut left_to_read = response_size - min(*read_amount, response_size);
        while left_to_read > 0 {
            let to_read = std::cmp::min(left_to_read, buffer.len());
            trace!("Discarding {} bytes", to_read);
            stream.read_exact(&mut buffer[0..to_read]).await.context("Discarding bytes according to Content-Length")?;
            left_to_read -= to_read;
        }
    } else if let Some(transfer_encoding) = headers.iter().find(|(hname,_)| *hname == "Transfer-Encoding") {
        trace!("Transfer encoding");
        let transfer_encoding = transfer_encoding.1;
        assert_eq!(transfer_encoding,"chunked");
        let mut chunk_header_start = *parsed_len;
        loop {
            // We need at least 3 bytes to read last chunk
            if chunk_header_start + 3 > *read_amount {
                ensure_space(*read_amount + 1024, &mut buffer);
                *read_amount += stream.read(&mut buffer[*read_amount..]).await?;
                continue;
            }
            if buffer[chunk_header_start] == b'0' {
                trace!("Last chunk found");
                break;
            }

            trace!("First bytes look like: [{:?}]", std::str::from_utf8(&buffer[chunk_header_start..chunk_header_start+4]).unwrap());
            // We need to find the first '\r'
            let chunk_header_end = match &buffer[chunk_header_start..].iter().position(|x| x == &b'\r') {
                Some(position) => chunk_header_start + *position,
                None => {
                    if (*read_amount - chunk_header_start) > 16 {
                        panic!("More than 16 bytes read and no trace of '\r'");
                    } else {
                        ensure_space(*read_amount+1024, &mut buffer);
                        *read_amount += stream.read(&mut buffer[*read_amount..]).await?;
                        continue;
                    }
                }
            };

            trace!("Header bytes look like: [{:?}]", std::str::from_utf8(&buffer[chunk_header_start..chunk_header_end]).unwrap());

            if chunk_header_start >=1 {
                ensure!(buffer[chunk_header_start-2] == b'\r' && buffer[chunk_header_start-1] == b'\n',
                        "Chunk invariant pre does not hold {:?} != '\r\n'", &buffer[chunk_header_start-2..chunk_header_start]);
            }
            ensure!(buffer[chunk_header_end] == b'\r' && buffer[chunk_header_end+1] == b'\n',
                    "Chunk invariant post does not hold {:?} != '\r\n'", &buffer[chunk_header_start-2..chunk_header_start]);


            let chunk_size_str = std::str::from_utf8(&buffer[chunk_header_start..chunk_header_end]).unwrap().to_owned();
            let chunk_size = usize::from_str_radix(&chunk_size_str, 16).context("Chunk size not in hex")?;

            ensure!(*read_amount > (chunk_header_end+1), "Read {} and chunk ended at {}", *read_amount, chunk_header_end);
            let left_in_buffer = *read_amount - (chunk_header_end+1);
            trace!("New chunk size is {} we have {} left in buffer", chunk_size, left_in_buffer);
            let chunk_size = chunk_size + 2; // \r\n closes the chunk
            let left_to_read = chunk_size - min(chunk_size,left_in_buffer);
            trace!("{} left to read ", left_to_read);
            if left_to_read == 0 {
                let next_header = chunk_header_end+2+chunk_size;
                chunk_header_start = next_header;
                continue;
            }

            let to_read = left_to_read;
            if left_to_read > buffer.len() {
                buffer.resize(left_to_read, 0);
            }

            trace!("Reading: {}", to_read);
            stream.read_exact(&mut buffer[0..to_read]).await?;
            chunk_header_start = left_to_read+1; // 1 byte after the previous chunk ends
            *read_amount = to_read;
            let extra_amount = stream.read(&mut buffer[to_read..]).await?;
            trace!("Extra read was: {}", extra_amount);
            *read_amount += extra_amount;
        }
    }

    Ok(())
}

#[inline]
fn ensure_space<T: Default>(space: usize, buffer: &mut Vec<T>) {
    if space > buffer.len() {
        buffer.resize_with(space, Default::default);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use simplelog::*;
    use std::pin::Pin;
    use std::task::Poll;
    use std::cmp::min;
    use flate2::read::GzDecoder;
    use std::io::Read;
    use galvanic_test::test_suite;

    struct AsyncBuffer {
        bytes: Vec<u8>,
        read_ptr: usize
    }
    impl AsyncBuffer {
        fn new(vec: Vec<u8>) -> Self {
            AsyncBuffer{
                bytes: vec,
                read_ptr: 0
            }
        }

    }

    impl AsyncRead for AsyncBuffer {
	fn poll_read( mut self: Pin<&mut Self>, _cx: &mut futures::task::Context, buf: &mut [u8])  -> Poll<Result<usize, futures::io::Error>> {
	    let chunk_size = min(buf.len(), self.bytes.len()-self.read_ptr);
            buf[0..chunk_size].copy_from_slice(&self.bytes[self.read_ptr..self.read_ptr+chunk_size]);
            self.read_ptr += chunk_size;
            return Poll::Ready(Ok(chunk_size))
	}
    }

const RESPONSE: &[u8] = br#"
HTTP/1.1 200 OK
Content-Type: text/html
Content-Length: 47

<!DOCTYPE html><html><body>Hello!</body></html>"#;

    test_suite!{
        name parser;

        use super::*;

        fixture base() -> (){
            setup(&mut self) {
                let config = ConfigBuilder::new().set_thread_level(LevelFilter::Info).build();
                let _ = SimpleLogger::init(log::LevelFilter::Warn, config);
            }
        }

        test test_parse() {
            //let _ = SimpleLogger::init(log::LevelFilter::Trace, Config::default());
            let mut stream = AsyncBuffer::new(RESPONSE.to_vec());
            let mut ctx = ParseContext::new(10_000);
            smol::run(async {
                let response = read_header(&mut stream, &mut ctx).await?;
                drop_body(&mut stream, &mut ctx, &response).await?;
                Result::<()>::Ok(())
            }).unwrap();
        }

        test test_parse_chunked() {
            //let _ = SimpleLogger::init(log::LevelFilter::Trace, Config::default());
            let mut d: GzDecoder<&[u8]> = GzDecoder::new(include_bytes!("../resources/yahoo.chunked.gz"));
            let mut buffer: Vec<u8> = Vec::new();
            d.read_to_end(&mut buffer).unwrap();
            let mut stream = AsyncBuffer::new(buffer);

            smol::run(async {
                let mut ctx = ParseContext::new(10_000);
                let response = read_header(&mut stream, &mut ctx).await?;
                drop_body(&mut stream, &mut ctx, &response).await?;
                Result::<()>::Ok(())
            }).unwrap();

        }
}
}

