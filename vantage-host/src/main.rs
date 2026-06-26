// Author: Riley @ MHK Consultants
// Date: 2026-06-26
// Purpose: Vantage host - rung 6 latency pass. Latest-frame-wins capture/encode, skip unchanged, low-latency H.264.

use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::Instant;
use enigo::agent::{Agent, Token};
use enigo::{Enigo, Settings};
use scap::capturer::{Capturer, Options, Resolution};
use scap::frame::{Frame, FrameType};
use openh264::OpenH264API;
use openh264::Timestamp;
use openh264::encoder::{Complexity, Encoder, EncoderConfig, FrameRate, UsageType};
use openh264::formats::{RgbSliceU8, YUVBuffer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use windows_sys::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use vantage_core::{Message, read_message, write_message};

const LISTEN_ADDR: &str = "0.0.0.0:9000";
const WARMUP_FRAMES: usize = 5;
const TARGET_FPS: u32 = 30;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
	make_dpi_aware();

	let listener = TcpListener::bind(LISTEN_ADDR).await?;
	println!("vantage-host: listening on {LISTEN_ADDR}, waiting for a viewer...");

	loop {
		let (socket, peer) = listener.accept().await?;
		println!("vantage-host: viewer connected from {peer}");
		serve_viewer(socket).await;
		println!("vantage-host: viewer gone, waiting for the next one...");
	}
}

fn make_dpi_aware() {
	// scap captures physical pixels, but enigo maps absolute mouse moves against GetSystemMetrics,
	// which reports virtualized (scaled) dimensions to a DPI-unaware process. Per-monitor awareness
	// makes GetSystemMetrics return physical pixels so the two coordinate spaces agree.
	unsafe {
		SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
	}
}

// === Capture to encode handoff ===

struct CapturedFrame {
	width: u32,
	height: u32,
	data: Vec<u8>,
}

// Single slot: capture overwrites with the newest frame, encode takes the latest. Stale frames are
// dropped here, before encode, so the encoder always works on the freshest capture and never falls behind.
struct Handoff {
	slot: Mutex<Option<CapturedFrame>>,
	ready: Condvar,
	running: AtomicBool,
}

impl Handoff {
	fn new() -> Self {
		Self {
			slot: Mutex::new(None),
			ready: Condvar::new(),
			running: AtomicBool::new(true),
		}
	}

	fn running(&self) -> bool {
		self.running.load(Ordering::Relaxed)
	}

	fn shutdown(&self) {
		self.running.store(false, Ordering::Relaxed);
		self.ready.notify_all();
	}

	fn put(&self, frame: CapturedFrame) {
		*self.slot.lock().unwrap() = Some(frame);
		self.ready.notify_one();
	}

	fn take(&self) -> Option<CapturedFrame> {
		let mut slot = self.slot.lock().unwrap();
		loop {
			if let Some(frame) = slot.take() {
				return Some(frame);
			}
			if !self.running() {
				return None;
			}
			slot = self.ready.wait(slot).unwrap();
		}
	}
}

async fn serve_viewer(socket: TcpStream) {
	let (mut read_half, mut write_half) = socket.into_split();

	let (frame_tx, mut frame_rx) = mpsc::channel::<Message>(2);
	let handoff = Arc::new(Handoff::new());

	let capture_handoff = handoff.clone();
	let capture = thread::spawn(move || {
		if let Err(e) = capture_loop(&capture_handoff) {
			eprintln!("vantage-host: capture stopped: {e}");
		}
		capture_handoff.shutdown();
	});

	let encode_handoff = handoff.clone();
	let encode = thread::spawn(move || {
		if let Err(e) = encode_loop(&encode_handoff, &frame_tx) {
			eprintln!("vantage-host: encode stopped: {e}");
		}
		encode_handoff.shutdown();
	});

	let (input_tx, input_rx) = std::sync::mpsc::channel::<Token>();
	let injector = thread::spawn(move || inject_loop(&input_rx));

	let reader = tokio::spawn(async move {
		loop {
			match read_message(&mut read_half).await {
				Ok(Message::Input(token)) => {
					if input_tx.send(token).is_err() {
						break;
					}
				}
				Ok(_) => {}
				Err(_) => break,
			}
		}
	});

	while let Some(message) = frame_rx.recv().await {
		if write_message(&mut write_half, &message).await.is_err() {
			break;
		}
	}

	drop(frame_rx);
	handoff.shutdown();
	reader.abort();
	let _ = reader.await;
	let _ = capture.join();
	let _ = encode.join();
	let _ = injector.join();
}

