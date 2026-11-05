//! Ogg page framing for an Opus stream.
//!
//! The framing follows RFC 3533 for the page layout and CRC, and RFC 7845 for
//! the `OpusHead` and `OpusTags` packets and for what a granule position means.

use alloc::vec::Vec;

use crate::error::Error;
use crate::sink::Sink;
use crate::spec::Channels;

/// Page body cap; a local choice, not an RFC 3533 limit.
const MAX_PAGE_BODY: usize = 4_096;

/// Lacing values one page can hold.
const MAX_PAGE_SEGMENTS: usize = 255;

/// Lacing value that says the packet continues into the next segment.
const CONTINUED_SEGMENT: u8 = 255;

/// Bytes one lacing segment describes.
const SEGMENT_BYTES: usize = 255;

/// Header flag for the first page of a stream.
const FLAG_BOS: u8 = 0x02;

/// Header flag for the last page of a stream.
const FLAG_EOS: u8 = 0x04;

/// Byte offset of the CRC field in a page header.
const CRC_OFFSET: usize = 22;

/// What the identification header announces about the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OggOpusConfig {
    /// Channel layout the stream carries.
    pub channels: Channels,
    /// Samples a decoder must drop from the start, counted at 48 kHz.
    pub pre_skip: u16,
    /// Sample rate the audio was captured at.
    pub input_sample_rate: u32,
    /// Serial number every page of this stream carries.
    pub serial: u32,
}

/// Frames Opus packets into Ogg pages.
pub struct OggOpusWriter<S: Sink> {
    sink: S,
    serial: u32,
    sequence: u32,
    decoded: u64,
    flushed_granule: u64,
    body: Vec<u8>,
    lacing: Vec<u8>,
    page: Vec<u8>,
}

impl<S: Sink> OggOpusWriter<S> {
    /// Writes the identification and comment headers to `sink`.
    ///
    /// # Errors
    ///
    /// Returns the sink's error when a header cannot be written.
    pub fn new(sink: S, config: OggOpusConfig) -> Result<Self, Error> {
        let mut writer = Self {
            sink,
            serial: config.serial,
            sequence: 0,
            decoded: 0,
            flushed_granule: 0,
            body: Vec::with_capacity(MAX_PAGE_BODY),
            lacing: Vec::with_capacity(MAX_PAGE_SEGMENTS),
            page: Vec::with_capacity(MAX_PAGE_BODY + MAX_PAGE_SEGMENTS + 27),
        };

        let head = identification_header(config)?;
        writer.body.extend_from_slice(&head);
        append_lacing(&mut writer.lacing, head.len())?;
        writer.flush_page(FLAG_BOS, 0)?;

        let tags = comment_header();
        writer.body.extend_from_slice(&tags);
        append_lacing(&mut writer.lacing, tags.len())?;
        writer.flush_page(0, 0)?;

        Ok(writer)
    }

    /// Adds one packet whose decoded length is `samples_48k`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidPacket`] when `packet` is empty or
    /// `samples_48k` is zero, [`Error::PacketTooLarge`] when the packet needs
    /// more lacing values than a page holds, and the sink's error when a page
    /// cannot be written.
    pub fn push_packet(&mut self, packet: &[u8], samples_48k: u32) -> Result<(), Error> {
        if packet.is_empty() || samples_48k == 0 {
            return Err(Error::InvalidPacket);
        }
        let segments = lacing_segments(packet.len());
        if segments > MAX_PAGE_SEGMENTS {
            return Err(Error::PacketTooLarge {
                bytes: packet.len(),
            });
        }

        // The pending page keeps its last packet until another arrives, which is
        // what puts the EOS flag on the page holding the final packet.
        let overflows = self.lacing.len() + segments > MAX_PAGE_SEGMENTS
            || self.body.len() + packet.len() > MAX_PAGE_BODY;
        if overflows && !self.body.is_empty() {
            let granule = self.decoded;
            self.flush_page(0, granule)?;
        }

        append_lacing(&mut self.lacing, packet.len())?;
        self.body.extend_from_slice(packet);
        self.decoded =
            self.decoded
                .checked_add(u64::from(samples_48k))
                .ok_or(Error::ContainerInvariant {
                    operation: "push packet",
                })?;
        Ok(())
    }

