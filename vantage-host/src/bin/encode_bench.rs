use std::error::Error;
use std::time::{Duration, Instant};

use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use vantage_host::encoder::{BgraFrame, HardwareEncoder, SoftwareEncoder, VideoEncoder};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const FPS: u32 = 30;
const FRAME_COUNT: usize = 300;

struct BenchRun {
	path: &'static str,
	frames_encoded: usize,
	decode_verified: bool,
	durations: Vec<Duration>,
}

fn main() -> Result<(), Box<dyn Error>> {
	println!("path | frames encoded | decode-verified | avg ms | median ms | p95 ms | fps");
	match HardwareEncoder::new(WIDTH, HEIGHT, FPS) {
		Ok(encoder) => print_run(run_encoder("hardware", encoder)?),
		Err(error) => println!("hardware: unavailable ({error}) | 0 | n/a | n/a | n/a | n/a | n/a"),
	}
	print_run(run_encoder("software", SoftwareEncoder::new(FPS)?)?);
	Ok(())
}

fn run_encoder(
	path: &'static str,
	mut encoder: impl VideoEncoder,
) -> Result<BenchRun, Box<dyn Error>> {
	let mut bgra = vec![0; WIDTH as usize * HEIGHT as usize * 4];
	for pixel in bgra.chunks_exact_mut(4) {
		pixel.copy_from_slice(&[24, 18, 12, 255]);
	}
	let mut durations = Vec::with_capacity(FRAME_COUNT);
	let mut bitstream = Vec::new();
	let mut frames_encoded = 0;
	for frame_index in 0..FRAME_COUNT {
		draw_moving_frame(&mut bgra, frame_index);
		let encode_start = Instant::now();
		let encoded = encoder.encode(
			BgraFrame {
				width: WIDTH,
				height: HEIGHT,
				data: &bgra,
			},
			(frame_index as u64 * 1_000) / FPS as u64,
		)?;
		durations.push(encode_start.elapsed());
		if !encoded.is_empty() {
			frames_encoded += 1;
		}
		bitstream.extend(encoded);
	}
	let decode_verified = verify_decode(&bitstream)?;
	if !decode_verified {
		return Err(format!("{path} encoder output did not decode at {WIDTH}x{HEIGHT}").into());
	}
	Ok(BenchRun {
		path,
		frames_encoded,
		decode_verified,
		durations,
	})
}

fn draw_moving_frame(bgra: &mut [u8], frame_index: usize) {
	let box_size = 96;
	let x = frame_index * 11 % (WIDTH as usize - box_size);
	let y = frame_index * 7 % (HEIGHT as usize - box_size);
	let previous_x = frame_index.saturating_sub(1) * 11 % (WIDTH as usize - box_size);
	let previous_y = frame_index.saturating_sub(1) * 7 % (HEIGHT as usize - box_size);
	fill_rect(bgra, previous_x, previous_y, box_size, [24, 18, 12, 255]);
	fill_rect(
		bgra,
		x,
		y,
		box_size,
		[
			(frame_index as u8).wrapping_mul(17),
			(frame_index as u8).wrapping_mul(29),
			(frame_index as u8).wrapping_mul(43),
			255,
		],
	);
	bgra[0] = frame_index as u8;
}

fn fill_rect(bgra: &mut [u8], x: usize, y: usize, size: usize, color: [u8; 4]) {
	for row in y..y + size {
		for column in x..x + size {
			let pixel = (row * WIDTH as usize + column) * 4;
			bgra[pixel..pixel + 4].copy_from_slice(&color);
		}
	}
}

fn verify_decode(bitstream: &[u8]) -> Result<bool, Box<dyn Error>> {
	let mut decoder = Decoder::new()?;
	for unit in openh264::nal_units(bitstream) {
		if let Some(frame) = decoder.decode(unit)?
			&& frame.dimensions() == (WIDTH as usize, HEIGHT as usize)
		{
			return Ok(true);
		}
	}
	Ok(false)
}

fn print_run(mut run: BenchRun) {
	run.durations.sort_unstable();
	let total_ms: f64 = run.durations.iter().map(Duration::as_secs_f64).sum::<f64>() * 1_000.0;
	let average_ms = total_ms / run.durations.len() as f64;
	let median_ms = run.durations[run.durations.len() / 2].as_secs_f64() * 1_000.0;
	let p95_index = (run.durations.len() * 95).div_ceil(100) - 1;
	let p95_ms = run.durations[p95_index].as_secs_f64() * 1_000.0;
	let fps = FRAME_COUNT as f64 / (total_ms / 1_000.0);
	println!(
		"{} | {} | {} | {:.3} | {:.3} | {:.3} | {:.2}",
		run.path,
		run.frames_encoded,
		if run.decode_verified { "yes" } else { "no" },
		average_ms,
		median_ms,
		p95_ms,
		fps,
	);
}