fn inject_loop(input_rx: &std::sync::mpsc::Receiver<Token>) {
	let mut enigo = match Enigo::new(&Settings::default()) {
		Ok(enigo) => enigo,
		Err(e) => {
			eprintln!("vantage-host: enigo init failed: {e}");
			return;
		}
	};
	while let Ok(token) = input_rx.recv() {
		if let Err(e) = enigo.execute(&token) {
			eprintln!("vantage-host: input injection failed: {e}");
		}
	}
}

fn capture_loop(handoff: &Handoff) -> Result<(), Box<dyn Error>> {
	if !scap::is_supported() {
		return Err("screen capture unsupported on this system (needs Windows 10 1903+ for Graphics Capture)".into());
	}

	let options = Options {
		fps: TARGET_FPS,
		show_cursor: true,
		show_highlight: false,
		output_type: FrameType::BGRAFrame,
		output_resolution: Resolution::Captured,
		..Default::default()
	};
	let mut capturer = Capturer::build(options)?;
	capturer.start_capture();

	for _ in 0..WARMUP_FRAMES {
		capturer.get_next_frame()?;
	}

	while handoff.running() {
		let Frame::BGRA(captured) = capturer.get_next_frame()? else {
			return Err("expected a BGRA frame from the Windows capture engine".into());
		};
		handoff.put(CapturedFrame {
			width: captured.width as u32,
			height: captured.height as u32,
			data: captured.data,
		});
	}

	capturer.stop_capture();
	Ok(())
}

fn encode_loop(handoff: &Handoff, tx: &mpsc::Sender<Message>) -> Result<(), Box<dyn Error>> {
	let config = EncoderConfig::new()
		.max_frame_rate(FrameRate::from_hz(TARGET_FPS as f32))
		.usage_type(UsageType::ScreenContentRealTime)
		.complexity(Complexity::Low)
		.skip_frames(false);
	let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config)?;
	let start = Instant::now();

	let mut announced = false;
	let mut last_hash: Option<u64> = None;
	let mut rgb: Vec<u8> = Vec::new();
	while let Some(frame) = handoff.take() {
		if !announced {
			if tx.blocking_send(Message::Hello { width: frame.width, height: frame.height }).is_err() {
				break;
			}
			announced = true;
		}

		let hash = frame_hash(&frame.data);
		if Some(hash) == last_hash {
			continue;
		}
		last_hash = Some(hash);

		let (width, height) = (frame.width as usize, frame.height as usize);
		rgb.resize(width * height * 3, 0);
		bgra_to_rgb(&frame.data, &mut rgb);
		let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(&rgb, (width, height)));
		let timestamp = Timestamp::from_millis(start.elapsed().as_millis() as u64);
		let encoded = encoder.encode_at(&yuv, timestamp)?.to_vec();
		if tx.blocking_send(Message::Frame(encoded)).is_err() {
			break;
		}
	}

	Ok(())
}

// Repack scap's BGRA into the tightly packed RGB that openh264's from_rgb8_source expects: a chunked
// byte shuffle dropping alpha and swapping B/R, far cheaper than the per-pixel from_rgb_source path.
fn bgra_to_rgb(bgra: &[u8], rgb: &mut [u8]) {
	for (src, dst) in bgra.chunks_exact(4).zip(rgb.chunks_exact_mut(3)) {
		dst[0] = src[2];
		dst[1] = src[1];
		dst[2] = src[0];
	}
}

// Cheap change detector: FNV-1a over a sampled subset of the BGRA bytes. Catches typing and UI
// changes; may skip sub-8px deltas, which the next real change re-encodes against the last sent frame.
fn frame_hash(data: &[u8]) -> u64 {
	let mut hash: u64 = 0xcbf29ce484222325;
	let mut i = 0;
	while i < data.len() {
		hash = (hash ^ data[i] as u64).wrapping_mul(0x0000_0100_0000_01b3);
		i += 32;
	}
	hash
}
