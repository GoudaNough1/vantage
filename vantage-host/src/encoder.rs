use std::error::Error;
use std::mem::ManuallyDrop;
use std::ptr::null_mut;

use openh264::encoder::{Complexity, Encoder, EncoderConfig, FrameRate, UsageType};
use openh264::formats::{RgbSliceU8, YUVBuffer};
use openh264::{OpenH264API, Timestamp};
use windows::Win32::Media::MediaFoundation::{
	eAVEncH264VProfile_Main, IMFActivate, IMFMediaEventGenerator, IMFMediaType, IMFSample,
	IMFTransform, METransformHaveOutput, METransformNeedInput, MF_E_NO_EVENTS_AVAILABLE,
	MF_E_TRANSFORM_NEED_MORE_INPUT, MF_EVENT_FLAG_NONE, MF_EVENT_FLAG_NO_WAIT, MF_LOW_LATENCY,
	MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
	MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_PIXEL_ASPECT_RATIO,
	MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
	MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video,
	MFSTARTUP_FULL, MFShutdown, MFStartup, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
	MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
	MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
	MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
	MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
	MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::{
	COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::core::Interface;

#[derive(Clone, Copy)]
pub struct BgraFrame<'a> {
	pub width: u32,
	pub height: u32,
	pub data: &'a [u8],
}

pub trait VideoEncoder {
	fn encode(
		&mut self,
		frame: BgraFrame<'_>,
		timestamp_ms: u64,
	) -> Result<Vec<u8>, Box<dyn Error>>;
}

pub struct SoftwareEncoder {
	encoder: Encoder,
	rgb: Vec<u8>,
}

impl SoftwareEncoder {
	pub fn new(fps: u32) -> Result<Self, Box<dyn Error>> {
		let config = EncoderConfig::new()
			.max_frame_rate(FrameRate::from_hz(fps as f32))
			.usage_type(UsageType::ScreenContentRealTime)
			.complexity(Complexity::Low)
			.skip_frames(false);
		let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)?;
		Ok(Self {
			encoder,
			rgb: Vec::new(),
		})
	}
}

impl VideoEncoder for SoftwareEncoder {
	fn encode(
		&mut self,
		frame: BgraFrame<'_>,
		timestamp_ms: u64,
	) -> Result<Vec<u8>, Box<dyn Error>> {
		validate_frame(frame)?;
		let dimensions = (frame.width as usize, frame.height as usize);
		self.rgb.resize(dimensions.0 * dimensions.1 * 3, 0);
		bgra_to_rgb(frame.data, &mut self.rgb);
		let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(&self.rgb, dimensions));
		let timestamp = Timestamp::from_millis(timestamp_ms);
		to_annex_b(&self.encoder.encode_at(&yuv, timestamp)?.to_vec())
	}
}

pub struct HardwareEncoder {
	transform: IMFTransform,
	activation: IMFActivate,
	events: Option<IMFMediaEventGenerator>,
	width: u32,
	height: u32,
	fps: u32,
	output_size: u32,
	parameter_sets: Vec<u8>,
	headers_sent: bool,
	need_input: u32,
	_session: MfSession,
}

struct MfSession;

impl Drop for MfSession {
	fn drop(&mut self) {
		unsafe {
			let _ = MFShutdown();
			CoUninitialize();
		}
	}
}

impl HardwareEncoder {
	pub fn new(width: u32, height: u32, fps: u32) -> Result<Self, Box<dyn Error>> {
		if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
			return Err("Media Foundation H.264 requires nonzero even dimensions".into());
		}

