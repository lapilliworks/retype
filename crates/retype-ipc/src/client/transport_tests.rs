use std::io::{self, BufReader, Cursor, Read, Write};
use std::sync::atomic::AtomicBool;

use super::transact;
use crate::{Request, Response, write_frame};

struct OrderedStream {
	request: Vec<u8>,
	response: Cursor<Vec<u8>>,
}

impl Read for OrderedStream {
	fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
		if !self.request.ends_with(b"\n") {
			return Err(io::Error::other("a read began before the request was written"));
		}
		self.response.read(buffer)
	}
}

impl Write for OrderedStream {
	fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
		self.request.extend_from_slice(bytes);
		Ok(bytes.len())
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

#[test]
fn a_request_is_written_before_its_response_is_read() {
	let mut response = Vec::new();
	write_frame(&mut response, &Response::ok(1, "ready").unwrap()).unwrap();
	let mut stream = BufReader::new(OrderedStream {
		request: Vec::new(),
		response: Cursor::new(response),
	});
	let frame = transact(&mut stream, &Request::new(1, "status"), &AtomicBool::new(false))
		.unwrap()
		.unwrap();
	assert_eq!(frame["result"], "ready");
}
