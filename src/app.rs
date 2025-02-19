use clap::{Parser, Subcommand};
use core_affinity::*;
use io_uring::{cqueue, opcode, squeue, types, IoUring};
use minstant::Instant;
use nix::libc::iovec;
use nix::sys::epoll::*;
use std::ffi::c_void;
use std::{
	io::{ErrorKind::WouldBlock, Read, Write},
	net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream},
	os::fd::AsRawFd,
	thread,
	time::Duration,
};
const PORT: u16 = 4242;
const CLIENTS: usize = 100;
const RING_SZ: usize = 1024 * 32;
const DURATION_SECS: u64 = 10;
const MSG_SZ: usize = 8;

fn client_loop(mut stream: TcpStream, runtime: Duration) -> Vec<u64> {
	let start = Instant::now();
	let mut bytes = vec![0u8; MSG_SZ];
	bytes[..8].copy_from_slice(&1u64.to_be_bytes());
	let mut latencies = Vec::new();
	while start.elapsed() < runtime {
		let send = start.elapsed().as_nanos() as u64;
		if stream.write_all(&bytes).is_err() {
			break;
		}
		if stream.read_exact(&mut bytes).is_err() {
			break;
		}
		let recv = start.elapsed().as_nanos() as u64;
		let dur = recv - send;
		latencies.push(dur);
		let now = Instant::now();
		while now.elapsed().as_micros() < 10000 {
			thread::yield_now()
		}
	}
	latencies
}

fn client() {
	let mut handles = Vec::new();
	for _ in 0..CLIENTS {
		let ip = "127.0.0.1".parse().unwrap();
		let addr = SocketAddrV4::new(ip, PORT);
		let stream = TcpStream::connect(&addr).unwrap();
		let dur = Duration::from_secs(DURATION_SECS);
		let h = thread::spawn(move || client_loop(stream, dur));
		handles.push(h);
	}

	let mut results: Vec<Vec<u64>> = Vec::new();
	for h in handles {
		let mut r = h.join().unwrap();
		r.sort();
		results.push(r);
	}

	let mut sent: Vec<_> = results.iter().map(|x| x.len()).collect();
	sent.sort();
	let median = sent[(sent.len() as f64 * 0.5).floor() as usize];
	let min = sent[0];
	let max = sent[sent.len() - 1];
	println!("==============");
	println!("Client Results");
	println!("==============");

	println!("Sent stats .. min: {min} p50: {median} max: {max}");

	for q in &[0.5f64, 0.9, 0.99, 0.999] {
		let mut tile_vec = Vec::new();
		for client in results.iter() {
			let l = client.len();
			let v = client[(l as f64 * q).floor() as usize];
			tile_vec.push(v);
		}

		tile_vec.sort();
		let l = tile_vec.len();
		let v = tile_vec[(l as f64 * 0.5).floor() as usize];
		println!("{}th %tile: {} us", q * 100., v / 1000);
	}
}

fn simple_server() {
	let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT);
	let listener = TcpListener::bind(&addr).unwrap();
	println!("Server is listening on {}", addr);

	let handler = |mut stream: TcpStream| {
		let mut bytes = [0u8; MSG_SZ];
		loop {
			if stream.read_exact(&mut bytes).is_err() {
				return;
			}
			if stream.write_all(&bytes).is_err() {
				return;
			}
		}
	};

	for stream in listener.incoming() {
		match stream {
			Ok(stream) => {
				thread::spawn(move || handler(stream));
			}
			Err(e) => {
				eprintln!("Error: {}", e);
			}
		}
	}
}

fn epoll_server() {
	let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT);
	let listener = TcpListener::bind(&addr).unwrap();

	let mut streams = Vec::new();
	for _ in 0..CLIENTS {
		let (stream, _) = listener.accept().unwrap();
		streams.push(stream);
	}

	let epoll = Epoll::new(EpollCreateFlags::empty()).unwrap();

	let mut states = Vec::new();
	for i in 0..streams.len() {
		let event = EpollEvent::new(EpollFlags::EPOLLIN, i as u64);
		epoll.add(&streams[i], event).unwrap();
		states.push(0);
	}

	let mut bytes = vec![0u8; MSG_SZ];
	let mut events = vec![EpollEvent::empty(); 10];
	let mut disconnected = 0;
	'outer: loop {
		let event_count = epoll.wait(&mut events, EpollTimeout::NONE).unwrap();
		for i in 0..event_count {
			let event = events[i];
			events[i] = EpollEvent::empty();
			let stream_idx = event.data();
			let stream = &mut streams[stream_idx as usize];
			let state = &mut states[stream_idx as usize];

			/*
			 * Ready to read
			 */
			if *state == 0 {
				if stream.read_exact(&mut bytes).is_err() {
					*state = 2;
					disconnected += 1;
					if disconnected == CLIENTS {
						break 'outer;
					}
				}
				let mut event =
					EpollEvent::new(EpollFlags::EPOLLOUT, stream_idx);
				epoll.modify(stream, &mut event).unwrap();
				*state = 1;
			}
			/*
			 * Ready to write
			 */
			else if *state == 1 {
				if stream.write_all(&bytes).is_err() {
					*state = 2;
					disconnected += 1;
					if disconnected == CLIENTS {
						break 'outer;
					}
				}
				let mut event =
					EpollEvent::new(EpollFlags::EPOLLIN, stream_idx);
				epoll.modify(stream, &mut event).unwrap();
				*state = 0;
			}

			/*
			 * We should not be getting an error for a disconnected
			 * client
			 */
			if *state == 2 {
				panic!("This should not happen");
			}
		}
	}
}