		if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).is_err() } {
			return Err("failed to initialize COM for Media Foundation".into());
		}
		if let Err(error) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
			unsafe { CoUninitialize() };
			return Err(error.into());
		}

		let activations = match enumerate_hardware_encoders() {
			Ok(activations) if !activations.is_empty() => activations,
			Ok(_) => {
				unsafe {
					let shutdown = MFShutdown();
					CoUninitialize();
					shutdown?;
				}
				return Err("no hardware H.264 Media Foundation encoder is available".into());
			}
			Err(error) => {
				unsafe {
					let _ = MFShutdown();
					CoUninitialize();
				}
				return Err(error);
			}
		};

		let mut last_error = None;
		for activation in activations {
			match unsafe { Self::activate(activation.clone(), width, height, fps) } {
				Ok(encoder) => return Ok(encoder),
				Err(error) => {
					let _ = unsafe { activation.ShutdownObject() };
					last_error = Some(error);
				}
			}
		}

		unsafe {
			let _ = MFShutdown();
			CoUninitialize();
		}
		Err(last_error.unwrap_or_else(|| "hardware H.264 encoder activation failed".into()))
	}

	unsafe fn activate(
		activation: IMFActivate,
		width: u32,
		height: u32,
		fps: u32,
	) -> Result<Self, Box<dyn Error>> {
		let transform: IMFTransform = unsafe { activation.ActivateObject()? };
		let attributes = unsafe { transform.GetAttributes()? };
		let is_async = unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0 };
		if is_async {
			unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)? };
		}
		unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1)? };

		let output_type = video_type(MFVideoFormat_H264, width, height, fps)?;
		unsafe {
			output_type.SetUINT32(&MF_MT_AVG_BITRATE, 8_000_000)?;
			output_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
			transform.SetOutputType(0, &output_type, 0)?;
		}
		let input_type = video_type(MFVideoFormat_NV12, width, height, fps)?;
		unsafe { transform.SetInputType(0, &input_type, 0)? };

		let stream_info = unsafe { transform.GetOutputStreamInfo(0)? };
		let events = if is_async {
			Some(transform.cast::<IMFMediaEventGenerator>()?)
		} else {
			None
		};
		let parameter_sets = media_type_parameter_sets(&output_type).unwrap_or_default();

		unsafe {
			transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
			transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
			transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
		}

		Ok(Self {
			transform,
			activation,
			events,
			width,
			height,
			fps,
			output_size: stream_info.cbSize.max(width * height),
			parameter_sets,
			headers_sent: false,
			need_input: 0,
			_session: MfSession,
		})
	}

	fn input_sample(&self, frame: BgraFrame<'_>, timestamp_ms: u64) -> Result<IMFSample, Box<dyn Error>> {
		let nv12_size = (self.width * self.height * 3 / 2) as usize;
		let buffer = unsafe { MFCreateMemoryBuffer(nv12_size as u32)? };
		let mut destination = null_mut();
		unsafe { buffer.Lock(&mut destination, None, None)? };
		let nv12 = unsafe { std::slice::from_raw_parts_mut(destination, nv12_size) };
		bgra_to_nv12(frame.data, self.width as usize, self.height as usize, nv12);
		unsafe {
			buffer.Unlock()?;
			buffer.SetCurrentLength(nv12_size as u32)?;
		}

		let sample = unsafe { MFCreateSample()? };
		unsafe {
			sample.AddBuffer(&buffer)?;
			sample.SetSampleTime((timestamp_ms * 10_000) as i64)?;
			sample.SetSampleDuration((10_000_000 / self.fps) as i64)?;
		}
		Ok(sample)
	}

	fn process_output(&mut self) -> Result<Vec<u8>, Box<dyn Error>> {
		let stream_info = unsafe { self.transform.GetOutputStreamInfo(0)? };
		let supplied_sample = if stream_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
			let sample = unsafe { MFCreateSample()? };
			let buffer = unsafe { MFCreateMemoryBuffer(self.output_size)? };
			unsafe { sample.AddBuffer(&buffer)? };
			Some(sample)
		} else {
			None
		};
		let mut output = MFT_OUTPUT_DATA_BUFFER {
			dwStreamID: 0,
			pSample: ManuallyDrop::new(supplied_sample),
			dwStatus: 0,
			pEvents: ManuallyDrop::new(None),
		};
		let mut status = 0;
		let process = unsafe { self.transform.ProcessOutput(0, std::slice::from_mut(&mut output), &mut status) };
		let sample = unsafe { ManuallyDrop::take(&mut output.pSample) };
		let events = unsafe { ManuallyDrop::take(&mut output.pEvents) };
		drop(events);
		if process.as_ref().is_err_and(|error| error.code() == windows::Win32::Media::MediaFoundation::MF_E_TRANSFORM_STREAM_CHANGE) {
			drop(sample);
			let output_type = unsafe { self.transform.GetOutputAvailableType(0, 0)? };
			unsafe { self.transform.SetOutputType(0, &output_type, 0)? };
			self.parameter_sets = media_type_parameter_sets(&output_type).unwrap_or_default();
			return Ok(Vec::new());
		}
		process?;
		let Some(sample) = sample else {
			return Ok(Vec::new());
		};
		let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
		let length = unsafe { buffer.GetCurrentLength()? } as usize;
		let mut source = null_mut();
		unsafe { buffer.Lock(&mut source, None, None)? };
		let encoded = unsafe { std::slice::from_raw_parts(source, length) }.to_vec();
		unsafe { buffer.Unlock()? };
		let annex_b = to_annex_b(&encoded)?;

		if self.parameter_sets.is_empty() {
			let output_type = unsafe { self.transform.GetOutputCurrentType(0)? };
			self.parameter_sets = media_type_parameter_sets(&output_type).unwrap_or_default();
		}
		// openh264 needs in-band SPS/PPS. If the first emitted frame lacks them, prepend the sequence
		// header from the output media type so a fresh decoder can start.
		if !self.headers_sent && !annex_b.is_empty() {
			self.headers_sent = true;
			if !contains_parameter_sets(&annex_b) && !self.parameter_sets.is_empty() {
				let mut with_headers = self.parameter_sets.clone();
				with_headers.extend_from_slice(&annex_b);
				return Ok(with_headers);
			}
		}
		Ok(annex_b)
	}

	fn handle_async_event(&mut self, no_wait: bool) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
		let flags = if no_wait { MF_EVENT_FLAG_NO_WAIT } else { MF_EVENT_FLAG_NONE };
		let events = self.events.as_ref().ok_or("asynchronous MFT has no event generator")?;
		let event = match unsafe { events.GetEvent(flags) } {
			Ok(event) => event,
			Err(error) if no_wait && error.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(None),
			Err(error) => return Err(error.into()),
		};
		let status = unsafe { event.GetStatus()? };
		status.ok()?;
		match unsafe { event.GetType()? } {
			event_type if event_type == METransformNeedInput.0 as u32 => {
				self.need_input += 1;
				Ok(Some(Vec::new()))
			}
			event_type if event_type == METransformHaveOutput.0 as u32 => {
				Ok(Some(self.process_output()?))
			}
			_ => Ok(Some(Vec::new())),
		}
	}

	fn encode_async(&mut self, sample: &IMFSample) -> Result<Vec<u8>, Box<dyn Error>> {
		// Async MFTs pull: never ProcessInput until the encoder signals METransformNeedInput. Block
		// for that credit (draining any output that arrives first), feed one sample, then drain the
		// rest without blocking.
		let mut encoded = Vec::new();
		while self.need_input == 0 {
			if let Some(output) = self.handle_async_event(false)? {
				encoded.extend(output);
			}
		}
		unsafe { self.transform.ProcessInput(0, sample, 0)? };
		self.need_input -= 1;
		while let Some(output) = self.handle_async_event(true)? {
			encoded.extend(output);
		}
		Ok(encoded)
	}

	fn encode_sync(&mut self, sample: &IMFSample) -> Result<Vec<u8>, Box<dyn Error>> {
		unsafe { self.transform.ProcessInput(0, sample, 0)? };
		let mut encoded = Vec::new();
		loop {
			match self.process_output() {
				Ok(output) => encoded.extend(output),
				Err(error) if error.downcast_ref::<windows::core::Error>()
					.is_some_and(|error| error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT) => break,
				Err(error) => return Err(error),
			}
		}
		Ok(encoded)
	}
}

