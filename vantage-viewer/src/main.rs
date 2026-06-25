// Author: Riley @ MHK Consultants
// Date: 2026-06-25
// Purpose: Vantage viewer - rung 4 remote input. Render the host stream and forward local mouse and keyboard.

use std::error::Error;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::thread;
use enigo::agent::Token;
use enigo::{Axis, Button, Coordinate, Direction, Key as EnigoKey};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use openh264::nal_units;
use softbuffer::{Context, Surface};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedReadHalf;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WinitKey, NamedKey};
use winit::window::{Window, WindowId};
use vantage_core::{Message, read_message, write_message};

const DEFAULT_HOST: &str = "127.0.0.1:9000";
const MAX_WINDOW_WIDTH: u32 = 1600;

// === Cross-thread events ===

enum ViewerEvent {
	Connected { width: u32, height: u32 },
	Frame(DecodedFrame),
	Disconnected(String),
}

struct DecodedFrame {
	width: usize,
	height: usize,
	rgb: Vec<u8>,
}

type ViewerSurface = Surface<Arc<Window>, Arc<Window>>;

// === Window application ===

struct App {
	host_addr: String,
	proxy: EventLoopProxy<ViewerEvent>,
	input_tx: UnboundedSender<Token>,
	input_rx: Option<UnboundedReceiver<Token>>,
	host_size: Option<(u32, u32)>,
	window: Option<Arc<Window>>,
	context: Option<Context<Arc<Window>>>,
	surface: Option<ViewerSurface>,
	frame: Option<DecodedFrame>,
	net_started: bool,
}

impl App {
	fn new(host_addr: String, proxy: EventLoopProxy<ViewerEvent>) -> Self {
		let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel();
		Self {
			host_addr,
			proxy,
			input_tx,
			input_rx: Some(input_rx),
			host_size: None,
			window: None,
			context: None,
			surface: None,
			frame: None,
			net_started: false,
		}
	}

	fn send_input(&self, token: Token) {
		let _ = self.input_tx.send(token);
	}

	fn map_cursor(&self, position: PhysicalPosition<f64>) -> Option<Token> {
		let (host_w, host_h) = self.host_size?;
		let size = self.window.as_ref()?.inner_size();
		if size.width == 0 || size.height == 0 {
			return None;
		}
		let x = (position.x * host_w as f64 / size.width as f64).round() as i32;
		let y = (position.y * host_h as f64 / size.height as f64).round() as i32;
		Some(Token::MoveMouse(x.clamp(0, host_w as i32 - 1), y.clamp(0, host_h as i32 - 1), Coordinate::Abs))
	}

	fn render(&mut self) -> Result<(), softbuffer::SoftBufferError> {
		let (Some(window), Some(surface), Some(frame)) = (&self.window, &mut self.surface, &self.frame) else {
			return Ok(());
		};
		let size = window.inner_size();
		let (Some(dst_w), Some(dst_h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
			return Ok(());
		};
		surface.resize(dst_w, dst_h)?;
		let mut buffer = surface.buffer_mut()?;

		let (frame_w, frame_h) = (frame.width, frame.height);
		let (dst_w, dst_h) = (size.width as usize, size.height as usize);
		for y in 0..dst_h {
			let src_row = (y * frame_h / dst_h) * frame_w;
			let dst_row = y * dst_w;
			for x in 0..dst_w {
				let src = (src_row + x * frame_w / dst_w) * 3;
				buffer[dst_row + x] =
					(frame.rgb[src] as u32) << 16 | (frame.rgb[src + 1] as u32) << 8 | frame.rgb[src + 2] as u32;
			}
		}

		window.pre_present_notify();
		buffer.present()
	}
}

impl ApplicationHandler<ViewerEvent> for App {
	fn resumed(&mut self, event_loop: &ActiveEventLoop) {
		if self.window.is_some() {
			return;
		}

		let attributes = Window::default_attributes()
			.with_title("Vantage Viewer")
			.with_inner_size(LogicalSize::new(1280.0, 720.0));
		let window = match event_loop.create_window(attributes) {
			Ok(window) => Arc::new(window),
			Err(e) => {
				eprintln!("vantage-viewer: failed to create window: {e}");
				event_loop.exit();
				return;
			}
		};

		let context = match Context::new(window.clone()) {
			Ok(context) => context,
			Err(e) => {
				eprintln!("vantage-viewer: softbuffer context failed: {e}");
				event_loop.exit();
				return;
			}
		};
		let surface = match Surface::new(&context, window.clone()) {
			Ok(surface) => surface,
			Err(e) => {
				eprintln!("vantage-viewer: softbuffer surface failed: {e}");
				event_loop.exit();
				return;
			}
		};

		self.window = Some(window);
		self.context = Some(context);
		self.surface = Some(surface);

		if !self.net_started {
			if let Some(input_rx) = self.input_rx.take() {
				self.net_started = true;
				start_network(self.host_addr.clone(), self.proxy.clone(), input_rx);
			}
		}
	}

