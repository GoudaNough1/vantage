// Author: Riley @ MHK Consultants
// Date: 2026-06-25
// Purpose: Shared Vantage protocol - message types and length-prefixed framing over async streams

use enigo::agent::Token;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// === Protocol ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
	Hello { width: u32, height: u32 },
	Frame(Vec<u8>),
	Input(Token),
}

fn encode(message: &Message) -> Result<Vec<u8>, bincode::error::EncodeError> {
	bincode::serde::encode_to_vec(message, bincode::config::standard())
}

fn decode(bytes: &[u8]) -> Result<Message, bincode::error::DecodeError> {
	let (message, _) = bincode::serde::decode_from_slice(bytes, bincode::config::standard())?;
	Ok(message)
}

// === Length-prefixed framing ===

pub async fn write_message<W: AsyncWrite + Unpin>(writer: &mut W, message: &Message) -> std::io::Result<()> {
	let bytes = encode(message).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
	writer.write_u32(bytes.len() as u32).await?;
	writer.write_all(&bytes).await?;
	Ok(())
}

pub async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<Message> {
	let len = reader.read_u32().await? as usize;
	let mut bytes = vec![0u8; len];
	reader.read_exact(&mut bytes).await?;
	decode(&bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