impl VideoEncoder for HardwareEncoder {
	fn encode(
		&mut self,
		frame: BgraFrame<'_>,
		timestamp_ms: u64,
	) -> Result<Vec<u8>, Box<dyn Error>> {
		validate_frame(frame)?;
		if frame.width != self.width || frame.height != self.height {
			return Err("Media Foundation encoder dimensions changed during the stream".into());
		}
		let sample = self.input_sample(frame, timestamp_ms)?;
		if self.events.is_some() {
			self.encode_async(&sample)
		} else {
			self.encode_sync(&sample)
		}
	}
}

impl Drop for HardwareEncoder {
	fn drop(&mut self) {
		unsafe {
			let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
			let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
			let _ = self.activation.ShutdownObject();
		}
	}
}

pub fn create_encoder(
	width: u32,
	height: u32,
	fps: u32,
) -> Box<dyn VideoEncoder> {
	match HardwareEncoder::new(width, height, fps) {
		Ok(encoder) => {
			println!("vantage-host: active H.264 encoder: Media Foundation hardware");
			Box::new(encoder)
		}
		Err(error) => {
			eprintln!("vantage-host: hardware encoder unavailable ({error}), falling back to software");
			let encoder = SoftwareEncoder::new(fps)
				.unwrap_or_else(|error| panic!("vantage-host: software encoder initialization failed: {error}"));
			println!("vantage-host: active H.264 encoder: openh264 software");
			Box::new(encoder)
		}
	}
}

