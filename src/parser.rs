use log::*;
use futures::AsyncRead;
use anyhow::{Result, Context};
use futures::prelude::*;

// Reads fully the response from the stream
pub async fn read_response<T: AsyncRead + Unpin>( mut stream: T) -> Result<()>{
    let mut buffer = vec![0;10_000];
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut response = httparse::Response::new(&mut headers);

    let mut read_amount = 0;
    let mut parsed;

    let mut parse_errors = 0;
    loop {
        read_amount += stream.read(&mut buffer[read_amount..]).await?;
        let res = response.parse(&buffer).context("Parsing");
        if res.is_err() && parse_errors < 2 {
            headers = [httparse::EMPTY_HEADER; 32];
            response = httparse::Response::new(&mut headers);
            parse_errors += 1;
            continue;
        }
        parsed = res?;
        if parsed.is_partial() {
            buffer.resize(buffer.len()*2, 0);
            headers = [httparse::EMPTY_HEADER; 32];
            response = httparse::Response::new(&mut headers);
        } else {
            break;
        }
    }
    let parsed_len = parsed.unwrap();
    trace!("Response header: \n{}", std::str::from_utf8(&buffer[0..parsed_len]).unwrap());

    if let Some(content_length) = response.headers.iter().find(|he| he.name == "Content-Length") {
        let content_length: usize = std::str::from_utf8(content_length.value)?.parse()?;
        debug!("Read: {}, parsed: {}, content length: {}", read_amount, parsed_len, content_length);
        let mut left_to_read = (content_length + parsed_len) - read_amount;
        while left_to_read > 0 {
            let to_read = std::cmp::min(left_to_read, buffer.len());
    //        info!("Discarding {} bytes", to_read);
            stream.read_exact(&mut buffer[0..to_read]).await?;
            left_to_read -= to_read;
        }
    } else if let Some(transfer_encoding) = response.headers.iter().find(|he| he.name == "Transfer-Encoding") {
        debug!("Transfer encoding");
        let transfer_encoding = std::str::from_utf8(transfer_encoding.value)?;
        assert_eq!(transfer_encoding,"chunked");
        let body_read = read_amount - parsed_len;
        if body_read < 16 {
            debug!("Not enough body to read chunk header");
            stream.read_exact(&mut buffer[read_amount..read_amount+16]).await?;
        }
        let mut chunk_header_start = parsed_len;
        loop {
            debug!("First bytes look like: [{:?}]", std::str::from_utf8(&buffer[chunk_header_start..chunk_header_start+4]).unwrap());
            let chunk_header_pos = &buffer[chunk_header_start..].iter().position(|x| x == &b'\r').unwrap();
            let chunk_header_end = chunk_header_start + chunk_header_pos;

            assert_eq!(&buffer[chunk_header_end], &b'\r');
            assert_eq!(&buffer[chunk_header_end+1], &b'\n');
            let chunk_size_str = std::str::from_utf8(&buffer[chunk_header_start..chunk_header_end]).unwrap().to_owned();
            let chunk_size = usize::from_str_radix(&chunk_size_str, 16).context("Chunk size not in hex")?;
            if chunk_size == 0 {
                debug!("Last chunk ingested");
                break;
            }

            let left_in_buffer = read_amount - (chunk_header_end+1);
            debug!("New chunk size is {} we have {} left in buffer", chunk_size, left_in_buffer);
            let chunk_size = chunk_size + 3; // \r\n closes the chunk
            let left_to_read = chunk_size - left_in_buffer;
            debug!("{} left to read ", left_to_read);

            let to_read = left_to_read;
            if left_to_read > buffer.len() {
                buffer.resize(to_read + 1024, 0);
            }

            debug!("Reading: {}", to_read);
            stream.read_exact(&mut buffer[0..to_read]).await?;
            chunk_header_start = left_to_read;
            read_amount = to_read;
            let extra_amount = stream.read(&mut buffer[to_read..]).await?;
            debug!("Extra read was: {}", extra_amount);
            read_amount += extra_amount;
        }

    }
    Ok(())

}

