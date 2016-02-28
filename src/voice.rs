//! Voice communication module.

use super::{Result, Error};

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::net::UdpSocket;

use websocket::ws::sender::Sender as SenderTrait;
use websocket::client::{Client, Sender, Receiver};
use websocket::stream::WebSocketStream;
use websocket::message::{Message as WsMessage, Type as MessageType};

use serde_json;
use serde_json::builder::ObjectBuilder;

use byteorder::{LittleEndian, BigEndian, WriteBytesExt, ReadBytesExt};

use super::model::*;

/// A readable audio source.
///
/// Audio is expected to be in signed 16-bit little-endian PCM (`pcm_s16le`)
/// format, at 48000Hz.
pub type AudioSource = Box<Read + Send>;

/// A websocket connection to the voice servers.
///
/// A VoiceConnection may be active or inactive. Use `voice_connect` and
/// `voice_disconnect` on the `Connection` you are feeding it events from to
/// change what channel it is connected to.
pub struct VoiceConnection {
	user_id: UserId,
	session_id: Option<String>,
	sender: mpsc::Sender<Status>,
	receiver: Option<mpsc::Receiver<Status>>,
}

impl VoiceConnection {
	/// Prepare a VoiceConnection for later use.
	pub fn new(user_id: UserId) -> Self {
		let (tx, rx) = mpsc::channel();
		VoiceConnection {
			user_id: user_id,
			session_id: None,
			sender: tx,
			receiver: Some(rx),
		}
	}

	/// Play from the given audio source.
	pub fn play(&self, source: AudioSource) {
		let _ = self.sender.send(Status::Source(source));
	}

	/// Stop the currently playing audio source.
	pub fn stop(&self) {
		let _ = self.sender.send(Status::Stop);
	}

	/// Update the voice state based on an event.
	pub fn update(&mut self, event: &Event) {
		match *event {
			Event::VoiceStateUpdate(_, ref voice_state) => {
				if voice_state.user_id == self.user_id {
					self.session_id = Some(voice_state.session_id.clone());
					if !voice_state.channel_id.is_some() {
						// drop the previous connection
						self.disconnect();
					}
				}
			}
			Event::VoiceServerUpdate { ref server_id, ref endpoint, ref token } => {
				if let Some(endpoint) = endpoint.as_ref() {
					self.connect(server_id, endpoint.clone(), token).expect("Voice::connect failure")
				} else {
					self.disconnect()
				}
			}
			_ => {}
		}
	}

	/// Check whether the voice thread is currently running.
	pub fn is_running(&self) -> bool {
		match self.receiver {
			None => self.sender.send(Status::Poke).is_ok(),
			Some(_) => false,
		}
	}

	fn disconnect(&mut self) {
		let (tx, rx) = mpsc::channel();
		self.sender = tx;
		self.receiver = Some(rx);
	}

	fn connect(&mut self, server_id: &ServerId, mut endpoint: String, token: &str) -> Result<()> {
		// take any pending receiver, or build a new one if there isn't any
		let rx = match self.receiver.take() {
			Some(rx) => rx,
			None => {
				let (tx, rx) = mpsc::channel();
				self.sender = tx;
				rx
			}
		};

		// prepare the URL: drop the :80 and prepend wss://
		if endpoint.ends_with(":80") {
			let len = endpoint.len();
			endpoint.truncate(len - 3);
		}
		// establish the websocket connection
		let url = match ::websocket::client::request::Url::parse(&format!("wss://{}", endpoint)) {
			Ok(url) => url,
			Err(_) => return Err(Error::Other("Invalid URL in Voice::connect()"))
		};
		let response = try!(try!(Client::connect(url)).send());
		try!(response.validate());
		let (mut sender, receiver) = response.begin().split();

		// send the handshake
		let map = ObjectBuilder::new()
			.insert("op", 0)
			.insert_object("d", |object| object
				.insert("server_id", &server_id.0)
				.insert("user_id", &self.user_id.0)
				.insert("session_id", self.session_id.as_ref().expect("no session id"))
				.insert("token", token)
			)
			.unwrap();
		try!(sender.send_message(&WsMessage::text(try!(serde_json::to_string(&map)))));

		// spin up the voice thread, where most of the action will take place
		try!(::std::thread::Builder::new()
			.name("Discord Voice Thread".into())
			.spawn(move || voice_thread(endpoint, sender, receiver, rx).unwrap()));
		Ok(())
	}
}

/// Use `ffmpeg` to open an audio file as a PCM stream.
///
/// Requires `ffmpeg` to be on the path and executable.
pub fn open_ffmpeg_stream<P: AsRef<::std::ffi::OsStr>>(path: P) -> Result<AudioSource> {
	use std::process::{Command, Stdio};
	let child = try!(Command::new("ffmpeg")
		.arg("-i").arg(path)
		.args(&[
			"-f", "s16le",
			"-ac", "1",
			"-ar", "48000",
			"-acodec", "pcm_s16le",
			"-"])
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::null())
		.spawn());
	Ok(Box::new(ProcessStream(child)))
}