fn enumerate_hardware_encoders() -> Result<Vec<IMFActivate>, Box<dyn Error>> {
	let input = MFT_REGISTER_TYPE_INFO {
		guidMajorType: MFMediaType_Video,
		guidSubtype: MFVideoFormat_NV12,
	};
	let output = MFT_REGISTER_TYPE_INFO {
		guidMajorType: MFMediaType_Video,
		guidSubtype: MFVideoFormat_H264,
	};
	let mut activation_ptr: *mut Option<IMFActivate> = null_mut();
	let mut count = 0;
	unsafe {
		MFTEnumEx(
			MFT_CATEGORY_VIDEO_ENCODER,
			MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
			Some(&input),
			Some(&output),
			&mut activation_ptr,
			&mut count,
		)?;
	}
	let mut activations = Vec::with_capacity(count as usize);
	if !activation_ptr.is_null() {
		for activation in unsafe { std::slice::from_raw_parts_mut(activation_ptr, count as usize) } {
			if let Some(activation) = activation.take() {
				activations.push(activation);
			}
		}
		unsafe { CoTaskMemFree(Some(activation_ptr.cast())) };
	}
	Ok(activations)
}

fn video_type(
	subtype: windows::core::GUID,
	width: u32,
	height: u32,
	fps: u32,
) -> Result<IMFMediaType, Box<dyn Error>> {
	let media_type = unsafe { MFCreateMediaType()? };
	unsafe {
		media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
		media_type.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
		media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_ratio(width, height))?;
		media_type.SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(fps, 1))?;
		media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1))?;
		media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
	}
	Ok(media_type)
}

fn pack_ratio(numerator: u32, denominator: u32) -> u64 {
	((numerator as u64) << 32) | denominator as u64
}

fn validate_frame(frame: BgraFrame<'_>) -> Result<(), Box<dyn Error>> {
	let expected = frame.width as usize * frame.height as usize * 4;
	if frame.data.len() != expected {
		return Err(format!("BGRA frame has {} bytes, expected {expected}", frame.data.len()).into());
	}
	Ok(())
}

fn bgra_to_rgb(bgra: &[u8], rgb: &mut [u8]) {
	for (src, dst) in bgra.chunks_exact(4).zip(rgb.chunks_exact_mut(3)) {
		dst[0] = src[2];
		dst[1] = src[1];
		dst[2] = src[0];
	}
}