    /// Writes the pending page with the end-of-stream flag and returns the sink.
    ///
    /// The final granule may be lower than the decoded total: RFC 7845 lets it
    /// trim the padding the encoder had to add to the last packet.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ContainerInvariant`] when `final_granule` exceeds the
    /// decoded total or is lower than the granule of an already written page,
    /// and the sink's error when the page cannot be written.
    pub fn finish(mut self, final_granule: u64) -> Result<S, Error> {
        if final_granule > self.decoded || final_granule < self.flushed_granule {
            return Err(Error::ContainerInvariant {
                operation: "finish",
            });
        }
        self.flush_page(FLAG_EOS, final_granule)?;
        Ok(self.sink)
    }

    /// Writes the pending page and clears it for reuse.
    fn flush_page(&mut self, header_type: u8, granule: u64) -> Result<(), Error> {
        let body = core::mem::take(&mut self.body);
        let lacing = core::mem::take(&mut self.lacing);
        let result = self.write_page(header_type, granule, &lacing, &body);
        self.body = body;
        self.lacing = lacing;
        self.body.clear();
        self.lacing.clear();
        result?;
        self.flushed_granule = granule;
        Ok(())
    }

    fn write_page(
        &mut self,
        header_type: u8,
        granule: u64,
        lacing: &[u8],
        body: &[u8],
    ) -> Result<(), Error> {
        let granule = i64::try_from(granule).map_err(|_| Error::ContainerInvariant {
            operation: "write page",
        })?;
        let segments = u8::try_from(lacing.len()).map_err(|_| Error::ContainerInvariant {
            operation: "write page",
        })?;

        self.page.clear();
        self.page.extend_from_slice(b"OggS");
        self.page.push(0);
        self.page.push(header_type);
        self.page.extend_from_slice(&granule.to_le_bytes());
        self.page.extend_from_slice(&self.serial.to_le_bytes());
        self.page.extend_from_slice(&self.sequence.to_le_bytes());
        self.page.extend_from_slice(&[0; 4]);
        self.page.push(segments);
        self.page.extend_from_slice(lacing);
        self.page.extend_from_slice(body);

        let crc = crc32(&self.page);
        self.page[CRC_OFFSET..CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
        self.sink.write_all(&self.page)?;
        self.sequence = self.sequence.wrapping_add(1);
        Ok(())
    }
}

/// Builds the `OpusHead` packet.
fn identification_header(config: OggOpusConfig) -> Result<[u8; 19], Error> {
    let channels =
        u8::try_from(config.channels.count()).map_err(|_| Error::ContainerInvariant {
            operation: "identification header",
        })?;
    let mut head = [0_u8; 19];
    head[..8].copy_from_slice(b"OpusHead");
    head[8] = 1;
    head[9] = channels;
    head[10..12].copy_from_slice(&config.pre_skip.to_le_bytes());
    head[12..16].copy_from_slice(&config.input_sample_rate.to_le_bytes());
    // Output gain stays at 0: the mixer already applied the source gains.
    head[16..18].copy_from_slice(&0_i16.to_le_bytes());
    // Mapping family 0 is the only one a mono or stereo stream may use.
    head[18] = 0;
    Ok(head)
}

/// Builds the `OpusTags` packet with the vendor string and no comments.
fn comment_header() -> [u8; 19] {
    let mut tags = [0_u8; 19];
    tags[..8].copy_from_slice(b"OpusTags");
    tags[8..12].copy_from_slice(&3_u32.to_le_bytes());
    tags[12..15].copy_from_slice(b"koe");
    tags[15..19].copy_from_slice(&0_u32.to_le_bytes());
    tags
}

/// Lacing values a packet of `bytes` bytes needs.
const fn lacing_segments(bytes: usize) -> usize {
    bytes / SEGMENT_BYTES + 1
}

/// Appends the lacing values of a packet with `bytes` bytes.
fn append_lacing(lacing: &mut Vec<u8>, bytes: usize) -> Result<(), Error> {
    let segments = lacing_segments(bytes);
    if segments > MAX_PAGE_SEGMENTS {
        return Err(Error::PacketTooLarge { bytes });
    }
    lacing.reserve(segments);
    for _ in 1..segments {
        lacing.push(CONTINUED_SEGMENT);
    }
    let remainder = u8::try_from(bytes % SEGMENT_BYTES).map_err(|_| Error::ContainerInvariant {
        operation: "append lacing",
    })?;
    lacing.push(remainder);
    Ok(())
}

/// Ogg's CRC-32: polynomial `0x04C11DB7`, zero init, no reflection, no final xor.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0_u32;
    for byte in bytes {
        let index = usize::from((crc >> 24).to_le_bytes()[0] ^ *byte);
        crc = (crc << 8) ^ CRC_TABLE[index];
    }
    crc
}

