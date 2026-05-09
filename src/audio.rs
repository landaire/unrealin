//! DARE IMA-ADPCM v3 reader and decoder.
//!
//! Format derived from `splintercell.xbe` static analysis (see
//! `docs/AUDIO.md`). The decoder is a best-effort port of `sub_195d70`;
//! several fields and the chunk seed prologue are still inferred. Output
//! correctness should be validated by ear against a known cue.

use std::io;

/// Codec id stored in byte 0 of the header. SC1 only ever ships v3.
pub const CODEC_VERSION_V3: u8 = 0x03;

/// Per-channel decoder state size, taken from `sub_1940b0` (writes the
/// initial step index of 5 at offset +4 of each 0x34-byte slot).
pub const PER_CHANNEL_STATE_BYTES: usize = 0x34;

const IMA_STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

const IMA_INDEX_TABLE: [i32; 16] = [
    -1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8,
];

/// 28-byte header read by `TImaAdpcm::InitHeader` (`sub_193780`).
///
/// Field interpretation is partial. `InitHeader` only explicitly touches
/// the four byte-swap targets (file offsets 14, 16, 20, 26) and the stereo
/// flag at offset 12; everything else is captured as raw bytes for
/// diagnostic dumping until we map more of the codec state struct.
#[derive(Clone, Debug)]
pub struct ImaHeader {
    pub codec_version: u8,
    pub raw_bytes_1_3: [u8; 3],
    pub ratio: f32,
    pub raw_8_11: [u8; 4],
    pub stereo_flag: u8,
    pub raw_byte_13: u8,
    /// File offset 14, big-endian. After byte-swap this becomes the dword
    /// at codec-state +0x40 (sign-extended). Treated as the "primary
    /// sample/block count" but exact meaning is still tbd.
    pub field_14: u16,
    /// File offset 16, big-endian.
    pub field_16: u16,
    pub raw_18_19: [u8; 2],
    /// File offset 20, big-endian.
    pub field_20: u16,
    pub raw_22_25: [u8; 4],
    /// File offset 26, big-endian.
    pub field_26: u16,
}

impl ImaHeader {
    pub const SIZE: usize = 28;

    pub fn parse(buf: &[u8]) -> io::Result<Self> {
        if buf.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "ima header truncated",
            ));
        }
        let codec_version = buf[0];
        if codec_version != CODEC_VERSION_V3 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported codec version 0x{codec_version:02x}, expected 0x03"),
            ));
        }
        Ok(Self {
            codec_version,
            raw_bytes_1_3: buf[1..4].try_into().unwrap(),
            ratio: f32::from_le_bytes(buf[4..8].try_into().unwrap()),
            raw_8_11: buf[8..12].try_into().unwrap(),
            stereo_flag: buf[12],
            raw_byte_13: buf[13],
            field_14: u16::from_be_bytes([buf[14], buf[15]]),
            field_16: u16::from_be_bytes([buf[16], buf[17]]),
            raw_18_19: buf[18..20].try_into().unwrap(),
            field_20: u16::from_be_bytes([buf[20], buf[21]]),
            raw_22_25: buf[22..26].try_into().unwrap(),
            field_26: u16::from_be_bytes([buf[26], buf[27]]),
        })
    }

    pub fn channels(&self) -> u16 {
        // Engine accepts 1, 2, 4, 6. We've only observed 0 and 1 so far
        // in shipped headers.
        if self.stereo_flag == 0 { 1 } else { 2 }
    }

    /// Best guess at samples-per-block. The runtime formula is
    /// `block_size = samples_per_block * bits / 8 + 1` with
    /// `bits_per_sample` defaulted to 4 in `sub_1940b0`. Until we
    /// identify the source field we hardcode the IMA-typical 64.
    pub fn samples_per_block(&self) -> u32 {
        64
    }

    pub fn bits_per_sample(&self) -> u32 {
        4
    }

    /// Per-channel encoded block size in bytes, matching the runtime
    /// formula `(samples_per_block * bits / 8) + 1`.
    pub fn block_bytes_per_channel(&self) -> usize {
        (self.samples_per_block() as usize * self.bits_per_sample() as usize) / 8 + 1
    }

    /// Sample rate guess. The `f32 ratio` field at +4 almost certainly
    /// encodes the rate, but the formula is not yet known. Default to
    /// 22050 Hz which is typical for Xbox-era voice/SFX banks.
    pub fn sample_rate_guess(&self) -> u32 {
        22050
    }
}

/// Per-channel IMA decoder state.
#[derive(Clone, Debug)]
struct ChannelState {
    predictor: i32,
    step_index: i32,
}

impl ChannelState {
    fn new() -> Self {
        Self {
            predictor: 0,
            step_index: 5,
        }
    }