/// Use `youtube-dl` and `ffmpeg` to stream from an internet source.
///
/// Requires both `youtube-dl` and `ffmpeg` to be on the path and executable.
/// On Windows, this means the `.exe` version of `youtube-dl` must be used.
pub fn open_ytdl_stream(url: &str) -> Result<AudioSource> {
	use std::process::{Command, Stdio};
	let output = try!(Command::new("youtube-dl")
		.args(&[
			"-f", "webm[abr>0]/bestaudio/best",
			"--no-playlist", "--print-json",
			"--skip-download",
			url])
		.stdin(Stdio::null())
		.output());
	if !output.status.success() {
		return Err(Error::Other("youtube-dl failed"))
	}

	let json: serde_json::Value = try!(serde_json::from_reader(&output.stdout[..]));
	let map = match json.as_object() {
		Some(map) => map,
		None => return Err(Error::Other("youtube-dl output could not be read"))
	};
	let url = match map.get("url").and_then(serde_json::Value::as_string) {
		Some(url) => url,
		None => return Err(Error::Other("youtube-dl output's \"url\" could not be read"))
	};
	open_ffmpeg_stream(url)
}

/// A stream that reads from a child's stdout and kills it on drop.
struct ProcessStream(::std::process::Child);

impl Read for ProcessStream {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		self.0.stdout.as_mut().expect("missing stdout").read(buf)
	}
}

impl Drop for ProcessStream {
	fn drop(&mut self) {
		// If we can't kill it, it's dead already or out of our hands
		let _ = self.0.kill();
	}
}

enum Status {
	Source(AudioSource),
	Stop,
	Poke,
}

fn recv_message(receiver: &mut Receiver<WebSocketStream>) -> Result<VoiceEvent> {
	use websocket::ws::receiver::Receiver;
	let message: WsMessage = try!(receiver.recv_message());
	if message.opcode != MessageType::Text {
		return Err(Error::Protocol("Voice websocket message was not Text"))
	}
	let json: serde_json::Value = try!(serde_json::from_reader(&message.payload[..]));
	let original = format!("{:?}", json);
	VoiceEvent::decode(json).map_err(|err| {
		// If there was a decode failure, print the original json for debugging
		warn!("Error vdecoding: {}", original);
		err
	})
}