const CRC_TABLE: [u32; 256] = crc_table();

const fn crc_table() -> [u32; 256] {
    let mut table = [0_u32; 256];
    let mut index = 0;
    let mut seed = 0_u32;
    while index < table.len() {
        let mut value = seed << 24;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 0x8000_0000 == 0 {
                value << 1
            } else {
                (value << 1) ^ 0x04C1_1DB7
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
        seed += 1;
    }
    table
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp, clippy::needless_range_loop)]

    use alloc::vec;

    use super::*;

    struct VecSink(Vec<u8>);

    impl Sink for VecSink {
        fn write_all(&mut self, bytes: &[u8]) -> Result<(), Error> {
            self.0.extend_from_slice(bytes);
            Ok(())
        }
    }

    fn config() -> OggOpusConfig {
        OggOpusConfig {
            channels: Channels::Stereo,
            pre_skip: 312,
            input_sample_rate: 48_000,
            serial: 0x1234_5678,
        }
    }

    fn writer() -> OggOpusWriter<VecSink> {
        OggOpusWriter::new(VecSink(Vec::new()), config()).unwrap()
    }

    #[derive(Debug)]
    struct Page {
        header_type: u8,
        granule: i64,
        serial: u32,
        sequence: u32,
        lacing: Vec<u8>,
        body: Vec<u8>,
        packets: Vec<Vec<u8>>,
    }

    /// Recomputes the Ogg CRC bit by bit, without the writer's table.
    fn parse_crc(bytes: &[u8]) -> u32 {
        let mut crc = 0_u32;
        for byte in bytes {
            crc ^= u32::from(*byte) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 == 0 {
                    crc << 1
                } else {
                    (crc << 1) ^ 0x04C1_1DB7
                };
            }
        }
        crc
    }

    fn parse_pages(bytes: &[u8]) -> Vec<Page> {
        let mut pages = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            assert_eq!(
                &bytes[offset..offset + 4],
                b"OggS",
                "every page starts with the capture pattern"
            );
            assert_eq!(bytes[offset + 4], 0, "the page version is zero");
            let header_type = bytes[offset + 5];
            let granule = i64::from_le_bytes(bytes[offset + 6..offset + 14].try_into().unwrap());
            let serial = u32::from_le_bytes(bytes[offset + 14..offset + 18].try_into().unwrap());
            let sequence = u32::from_le_bytes(bytes[offset + 18..offset + 22].try_into().unwrap());
            let crc = u32::from_le_bytes(bytes[offset + 22..offset + 26].try_into().unwrap());
            let segments = usize::from(bytes[offset + 26]);
            let lacing_start = offset + 27;
            let lacing = bytes[lacing_start..lacing_start + segments].to_vec();
            let body_len: usize = lacing.iter().map(|value| usize::from(*value)).sum();
            let body_start = lacing_start + segments;
            let body = bytes[body_start..body_start + body_len].to_vec();
            let end = body_start + body_len;

            let mut zeroed = bytes[offset..end].to_vec();
            zeroed[22..26].fill(0);
            assert_eq!(crc, parse_crc(&zeroed), "page CRC covers the whole page");

            let mut packets = Vec::new();
            let mut start = 0;
            let mut length = 0;
            for value in &lacing {
                length += usize::from(*value);
                if *value < CONTINUED_SEGMENT {
                    packets.push(body[start..start + length].to_vec());
                    start += length;
                    length = 0;
                }
            }

            pages.push(Page {
                header_type,
                granule,
                serial,
                sequence,
                lacing,
                body,
                packets,
            });
            offset = end;
        }
        pages
    }

    fn finished(writer: OggOpusWriter<VecSink>, granule: u64) -> Vec<Page> {
        let sink = writer.finish(granule).unwrap();
        parse_pages(&sink.0)
    }

    #[test]
    fn crc_matches_the_reference_vectors() {
        assert_eq!(crc32(b"123456789"), 0x89A1_897F);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn headers_describe_the_stream() {
        let pages = finished(writer(), 0);
        assert_eq!(pages.len(), 3, "two headers and the empty end page");

        let head = &pages[0];
        assert_eq!(head.header_type, FLAG_BOS);
        assert_eq!(head.granule, 0);
        assert_eq!(head.sequence, 0);
        assert_eq!(head.serial, 0x1234_5678);
        assert_eq!(head.packets.len(), 1);
        let packet = &head.packets[0];
        assert_eq!(packet.len(), 19);
        assert_eq!(&packet[..8], b"OpusHead");
        assert_eq!(packet[8], 1, "the version is 1");
        assert_eq!(packet[9], 2, "the channel count comes from the layout");
        assert_eq!(u16::from_le_bytes([packet[10], packet[11]]), 312);
        assert_eq!(
            u32::from_le_bytes([packet[12], packet[13], packet[14], packet[15]]),
            48_000
        );
        assert_eq!(i16::from_le_bytes([packet[16], packet[17]]), 0);
        assert_eq!(packet[18], 0, "the mapping family is 0");

        let tags = &pages[1];
        assert_eq!(tags.header_type, 0);
        assert_eq!(tags.granule, 0);
        assert_eq!(tags.sequence, 1);
        let packet = &tags.packets[0];
        assert_eq!(packet.len(), 19);
        assert_eq!(&packet[..8], b"OpusTags");
        assert_eq!(u32::from_le_bytes(packet[8..12].try_into().unwrap()), 3);
        assert_eq!(&packet[12..15], b"koe");
        assert_eq!(u32::from_le_bytes(packet[15..19].try_into().unwrap()), 0);

        let end = &pages[2];
        assert_eq!(end.header_type, FLAG_EOS);
        assert_eq!(end.granule, 0);
        assert_eq!(end.sequence, 2);
        assert_eq!(end.packets, Vec::<Vec<u8>>::new());
    }

    #[test]
    fn one_packet_reports_its_decoded_samples() {
        let mut writer = writer();
        writer.push_packet(&[0x11; 100], 960).unwrap();
        let pages = finished(writer, 960);

        let audio = pages.last().unwrap();
        assert_eq!(audio.header_type, FLAG_EOS);
        assert_eq!(audio.granule, 960);
        assert_eq!(audio.packets, vec![vec![0x11; 100]]);
    }

    #[test]
    fn packet_lacing_splits_at_255_bytes() {
        for (length, expected) in [
            (1_usize, vec![1_u8]),
            (255, vec![255, 0]),
            (510, vec![255, 255, 0]),
            (511, vec![255, 255, 1]),
        ] {
            let mut writer = writer();
            writer.push_packet(&vec![0x5A; length], 960).unwrap();
            let pages = finished(writer, 960);
            let audio = pages.last().unwrap();
            assert_eq!(audio.lacing, expected, "lacing for a {length}-byte packet");
            assert_eq!(audio.packets, vec![vec![0x5A; length]]);
        }
    }

    #[test]
    fn the_largest_fittable_packet_uses_every_segment() {
        let mut writer = writer();
        writer.push_packet(&vec![0x33; 65_024], 960).unwrap();
        let pages = finished(writer, 960);
        assert_eq!(
            pages.len(),
            3,
            "headers and one audio page, with no empty page"
        );
        let audio = pages.last().unwrap();
        assert_eq!(audio.lacing.len(), 255);
        assert_eq!(audio.packets[0].len(), 65_024);
    }

    #[test]
    fn a_page_holds_255_one_segment_packets() {
        let mut stream = writer();
        for _ in 0..256 {
            stream.push_packet(&[0x77], 960).unwrap();
        }
        let pages = finished(stream, 256 * 960);
        let audio = &pages[2..];
        assert_eq!(audio.len(), 2, "the 256th packet starts a second page");
        assert_eq!(
            audio[0].packets.len(),
            MAX_PAGE_SEGMENTS,
            "the first page takes every lacing value"
        );
        assert_eq!(audio[1].packets.len(), 1);
        assert_eq!(audio[1].header_type, FLAG_EOS);
    }

    #[test]
    fn a_page_stops_at_the_body_cap() {
        let mut stream = writer();
        for _ in 0..5 {
            stream.push_packet(&[0x88; 1_024], 960).unwrap();
        }
        let pages = finished(stream, 5 * 960);
        let audio = &pages[2..];
        assert_eq!(
            audio.len(),
            2,
            "four packets fill the page and the fifth starts another"
        );
        assert_eq!(audio[0].packets.len(), 4);
        assert_eq!(audio[0].body.len(), MAX_PAGE_BODY);
        assert_eq!(audio[1].packets.len(), 1);
    }

    #[test]
    fn packets_beyond_one_page_are_rejected() {
        let mut writer = writer();
        assert_eq!(
            writer.push_packet(&vec![0; 65_025], 960),
            Err(Error::PacketTooLarge { bytes: 65_025 })
        );
    }

    #[test]
    fn empty_packets_and_packets_without_samples_are_rejected() {
        let mut writer = writer();
        assert_eq!(writer.push_packet(&[], 960), Err(Error::InvalidPacket));
        assert_eq!(writer.push_packet(&[0; 8], 0), Err(Error::InvalidPacket));
    }

    #[test]
    fn granules_count_every_packet_pushed() {
        let mut stream = writer();
        for _ in 0..40 {
            stream.push_packet(&[0x22; 300], 960).unwrap();
        }
        let pages = finished(stream, 40 * 960);

        let audio = &pages[2..];
        assert!(audio.len() >= 4, "40 packets need several pages");
        let mut cumulative = 0;
        for page in audio {
            cumulative += i64::try_from(page.packets.len()).unwrap() * 960;
            assert_eq!(
                page.granule, cumulative,
                "each granule counts every packet pushed so far"
            );
        }
        assert_eq!(cumulative, 40 * 960);
    }

    #[test]
    fn a_multi_page_stream_keeps_its_packets_and_flags() {
        let mut stream = writer();
        let packets: Vec<Vec<u8>> = (0..600_u16)
            .map(|index| vec![u8::try_from(index % 251).unwrap(); 300])
            .collect();
        for packet in &packets {
            stream.push_packet(packet, 480).unwrap();
        }
        let pages = finished(stream, 600 * 480);
        assert!(pages.len() > 10, "the stream must span many pages");

        for (index, page) in pages.iter().enumerate() {
            assert_eq!(
                usize::try_from(page.sequence).unwrap(),
                index,
                "sequence numbers count every page"
            );
            assert_eq!(page.serial, 0x1234_5678);
            assert!(
                page.body.len() <= MAX_PAGE_BODY,
                "pages respect the body cap"
            );
        }
        assert_eq!(
            pages[0].header_type, FLAG_BOS,
            "only page 0 starts the stream"
        );
        assert!(
            pages[1..]
                .iter()
                .all(|page| page.header_type & FLAG_BOS == 0)
        );
        let last = pages.len() - 1;
        assert_eq!(pages[last].header_type, FLAG_EOS, "the last page ends it");
        assert!(
            pages[..last]
                .iter()
                .all(|page| page.header_type & FLAG_EOS == 0),
            "no page ends the stream before the last one"
        );

        let recovered: Vec<Vec<u8>> = pages[2..]
            .iter()
            .flat_map(|page| page.packets.iter().cloned())
            .collect();
        assert_eq!(recovered, packets, "every packet survives the framing");
    }

    #[test]
    fn the_final_granule_trims_the_padding() {
        let mut stream = writer();
        for _ in 0..3 {
            stream.push_packet(&[0x44; 100], 960).unwrap();
        }
        let pages = finished(stream, 2_000);
        assert_eq!(pages.last().unwrap().granule, 2_000);
    }

    #[test]
    fn a_granule_outside_the_stream_is_rejected() {
        let mut stream = writer();
        for _ in 0..40 {
            stream.push_packet(&[0x55; 300], 960).unwrap();
        }
        assert_eq!(
            stream.finish(40 * 960 + 1).err(),
            Some(Error::ContainerInvariant {
                operation: "finish"
            })
        );

        let mut stream = writer();
        for _ in 0..40 {
            stream.push_packet(&[0x55; 300], 960).unwrap();
        }
        assert_eq!(
            stream.finish(0).err(),
            Some(Error::ContainerInvariant {
                operation: "finish"
            })
        );
    }

    #[test]
    fn a_flushed_page_carries_no_end_flag() {
        let mut stream = writer();
        for _ in 0..40 {
            stream.push_packet(&[0x66; 300], 960).unwrap();
        }
        let pages = finished(stream, 40 * 960);
        assert!(
            pages[2..pages.len() - 1]
                .iter()
                .all(|page| page.header_type == 0)
        );
    }
}
