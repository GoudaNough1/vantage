// Author: Riley @ MHK Consultants
// Date: 2026-06-25
// Purpose: Vantage host - rung 4 remote input. Stream H.264 to a viewer and inject the input it sends back.

use std::error::Error;
use std::thread;
use enigo::agent::{Agent, Token};
use enigo::{Enigo, Settings};
use scap::capturer::{Capturer, Options, Resolution};
use scap::frame::{Frame, FrameType};
use openh264::OpenH264API;
use openh264::Timestamp;
use openh264::encoder::{Encoder, EncoderConfig, FrameRate, UsageType};
use openh264::formats::{BgraSliceU8, YUVBuffer};
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

async fn serve_viewer(socket: TcpStream) {
	let (mut read_half, mut write_half) = socket.into_split();

	let (frame_tx, mut frame_rx) = mpsc::channel::<Message>(8);
	let capture = thread::spawn(move || {
		if let Err(e) = capture_encode_loop(&frame_tx) {
			eprintln!("vantage-host: capture/encode stopped: {e}");
		}
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
	reader.abort();
	let _ = reader.await;
	let _ = capture.join();
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

fn capture_encode_loop(tx: &mpsc::Sender<Message>) -> Result<(), Box<dyn Error>> {
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

	let config = EncoderConfig::new()
		.max_frame_rate(FrameRate::from_hz(TARGET_FPS as f32))
		.usage_type(UsageType::ScreenContentRealTime)
		.skip_frames(false);
	let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config)?;

	let mut announced = false;
	let mut index: u64 = 0;
	loop {
		let Frame::BGRA(captured) = capturer.get_next_frame()? else {
			return Err("expected a BGRA frame from the Windows capture engine".into());
		};
		let (width, height) = (captured.width as u32, captured.height as u32);

		if !announced {
			if tx.blocking_send(Message::Hello { width, height }).is_err() {
				break;
			}
			announced = true;
		}

		let source = BgraSliceU8::new(&captured.data, (width as usize, height as usize));
		let yuv = YUVBuffer::from_rgb_source(source);
		let timestamp = Timestamp::from_millis(index * 1000 / TARGET_FPS as u64);
		let encoded = encoder.encode_at(&yuv, timestamp)?.to_vec();
		if tx.blocking_send(Message::Frame(encoded)).is_err() {
			break;
		}
		index += 1;
	}

	capturer.stop_capture();
	Ok(())
}