fn iouring_server() {
	let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT);
	let listener = TcpListener::bind(&addr).unwrap();

	let mut streams = Vec::new();
	for _ in 0..CLIENTS {
		let (stream, _) = listener.accept().unwrap();
		streams.push(stream);
	}

	let mut buffers = vec![[0u8; MSG_SZ]; CLIENTS as usize];
	let mut builder = IoUring::builder();
	builder.setup_sqpoll(1000);

	let mut ring: IoUring<squeue::Entry, cqueue::Entry> =
		builder.build(RING_SZ as u32).unwrap();

	let iovecs: Vec<_> = buffers
		.iter_mut()
		.map(|buf| iovec {
			iov_base: buf.as_mut_ptr() as *mut c_void,
			iov_len: buf.len(),
		})
		.collect();

	unsafe {
		ring.submitter()
			.register_buffers(iovecs.as_slice())
			.unwrap();
	}

	let mut sqes = Vec::new();
	let mut cqes = Vec::new();
	let msg_sz = MSG_SZ as u32;

	let mut stream_fds = Vec::new();
	for i in 0..streams.len() {
		stream_fds.push(types::Fd(streams[i].as_raw_fd()));
		let ptr = buffers[i].as_mut_ptr();
		let fd = stream_fds[i];
		let entry = opcode::Read::new(fd, ptr, msg_sz)
			.build()
			.user_data(i as u64);
		sqes.push(entry);
	}

	unsafe {
		ring.submission().push_multiple(&sqes).unwrap();
	}
	ring.submit().unwrap();

	let mut disconnected = 0;
	'outer: loop {
		sqes.clear();

		for cqe in ring.completion() {
			cqes.push(cqe);
		}
		ring.submission().sync();
		ring.completion().sync();

		'inner: for cqe in cqes.drain(..) {
			let udata = cqe.user_data();
			let res = cqe.result();
			if res < 0 {
				disconnected += 1;
				if disconnected == CLIENTS {
					break 'outer;
				}
				continue 'inner;
			}

			/*
			 * We received a request.
			 * 1. Do work
			 * 2. Write to the corresponding send buffer
			 * 3. Enqueue both send and receive. We can enqueue both
			 *    because the read will only happen after then write
			 *    succeeds
			 */
			if udata < streams.len() as u64 {
				let idx = udata as usize;
				let fd = stream_fds[idx];

				let ptr = buffers[idx].as_mut_ptr();
				let entry = opcode::ReadFixed::new(fd, ptr, msg_sz, idx as u16)
					.build()
					.user_data(udata as u64);
				'push_loop: loop {
					if unsafe { ring.submission().push(&entry) }.is_ok() {
						break 'push_loop;
					}
				}

				let tx_udata = udata + streams.len() as u64;
				let ptr = buffers[idx].as_mut_ptr();
				let entry =
					opcode::WriteFixed::new(fd, ptr, msg_sz, idx as u16)
						.build()
						.user_data(tx_udata);
				'push_loop: loop {
					if unsafe { ring.submission().push(&entry) }.is_ok() {
						break 'push_loop;
					}
				}
				ring.submit().unwrap();
			}
		}
	}
}

fn round_robin_server() {
	let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT);
	let listener = TcpListener::bind(&addr).unwrap();

	let mut streams = Vec::new();
	let mut states = Vec::new();
	for _ in 0..CLIENTS {
		let (stream, _) = listener.accept().unwrap();
		stream.set_nonblocking(true).unwrap();
		streams.push(stream);
		states.push(State::R);
	}

	let mut buffers = vec![[0u8; MSG_SZ]; CLIENTS as usize];

	enum State {
		R,
		W,
		D,
	}

	let try_read = |stream: &mut TcpStream, buf: &mut [u8]| -> State {
		match stream.read_exact(buf) {
			Ok(_) => State::W,
			Err(x) => {
				if x.kind() == WouldBlock {
					State::R
				} else {
					State::D
				}
			}
		}
	};

	let try_write = |stream: &mut TcpStream, buf: &[u8]| -> State {
		match stream.write_all(buf) {
			Ok(_) => State::R,
			Err(x) => {
				if x.kind() == WouldBlock {
					State::W
				} else {
					State::D
				}
			}
		}
	};

	let mut disconnected = 0;
	'outer: loop {
		for idx in 0..streams.len() {
			let stream = &mut streams[idx];
			let state = &mut states[idx];
			let buffer = &mut buffers[idx];

			match state {
				State::R => {
					let s = try_read(stream, buffer);
					*state = s;
				}
				State::W => {
					let s = try_write(stream, buffer);
					*state = s;
				}
				State::D => {
					disconnected += 1;
					if disconnected == CLIENTS {
						break 'outer;
					}
				}
			}
		}
	}
}

#[derive(Subcommand, Debug)]
enum Commands {
	SimpleServer,
	RrServer,
	IouringServer,
	EpollServer,
	Client,
}

#[derive(Parser, Debug)]
#[command(version, about, long_about=None)]
pub struct Args {
	#[command(subcommand)]
	cmd: Commands,
}

fn main() {
	let core_id = CoreId { id: 1usize };
	let args = Args::parse();

	match args.cmd {
		Commands::Client => client(),
		Commands::SimpleServer => {
			core_affinity::set_for_current(core_id);
			simple_server()
		}
		Commands::IouringServer => {
			core_affinity::set_for_current(core_id);
			iouring_server()
		}
		Commands::EpollServer => {
			core_affinity::set_for_current(core_id);
			epoll_server()
		}
		Commands::RrServer => {
			core_affinity::set_for_current(core_id);
			round_robin_server()
		}
	}
}