	fn user_event(&mut self, event_loop: &ActiveEventLoop, event: ViewerEvent) {
		match event {
			ViewerEvent::Connected { width, height } => {
				println!("vantage-viewer: connected, host is {width}x{height}");
				self.host_size = Some((width, height));
				if let Some(window) = &self.window {
					let _ = window.request_inner_size(fit_window(width, height));
				}
			}
			ViewerEvent::Frame(frame) => {
				self.frame = Some(frame);
				if let Some(window) = &self.window {
					window.request_redraw();
				}
			}
			ViewerEvent::Disconnected(reason) => {
				eprintln!("vantage-viewer: disconnected: {reason}");
				event_loop.exit();
			}
		}
	}

	fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
		match event {
			WindowEvent::CloseRequested => event_loop.exit(),
			WindowEvent::Resized(_) => {
				if let Some(window) = &self.window {
					window.request_redraw();
				}
			}
			WindowEvent::RedrawRequested => {
				if let Err(e) = self.render() {
					eprintln!("vantage-viewer: render failed: {e}");
				}
			}
			WindowEvent::CursorMoved { position, .. } => {
				if let Some(token) = self.map_cursor(position) {
					self.send_input(token);
				}
			}
			WindowEvent::MouseInput { state, button, .. } if self.host_size.is_some() => {
				if let Some(token) = map_button(state, button) {
					self.send_input(token);
				}
			}
			WindowEvent::MouseWheel { delta, .. } if self.host_size.is_some() => {
				if let Some(token) = map_scroll(delta) {
					self.send_input(token);
				}
			}
			WindowEvent::KeyboardInput { event, .. } if self.host_size.is_some() => {
				if let Some(token) = map_key(&event) {
					self.send_input(token);
				}
			}
			_ => {}
		}
	}
}

fn fit_window(width: u32, height: u32) -> LogicalSize<f64> {
	if width <= MAX_WINDOW_WIDTH {
		return LogicalSize::new(width as f64, height as f64);
	}
	let scale = MAX_WINDOW_WIDTH as f64 / width as f64;
	LogicalSize::new(MAX_WINDOW_WIDTH as f64, (height as f64 * scale).round())
}

// === Input mapping (winit -> enigo tokens) ===

fn map_button(state: ElementState, button: MouseButton) -> Option<Token> {
	let button = match button {
		MouseButton::Left => Button::Left,
		MouseButton::Right => Button::Right,
		MouseButton::Middle => Button::Middle,
		MouseButton::Back => Button::Back,
		MouseButton::Forward => Button::Forward,
		_ => return None,
	};
	let direction = match state {
		ElementState::Pressed => Direction::Press,
		ElementState::Released => Direction::Release,
	};
	Some(Token::Button(button, direction))
}

fn map_scroll(delta: MouseScrollDelta) -> Option<Token> {
	let (horizontal, vertical) = match delta {
		MouseScrollDelta::LineDelta(x, y) => (x.round() as i32, y.round() as i32),
		MouseScrollDelta::PixelDelta(p) => ((p.x / 120.0).round() as i32, (p.y / 120.0).round() as i32),
	};
	if vertical != 0 {
		Some(Token::Scroll(-vertical, Axis::Vertical))
	} else if horizontal != 0 {
		Some(Token::Scroll(horizontal, Axis::Horizontal))
	} else {
		None
	}
}

