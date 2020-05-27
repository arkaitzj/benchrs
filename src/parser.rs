use log::*;
use futures::AsyncRead;
use anyhow::{Result, Context};
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
    pub write_idx: usize,
    pub response: Option<Response>
}

impl ParseContext {
    fn new(size: usize) -> Self {
        return ParseContext {
            bytes: vec![0;size],
            read_idx: 0,
            write_idx: 0,
            response: None
        };
    }
}

pub async fn read_header<T: AsyncRead + Unpin>(stream: &mut T) -> Result<ParseContext> {

    let mut buffer = ParseContext::new(10_000);
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut response = httparse::Response::new(&mut headers);

    let mut read_amount = 0;
    let mut parsed;

    let mut parse_errors = 0;
    loop {
        read_amount += stream.read(&mut buffer.bytes[read_amount..]).await?;
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
    buffer.response = Some(Response{
        status: response.code.unwrap(),
        headers: headers.iter().filter(|h| **h !=httparse::EMPTY_HEADER).map(|h| (h.name.to_owned(), from_utf8(h.value).unwrap().to_owned())).collect()
    });
    buffer.read_idx = parsed.unwrap();
    buffer.write_idx = read_amount;
    return Ok(buffer);
}

pub async fn drop_body<T: AsyncRead + Unpin>(stream: &mut T, mut parse_context: ParseContext) -> Result<()> {
    let headers = parse_context.response.unwrap().headers;
    let mut buffer = parse_context.bytes;
    let read_amount = &mut parse_context.write_idx;
    let parsed_len  = &mut parse_context.read_idx;

    if let Some(content_length) = headers.iter().find(|(hname,_)| *hname == "Content-Length") {
        let content_length: usize = content_length.1.parse()?;
        trace!("Read: {}, parsed: {}, content length: {}", read_amount, parsed_len, content_length);
        let response_size = (content_length + *parsed_len);
        let mut left_to_read = response_size - min(*read_amount, response_size);
        while left_to_read > 0 {
            let to_read = std::cmp::min(left_to_read, buffer.len());
    //        info!("Discarding {} bytes", to_read);
            stream.read_exact(&mut buffer[0..to_read]).await?;
            left_to_read -= to_read;
        }
    } else if let Some(transfer_encoding) = headers.iter().find(|(hname,_)| *hname == "Transfer-Encoding") {
        trace!("Transfer encoding");
        let transfer_encoding = transfer_encoding.1;
        assert_eq!(transfer_encoding,"chunked");
        let body_read = *read_amount - *parsed_len;
        if body_read < 16 {
            debug!("Not enough body to read chunk header");
            stream.read_exact(&mut buffer[*read_amount..*read_amount+16]).await?;
        }
        let mut chunk_header_start = *parsed_len;
        loop {
            trace!("First bytes look like: [{:?}]", std::str::from_utf8(&buffer[chunk_header_start..chunk_header_start+4]).unwrap());
            let chunk_header_pos = &buffer[chunk_header_start..].iter().position(|x| x == &b'\r').unwrap();
            let chunk_header_end = chunk_header_start + chunk_header_pos;

            assert_eq!(&buffer[chunk_header_end], &b'\r');
            assert_eq!(&buffer[chunk_header_end+1], &b'\n');
            let chunk_size_str = std::str::from_utf8(&buffer[chunk_header_start..chunk_header_end]).unwrap().to_owned();
            let chunk_size = usize::from_str_radix(&chunk_size_str, 16).context("Chunk size not in hex")?;
            if chunk_size == 0 {
                trace!("Last chunk ingested");
                break;
            }

            let left_in_buffer = *read_amount - (chunk_header_end+1);
            trace!("New chunk size is {} we have {} left in buffer", chunk_size, left_in_buffer);
            let chunk_size = chunk_size + 3; // \r\n closes the chunk
            let left_to_read = chunk_size - left_in_buffer;
            trace!("{} left to read ", left_to_read);

            let to_read = left_to_read;
            if left_to_read > buffer.len() {
                buffer.resize(to_read + 1024, 0);
            }

            trace!("Reading: {}", to_read);
            stream.read_exact(&mut buffer[0..to_read]).await?;
            chunk_header_start = left_to_read;
            *read_amount = to_read;
            let extra_amount = stream.read(&mut buffer[to_read..]).await?;
            trace!("Extra read was: {}", extra_amount);
            *read_amount += extra_amount;
        }
    }

    return Ok(());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use smol::Task;
    use simplelog::*;
    use crate::producer::*;
    use std::pin::Pin;
    use std::task::Poll;
    use std::cmp::min;


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
	fn poll_read( mut self: Pin<&mut Self>, cx: &mut futures::task::Context, buf: &mut [u8])  -> Poll<Result<usize, futures::io::Error>> {
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

    #[test]
    fn test_parse() {
        let _ = SimpleLogger::init(log::LevelFilter::Trace, Config::default());
        let mut stream = AsyncBuffer::new(RESPONSE.to_vec());
        smol::run(async {
            let ctx = read_header(&mut stream).await?;
            drop_body(&mut stream, ctx).await?;
            Result::<()>::Ok(())
        });
    }
}

