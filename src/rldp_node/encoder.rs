use anyhow::Result;

use crate::utils::*;

pub struct RaptorQEncoder {
    engine: raptorq::Encoder,
    params: RaptorQFecType,
    source_packets: Vec<raptorq::EncodingPacket>,
    encoder_index: usize,
}

impl RaptorQEncoder {
    pub fn with_data(data: &[u8]) -> Self {
        let engine = raptorq::Encoder::with_defaults(data, MAX_TRANSMISSION_UNIT as u16);
        let source_packets = engine
            .get_block_encoders()
            .iter()
            .flat_map(|encoder| encoder.source_packets().into_iter().rev())
            .collect::<Vec<_>>();

        Self {
            engine,
            params: RaptorQFecType {
                data_size: data.len() as u32,
                symbol_size: MAX_TRANSMISSION_UNIT,
                symbols_count: source_packets.len() as u32,
            },
            source_packets,
            encoder_index: 0,
        }
    }

    pub fn encode(&mut self, seqno: &mut u32) -> Result<Vec<u8>> {
        let packet = if let Some(packet) = self.source_packets.pop() {
            packet
        } else {
            let encoders = self.engine.get_block_encoders();
            let packet = match encoders[self.encoder_index].repair_packets(*seqno, 1).pop() {
                Some(packet) => packet,
                None => return Err(EncoderError::FailedToEncode.into()),
            };
            self.encoder_index = (self.encoder_index + 1) % encoders.len();
            packet
        };

        *seqno = packet.payload_id().encoding_symbol_id();

        Ok(packet.data().to_vec())
    }

    #[inline(always)]
    pub fn params(&self) -> &RaptorQFecType {
        &self.params
    }
}

pub const MAX_TRANSMISSION_UNIT: u32 = 768;

#[derive(thiserror::Error, Debug)]
enum EncoderError {
    #[error("Failed to encode repair packet")]
    FailedToEncode,
}
