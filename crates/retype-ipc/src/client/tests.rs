#[test]
fn request_paths_are_resolved_in_the_client_process() {
	let relative = std::path::Path::new("relative").join("file.png");
	let expected = std::env::current_dir().unwrap().join(&relative);
	assert_eq!(super::local_path(&relative).unwrap(), expected);
}

#[cfg(unix)]
mod unix {
	use std::io::{BufReader, Read};
	use std::os::unix::net::UnixStream;
	use std::sync::mpsc;
	use std::time::{Duration, Instant};

	use super::super::{Client, ClientError};
	use crate::{Response, read_frame, write_frame};

	#[test]
	fn consecutive_requests_finish_and_dropping_closes_the_connection() {
		let (client, server) = UnixStream::pair().unwrap();
		server.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
		let (sender, receiver) = mpsc::channel();
		let server = std::thread::spawn(move || {
			let mut server = BufReader::new(server);
			for id in 1..=2 {
				let request = read_frame(&mut server).unwrap().unwrap();
				assert_eq!(request["id"], id);
				write_frame(server.get_mut(), &Response::ok(id, id).unwrap()).unwrap();
			}
			sender.send(read_frame(&mut server).unwrap().is_none()).unwrap();
		});

		let mut client = Client::from_stream(client);
		for id in 1..=2 {
			let answer = client
				.request_with_timeout("status", serde_json::Value::Null, Duration::from_secs(2))
				.unwrap();
			assert_eq!(answer, id);
		}
		drop(client);
		assert!(receiver.recv_timeout(Duration::from_secs(3)).unwrap());
		server.join().unwrap();
	}

	#[test]
	fn a_timed_out_read_closes_the_connection() {
		let (client, server) = UnixStream::pair().unwrap();
		server.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
		let (sender, receiver) = mpsc::channel();
		let server = std::thread::spawn(move || {
			let mut server = BufReader::new(server);
			assert!(read_frame(&mut server).unwrap().is_some());
			sender.send(read_frame(&mut server).unwrap().is_none()).unwrap();
		});

		let mut client = Client::from_stream(client);
		let timeout = Duration::from_millis(150);
		let started = Instant::now();
		let result = client.request_with_timeout("status", serde_json::Value::Null, timeout);
		assert!(matches!(result, Err(ClientError::Timeout(value)) if value == timeout));
		assert!(started.elapsed() < Duration::from_secs(3));
		assert!(matches!(
			client.request_with_timeout("status", serde_json::Value::Null, timeout),
			Err(ClientError::Timeout(_))
		));
		assert!(receiver.recv_timeout(Duration::from_secs(3)).unwrap());
		server.join().unwrap();
	}

	#[test]
	fn a_timed_out_write_closes_the_connection() {
		let (client, mut server) = UnixStream::pair().unwrap();
		server.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
		let mut client = Client::from_stream(client);
		let timeout = Duration::from_millis(150);
		let payload = "x".repeat(4 * 1024 * 1024);
		let started = Instant::now();
		let result = client.request_with_timeout("large", serde_json::json!({"data": payload}), timeout);
		assert!(matches!(result, Err(ClientError::Timeout(value)) if value == timeout));
		assert!(started.elapsed() < Duration::from_secs(3));
		let mut bytes = Vec::new();
		server.read_to_end(&mut bytes).unwrap();
		assert!(
			bytes.len() < 4 * 1024 * 1024,
			"the peer unexpectedly received the whole request"
		);
	}
}

#[cfg(windows)]
mod windows;