fn bgra_to_nv12(bgra: &[u8], width: usize, height: usize, nv12: &mut [u8]) {
	let (y_plane, uv_plane) = nv12.split_at_mut(width * height);
	for y in 0..height {
		for x in 0..width {
			let pixel = (y * width + x) * 4;
			let b = bgra[pixel] as i32;
			let g = bgra[pixel + 1] as i32;
			let r = bgra[pixel + 2] as i32;
			y_plane[y * width + x] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8;
		}
	}
	for y in (0..height).step_by(2) {
		for x in (0..width).step_by(2) {
			let mut r = 0;
			let mut g = 0;
			let mut b = 0;
			for row in 0..2 {
				for column in 0..2 {
					let pixel = ((y + row) * width + x + column) * 4;
					b += bgra[pixel] as i32;
					g += bgra[pixel + 1] as i32;
					r += bgra[pixel + 2] as i32;
				}
			}
			// r/g/b are 2x2 block sums, so >>10 folds the /4 averaging into the /256 coefficient
			// scale; +512 rounds to nearest.
			let uv = y / 2 * width + x;
			uv_plane[uv] = (((-38 * r - 74 * g + 112 * b + 512) >> 10) + 128).clamp(0, 255) as u8;
			uv_plane[uv + 1] = (((112 * r - 94 * g - 18 * b + 512) >> 10) + 128).clamp(0, 255) as u8;
		}
	}
}

fn media_type_parameter_sets(media_type: &IMFMediaType) -> Result<Vec<u8>, Box<dyn Error>> {
	let size = unsafe { media_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }?;
	let mut sequence_header = vec![0; size as usize];
	unsafe { media_type.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut sequence_header, None)? };
	to_annex_b(&sequence_header)
}

fn contains_parameter_sets(stream: &[u8]) -> bool {
	let mut sps = false;
	let mut pps = false;
	for unit in openh264::nal_units(stream) {
		let prefix = if unit.starts_with(&[0, 0, 0, 1]) { 4 } else { 3 };
		if unit.len() > prefix {
			match unit[prefix] & 0x1f {
				7 => sps = true,
				8 => pps = true,
				_ => {}
			}
		}
	}
	sps && pps
}

fn to_annex_b(stream: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
	if stream.is_empty() {
		return Ok(Vec::new());
	}
	if stream.windows(3).any(|prefix| prefix == [0, 0, 1]) {
		return Ok(stream.to_vec());
	}
	if stream[0] == 1 && stream.len() >= 7 {
		return avcc_header_to_annex_b(stream);
	}

	let mut annex_b = Vec::with_capacity(stream.len() + 16);
	let mut offset = 0;
	while offset + 4 <= stream.len() {
		let length = u32::from_be_bytes(stream[offset..offset + 4].try_into()?) as usize;
		offset += 4;
		if length == 0 || offset + length > stream.len() {
			return Err("invalid length-prefixed H.264 output".into());
		}
		annex_b.extend_from_slice(&[0, 0, 0, 1]);
		annex_b.extend_from_slice(&stream[offset..offset + length]);
		offset += length;
	}
	if offset != stream.len() || annex_b.is_empty() {
		return Err("H.264 output is neither Annex-B nor length-prefixed AVC".into());
	}
	Ok(annex_b)
}

fn avcc_header_to_annex_b(header: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
	let mut annex_b = Vec::new();
	let mut offset = 6;
	let sps_count = (header[5] & 0x1f) as usize;
	for _ in 0..sps_count {
		append_avcc_unit(header, &mut offset, &mut annex_b)?;
	}
	if offset >= header.len() {
		return Err("AVC configuration record is missing PPS data".into());
	}
	let pps_count = header[offset] as usize;
	offset += 1;
	for _ in 0..pps_count {
		append_avcc_unit(header, &mut offset, &mut annex_b)?;
	}
	Ok(annex_b)
}

fn append_avcc_unit(
	header: &[u8],
	offset: &mut usize,
	annex_b: &mut Vec<u8>,
) -> Result<(), Box<dyn Error>> {
	if *offset + 2 > header.len() {
		return Err("truncated AVC configuration record".into());
	}
	let length = u16::from_be_bytes(header[*offset..*offset + 2].try_into()?) as usize;
	*offset += 2;
	if *offset + length > header.len() {
		return Err("truncated AVC parameter set".into());
	}
	annex_b.extend_from_slice(&[0, 0, 0, 1]);
	annex_b.extend_from_slice(&header[*offset..*offset + length]);
	*offset += length;
	Ok(())
}