fn map_key(event: &KeyEvent) -> Option<Token> {
	if event.state != ElementState::Pressed {
		return None;
	}
	match &event.logical_key {
		WinitKey::Named(named) => {
			let key = match named {
				NamedKey::Enter => EnigoKey::Return,
				NamedKey::Backspace => EnigoKey::Backspace,
				NamedKey::Tab => EnigoKey::Tab,
				NamedKey::Escape => EnigoKey::Escape,
				NamedKey::Space => EnigoKey::Space,
				NamedKey::Delete => EnigoKey::Delete,
				NamedKey::ArrowUp => EnigoKey::UpArrow,
				NamedKey::ArrowDown => EnigoKey::DownArrow,
				NamedKey::ArrowLeft => EnigoKey::LeftArrow,
				NamedKey::ArrowRight => EnigoKey::RightArrow,
				NamedKey::Home => EnigoKey::Home,
				NamedKey::End => EnigoKey::End,
				NamedKey::PageUp => EnigoKey::PageUp,
				NamedKey::PageDown => EnigoKey::PageDown,
				_ => return None,
			};
			Some(Token::Key(key, Direction::Click))
		}
		WinitKey::Character(text) => Some(Token::Text(text.to_string())),
		_ => None,
	}
}

// === Network and decode ===

fn start_network(host_addr: String, proxy: EventLoopProxy<ViewerEvent>, input_rx: UnboundedReceiver<Token>) {
	thread::spawn(move || {
		let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
			Ok(runtime) => runtime,
			Err(e) => {
				let _ = proxy.send_event(ViewerEvent::Disconnected(format!("runtime init failed: {e}")));
				return;
			}
		};
		runtime.block_on(async {
			if let Err(e) = network_loop(&host_addr, &proxy, input_rx).await {
				let _ = proxy.send_event(ViewerEvent::Disconnected(e.to_string()));
			}
		});
	});
}

async fn network_loop(
	host_addr: &str,
	proxy: &EventLoopProxy<ViewerEvent>,
	mut input_rx: UnboundedReceiver<Token>,
) -> Result<(), Box<dyn Error>> {
	let stream = TcpStream::connect(host_addr).await?;
	let (mut read_half, mut write_half) = stream.into_split();

	let writer = tokio::spawn(async move {
		while let Some(token) = input_rx.recv().await {
			if write_message(&mut write_half, &Message::Input(token)).await.is_err() {
				break;
			}
		}
	});

	let mut decoder = Decoder::new()?;
	let result = read_frames(&mut read_half, proxy, &mut decoder).await;
	writer.abort();
	result
}

async fn read_frames(
	read_half: &mut OwnedReadHalf,
	proxy: &EventLoopProxy<ViewerEvent>,
	decoder: &mut Decoder,
) -> Result<(), Box<dyn Error>> {
	loop {
		match read_message(read_half).await? {
			Message::Hello { width, height } => {
				if proxy.send_event(ViewerEvent::Connected { width, height }).is_err() {
					return Ok(());
				}
			}
			Message::Frame(data) => {
				for nal in nal_units(&data) {
					if let Some(yuv) = decoder.decode(nal)? {
						let (width, height) = yuv.dimensions();
						let mut rgb = vec![0u8; width * height * 3];
						yuv.write_rgb8(&mut rgb);
						if proxy.send_event(ViewerEvent::Frame(DecodedFrame { width, height, rgb })).is_err() {
							return Ok(());
						}
					}
				}
			}
			Message::Input(_) => {}
		}
	}
}

fn main() -> Result<(), Box<dyn Error>> {
	let host_addr = std::env::args().nth(1).unwrap_or_else(|| DEFAULT_HOST.to_string());
	println!("vantage-viewer: connecting to {host_addr}");

	let event_loop = EventLoop::<ViewerEvent>::with_user_event().build()?;
	event_loop.set_control_flow(ControlFlow::Wait);
	let proxy = event_loop.create_proxy();
	let mut app = App::new(host_addr, proxy);
	event_loop.run_app(&mut app)?;
	Ok(())
}