fn voice_thread(
	endpoint: String,
	mut sender: Sender<WebSocketStream>,
	mut receiver: Receiver<WebSocketStream>,
	channel: mpsc::Receiver<Status>,
) -> Result<()> {
	use opus;
	use sodiumoxide::crypto::secretbox as crypto;
	use std::io::Cursor;

	// read the first websocket message
	let (interval, port, ssrc, modes) = match try!(recv_message(&mut receiver)) {
		VoiceEvent::Handshake { heartbeat_interval, port, ssrc, modes } => (heartbeat_interval, port, ssrc, modes),
		_ => return Err(Error::Protocol("First voice event was not Handshake"))
	};
	if !modes.iter().find(|&s| s == "xsalsa20_poly1305").is_some() {
		return Err(Error::Protocol("Voice mode \"xsalsa20_poly1305\" unavailable"))
	}

	// bind a UDP socket and send the ssrc value in a packet as identification
	let udp = try!(UdpSocket::bind("0.0.0.0:0"));
	let mut bytes = [0; 4];
	try!(Cursor::new(&mut bytes[..]).write_u32::<BigEndian>(ssrc));
	let destination = {
		use std::net::ToSocketAddrs;
		try!(try!((&endpoint[..], port).to_socket_addrs())
			.next()
			.ok_or(Error::Other("Failed to resolve voice hostname")))
	};
	try!(udp.send_to(&bytes, destination));

	// receive the response to the identification to get port and address info
	let mut bytes = [0; 256];
	let (len, _remote_addr) = try!(udp.recv_from(&mut bytes));
	let mut cursor = Cursor::new(&bytes[..len]);
	let _ = try!(cursor.read_u32::<LittleEndian>()); // discard padding
	let port_number = try!(cursor.read_u16::<LittleEndian>());

	// send the acknowledgement websocket message
	let map = ObjectBuilder::new()
		.insert("op", 1)
		.insert_object("d", |object| object
			.insert("protocol", "udp")
			.insert_object("data", |object| object
				.insert("address", "")
				.insert("port", port_number)
				.insert("mode", "xsalsa20_poly1305")
			)
		)
		.unwrap();
	try!(sender.send_message(&WsMessage::text(try!(serde_json::to_string(&map)))));

	// discard websocket messages until we get the Ready
	let encryption_key;
	loop {
		match try!(recv_message(&mut receiver)) {
			VoiceEvent::Ready { mode, secret_key } => {
				encryption_key = crypto::Key::from_slice(&secret_key).expect("failed to create key");
				if mode != "xsalsa20_poly1305" {
					return Err(Error::Protocol("Voice mode in Ready was not \"xsalsa20_poly1305\""))
				}
				break
			}
			VoiceEvent::Unknown(op, value) => debug!("Unknown message type: {}/{:?}", op, value),
			_ => {},
		}
	}

	// start a drain thread for the websocket receiver - without this, eventually
	// the OS buffer will fill and the connection will be dropped
	try!(::std::thread::Builder::new()
		.name("Discord Voice Drain Thread".into())
		.spawn(move || drain_thread(receiver)));

	// prepare buffers for later use
	let mut opus = try!(opus::Encoder::new(48000, opus::Channels::Mono, opus::CodingMode::Audio));
	let mut audio_buffer = [0i16; 960];
	let mut packet = Vec::with_capacity(256);
	let mut sequence = 0;
	let mut timestamp = 0;
	let mut speaking = false;

	let mut audio = None;

	let audio_duration = ::time::Duration::milliseconds(20);
	let keepalive_duration = ::time::Duration::milliseconds(interval as i64);
	let mut audio_timer = ::Timer::new(audio_duration);
	let mut keepalive_timer = ::Timer::new(keepalive_duration);

	let mut nonce = crypto::Nonce([0; 24]);

	// start the main loop
	info!("Voice connected to {}", endpoint);
	'outer: loop {
		::sleep_ms(3);

		loop {
			match channel.try_recv() {
				Ok(Status::Source(source)) => audio = Some(source),
				Ok(Status::Stop) => audio = None,
				Ok(Status::Poke) => {},
				Err(mpsc::TryRecvError::Empty) => break,
				Err(mpsc::TryRecvError::Disconnected) => break 'outer,
			}
		}

		if keepalive_timer.check_and_add(keepalive_duration) {
			let map = ObjectBuilder::new()
				.insert("op", 3)
				.insert("d", serde_json::Value::Null)
				.unwrap();
			let json = try!(serde_json::to_string(&map));
			try!(sender.send_message(&WsMessage::text(json)));
		}

		if audio_timer.check_and_add(audio_duration) {
			// read the audio from the source
			let len = match audio.as_mut() {
				Some(source) => try!(next_frame(source, &mut audio_buffer[..])),
				None => 0
			};
			if len == 0 {
				// stop speaking, don't send any audio
				try!(set_speaking(&mut sender, &mut speaking, false));
				continue
			} else if len < audio_buffer.len() {
				// zero-fill the rest of the buffer
				for value in &mut audio_buffer[len..] {
					*value = 0;
				}
			}
			try!(set_speaking(&mut sender, &mut speaking, true));

			// prepare the packet header
			const HEADER_LEN: usize = 12;
			packet.clear();
			try!(packet.write_all(&[0x80, 0x78]));
			try!(packet.write_u16::<BigEndian>(sequence));
			try!(packet.write_u32::<BigEndian>(timestamp));
			try!(packet.write_u32::<BigEndian>(ssrc));
			nonce.0[..12].clone_from_slice(&packet[..12]);

			// encode the audio data and transmit it
			let mut new_opus_buf = [0; 256];
			let len = opus.encode(&audio_buffer, &mut new_opus_buf).expect("failed encode");
			packet.extend(crypto::seal(&new_opus_buf[..len], &nonce, &encryption_key));
			try!(udp.send_to(&packet[..], destination));

			sequence = sequence.wrapping_add(1);
			timestamp = timestamp.wrapping_add(960);
		}
	}

	// shutting down the sender like this will also terminate the drain thread
	try!(sender.get_mut().shutdown(::std::net::Shutdown::Both));
	info!("Voice disconnected");
	Ok(())
}

fn next_frame(source: &mut AudioSource, buffer: &mut [i16]) -> Result<usize> {
	for (i, val) in buffer.iter_mut().enumerate() {
		*val = match source.read_i16::<LittleEndian>() {
			Ok(val) => val,
			Err(::byteorder::Error::UnexpectedEOF) => return Ok(i),
			Err(::byteorder::Error::Io(e)) => return Err(From::from(e))
		};
	}
	Ok(buffer.len())
}

fn set_speaking(sender: &mut Sender<WebSocketStream>, store: &mut bool, speaking: bool) -> Result<()> {
	if *store == speaking { return Ok(()) }
	*store = speaking;
	trace!("Speaking: {}", speaking);
	let map = ObjectBuilder::new()
		.insert("op", 5)
		.insert_object("d", |object| object
			.insert("speaking", speaking)
			.insert("delay", 0)
		)
		.unwrap();
	sender.send_message(&WsMessage::text(try!(serde_json::to_string(&map)))).map_err(From::from)
}

fn drain_thread(mut receiver: Receiver<WebSocketStream>) -> Receiver<WebSocketStream> {
	while let Ok(_) = recv_message(&mut receiver) {}
	receiver
}