    /// Seed from a 52-byte slot copied from the chunk prologue. We treat
    /// byte 0..1 as the int16 predictor and byte 4..7 as the int32
    /// step_index based on the layout in `sub_1940b0`. Other bytes
    /// (lookahead/working buffers) are ignored by this straight-line
    /// decoder.
    fn seed_from_slot(&mut self, slot: &[u8]) {
        self.predictor = i16::from_le_bytes([slot[0], slot[1]]) as i32;
        let step_index = i32::from_le_bytes([slot[4], slot[5], slot[6], slot[7]]);
        self.step_index = step_index.clamp(0, 88);
    }

    fn decode_nibble(&mut self, nibble: u8) -> i16 {
        let step = IMA_STEP_TABLE[self.step_index as usize];
        let sign = nibble & 0x8;
        let mag = (nibble & 0x7) as i32;

        let mut diff = step >> 3;
        if mag & 4 != 0 {
            diff += step;
        }
        if mag & 2 != 0 {
            diff += step >> 1;
        }
        if mag & 1 != 0 {
            diff += step >> 2;
        }
        if sign != 0 {
            self.predictor -= diff;
        } else {
            self.predictor += diff;
        }
        self.predictor = self.predictor.clamp(-32768, 32767);

        self.step_index = (self.step_index + IMA_INDEX_TABLE[nibble as usize]).clamp(0, 88);
        self.predictor as i16
    }
}

/// Decoder output. PCM is interleaved when `channels > 1`.
#[derive(Debug)]
pub struct Decoded {
    pub header: ImaHeader,
    pub pcm: Vec<i16>,
}

/// Decode a single-stream `.SS2` / `.LS2` payload to interleaved 16-bit
/// PCM. `data` must start at the IMA header (codec id 0x03 at index 0).
///
/// Multi-bank containers (`Music_*.SS2`, `STREAM.SS2`) need their outer
/// index decoded first; this function is for single-stream files only.
pub fn decode_single_stream(data: &[u8]) -> io::Result<Decoded> {
    let header = ImaHeader::parse(data)?;
    let channels = header.channels() as usize;
    let block_bytes = header.block_bytes_per_channel();
    let samples_per_block = header.samples_per_block() as usize;

    let mut cursor = &data[ImaHeader::SIZE..];

    let prologue_size = channels * PER_CHANNEL_STATE_BYTES;
    if cursor.len() < prologue_size {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "missing channel state prologue",
        ));
    }
    let mut states = vec![ChannelState::new(); channels];
    for (ch, state) in states.iter_mut().enumerate() {
        let slot_start = ch * PER_CHANNEL_STATE_BYTES;
        let slot = &cursor[slot_start..slot_start + PER_CHANNEL_STATE_BYTES];
        state.seed_from_slot(slot);
    }
    cursor = &cursor[prologue_size..];

    // Decode per-channel sample buffers, then interleave.
    let mut per_channel: Vec<Vec<i16>> = vec![Vec::new(); channels];

    while cursor.len() >= block_bytes * channels {
        decode_block_into(
            &cursor[..block_bytes * channels],
            &mut states,
            samples_per_block,
            &mut per_channel,
        );
        cursor = &cursor[block_bytes * channels..];
    }

    let total_samples = per_channel[0].len();
    let mut pcm = Vec::with_capacity(total_samples * channels);
    for i in 0..total_samples {
        for ch in 0..channels {
            pcm.push(per_channel[ch][i]);
        }
    }

    Ok(Decoded { header, pcm })
}

fn decode_block_into(
    block: &[u8],
    states: &mut [ChannelState],
    samples_per_block: usize,
    per_channel: &mut [Vec<i16>],
) {
    let channels = states.len();
    let nibble_bytes = samples_per_block / 2;
    let block_bytes_per_ch = nibble_bytes + 1;

    for (ch, state) in states.iter_mut().enumerate() {
        let off = ch * block_bytes_per_ch;
        // Leading byte: best guess is a step-index refresh. If the
        // decoder drifts this is the most likely place to revisit.
        state.step_index = (block[off] as i32).clamp(0, 88);
    }

    for nibble_idx in 0..nibble_bytes {
        for (ch, state) in states.iter_mut().enumerate() {
            let off = ch * block_bytes_per_ch + 1 + nibble_idx;
            let byte = block[off];
            per_channel[ch].push(state.decode_nibble(byte & 0x0f));
            per_channel[ch].push(state.decode_nibble((byte >> 4) & 0x0f));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_parse_minimal() {
        let mut buf = [0u8; ImaHeader::SIZE];
        buf[0] = 0x03;
        buf[12] = 0x01;
        buf[14] = 0x00;
        buf[15] = 0x0a;
        let h = ImaHeader::parse(&buf).unwrap();
        assert_eq!(h.codec_version, 0x03);
        assert_eq!(h.stereo_flag, 0x01);
        assert_eq!(h.field_14, 0x000a);
        assert_eq!(h.channels(), 2);
    }

    #[test]
    fn header_rejects_wrong_version() {
        let mut buf = [0u8; ImaHeader::SIZE];
        buf[0] = 0x02;
        let err = ImaHeader::parse(&buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
