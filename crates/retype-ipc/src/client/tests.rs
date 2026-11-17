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
	fn a_lost_submission_acknowledgement_retains_the_client_chosen_handle() {
		let (client, server) = UnixStream::pair().unwrap();
		let (send, receive) = mpsc::sync_channel(1);
		let server = std::thread::spawn(move || {
			let mut server = BufReader::new(server);
			let hello = read_frame(&mut server).unwrap().unwrap();
			write_frame(
				server.get_mut(),
				&Response::ok(
					hello["id"].as_u64().unwrap(),
					serde_json::json!({
						"ipc": crate::IPC_VERSION, "pid": 1, "machine": "test", "uptime_ms": 0,
						"features": ["jobs.submit.id"], "capabilities": {}
					}),
				)
				.unwrap(),
			)
			.unwrap();
			let submitted = read_frame(&mut server).unwrap().unwrap();
			send.send(submitted["params"]["job_id"].clone()).unwrap();
		});
		let mut client = Client::from_stream(client);
		client.hello("test").unwrap();
		let error = client
			.jobs_submit(&crate::JobSubmit {
				input: "input.png".into(),
				intent: "webp".into(),
				output: None,
				force: false,
				mode: crate::SubmitMode::Immediate,
			})
			.unwrap_err();
		let ClientError::Rpc(error) = error else {
			panic!("missing structured submission outcome");
		};
		assert_eq!(error.detail.unwrap()["id"], receive.recv().unwrap());
		assert!(error.message.contains("before retrying"));
		server.join().unwrap();
	}

	#[test]
	fn job_listing_reads_every_page_in_order() {
		let (client, server) = UnixStream::pair().unwrap();
		server.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
		let server = std::thread::spawn(move || {
			let mut server = BufReader::new(server);
			for offset in 0..2 {
				let request = read_frame(&mut server).unwrap().unwrap();
				assert_eq!(request["params"]["offset"], offset);
				let value = serde_json::json!({
					"jobs": [{"id": format!("{offset:026}"), "state": "done", "source": "a.png"}],
					"next_offset": if offset == 0 { Some(1) } else { None },
				});
				write_frame(server.get_mut(), &Response::ok(offset + 1, value).unwrap()).unwrap();
			}
		});
		let mut client = Client::from_stream(client);
		let jobs = client.jobs_list().unwrap();
		assert_eq!(jobs.jobs.len(), 2);
		assert!(jobs.jobs[0].id < jobs.jobs[1].id);
		assert!(jobs.next_offset.is_none());
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
		let payload = "x".repeat(crate::MAX_FRAME / 2);
		let started = Instant::now();
		let result = client.request_with_timeout("large", serde_json::json!({"data": payload}), timeout);
		assert!(matches!(result, Err(ClientError::Timeout(value)) if value == timeout));
		assert!(started.elapsed() < Duration::from_secs(3));
		let mut bytes = Vec::new();
		server.read_to_end(&mut bytes).unwrap();
		assert!(
			bytes.len() < crate::MAX_FRAME / 2,
			"the peer unexpectedly received the whole request"
		);
	}
}

#[cfg(windows)]
mod windows;
