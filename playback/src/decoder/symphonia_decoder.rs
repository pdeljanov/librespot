use std::io;

use symphonia::core::{
    audio::SampleBuffer,
    codecs::{Decoder, DecoderOptions},
    errors::Error,
    formats::{FormatReader, SeekMode, SeekTo},
    io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions},
    meta::{MetadataOptions, StandardTagKey, Value},
    probe::Hint,
    units::Time,
};

use super::{AudioDecoder, AudioPacket, DecoderError, DecoderResult};

use crate::{
    metadata::audio::{AudioFileFormat, AudioFiles},
    player::NormalisationData,
    NUM_CHANNELS, PAGES_PER_MS, SAMPLE_RATE,
};

pub struct SymphoniaDecoder {
    decoder: Box<dyn Decoder>,
    format: Box<dyn FormatReader>,
    sample_buffer: Option<SampleBuffer<f64>>,
}

impl SymphoniaDecoder {
    pub fn new<R>(input: R, format: AudioFileFormat) -> DecoderResult<Self>
    where
        R: MediaSource + 'static,
    {
        let mss_opts = MediaSourceStreamOptions {
            buffer_len: librespot_audio::MINIMUM_DOWNLOAD_SIZE,
        };
        let mss = MediaSourceStream::new(Box::new(input), mss_opts);

        // Not necessary, but speeds up loading.
        let mut hint = Hint::new();
        if AudioFiles::is_ogg_vorbis(format) {
            hint.with_extension("ogg");
            hint.mime_type("audio/ogg");
        } else if AudioFiles::is_mp3(format) {
            hint.with_extension("mp3");
            hint.mime_type("audio/mp3");
        } else if AudioFiles::is_flac(format) {
            hint.with_extension("flac");
            hint.mime_type("audio/flac");
        }

        let format_opts = Default::default();
        let metadata_opts: MetadataOptions = Default::default();
        let decoder_opts: DecoderOptions = Default::default();

        let probed =
            symphonia::default::get_probe().format(&hint, mss, &format_opts, &metadata_opts)?;
        let format = probed.format;

        let track = format.default_track().ok_or_else(|| {
            DecoderError::SymphoniaDecoder("Could not retrieve default track".into())
        })?;

        let decoder = symphonia::default::get_codecs().make(&track.codec_params, &decoder_opts)?;

        let codec_params = decoder.codec_params();
        let rate = codec_params.sample_rate.ok_or_else(|| {
            DecoderError::SymphoniaDecoder("Could not retrieve sample rate".into())
        })?;
        let channels = codec_params.channels.ok_or_else(|| {
            DecoderError::SymphoniaDecoder("Could not retrieve channel configuration".into())
        })?;

        if rate != SAMPLE_RATE {
            return Err(DecoderError::SymphoniaDecoder(format!(
                "Unsupported sample rate: {}",
                rate
            )));
        }

        if channels.count() != NUM_CHANNELS as usize {
            return Err(DecoderError::SymphoniaDecoder(format!(
                "Unsupported number of channels: {}",
                channels
            )));
        }

        Ok(Self {
            decoder,
            format,

            // We set the sample buffer when decoding the first full packet,
            // whose duration is also the ideal sample buffer size.
            sample_buffer: None,
        })
    }

    pub fn normalisation_data(&mut self) -> Option<NormalisationData> {
        let mut metadata = self.format.metadata();
        loop {
            if let Some(_discarded_revision) = metadata.pop() {
                // Advance to the latest metadata revision.
                continue;
            } else {
                let revision = metadata.current()?;
                let tags = revision.tags();

                if tags.is_empty() {
                    // The latest metadata entry in the log is empty.
                    return None;
                }

                let mut data = NormalisationData::default();
                let mut i = 0;
                while i < tags.len() {
                    if let Value::Float(value) = tags[i].value {
                        #[allow(non_snake_case)]
                        match tags[i].std_key {
                            Some(StandardTagKey::ReplayGainAlbumGain) => data.album_gain_db = value,
                            Some(StandardTagKey::ReplayGainAlbumPeak) => data.album_peak = value,
                            Some(StandardTagKey::ReplayGainTrackGain) => data.track_gain_db = value,
                            Some(StandardTagKey::ReplayGainTrackPeak) => data.track_peak = value,
                            _ => (),
                        }
                    }
                    i += 1;
                }

                break Some(data);
            }
        }
    }

    fn ts_to_ms(&self, ts: u64) -> u32 {
        let time_base = self.decoder.codec_params().time_base;
        let seeked_to_ms = match time_base {
            Some(time_base) => {
                let time = time_base.calc_time(ts);
                (time.seconds as f64 + time.frac) * 1000.
            }
            // Fallback in the unexpected case that the format has no base time set.
            None => (ts as f64 * PAGES_PER_MS),
        };
        seeked_to_ms as u32
    }
}

impl AudioDecoder for SymphoniaDecoder {
    fn seek(&mut self, position_ms: u32) -> Result<u32, DecoderError> {
        let seconds = position_ms as u64 / 1000;
        let frac = (position_ms as f64 % 1000.) / 1000.;
        let time = Time::new(seconds, frac);

        // `track_id: None` implies the default track ID (of the container, not of Spotify).
        let seeked_to_ts = self.format.seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time,
                track_id: None,
            },
        )?;

        // Seeking is a `FormatReader` operation, so the decoder cannot reliably
        // know when a seek took place. Reset it to avoid audio glitches.
        self.decoder.reset();

        Ok(self.ts_to_ms(seeked_to_ts.actual_ts))
    }

    fn next_packet(&mut self) -> DecoderResult<Option<(u32, AudioPacket)>> {
        let packet = match self.format.next_packet() {
            Ok(packet) => packet,
            Err(Error::IoError(err)) => {
                if err.kind() == io::ErrorKind::UnexpectedEof {
                    return Ok(None);
                } else {
                    return Err(DecoderError::SymphoniaDecoder(err.to_string()));
                }
            }
            Err(Error::ResetRequired) => {
                self.decoder.reset();
                return self.next_packet();
            }
            Err(err) => {
                return Err(err.into());
            }
        };

        let position_ms = self.ts_to_ms(packet.pts());

        match self.decoder.decode(&packet) {
            Ok(decoded) => {
                if self.sample_buffer.is_none() {
                    let spec = *decoded.spec();
                    let duration = decoded.capacity() as u64;
                    self.sample_buffer
                        .replace(SampleBuffer::new(duration, spec));
                }

                let sample_buffer = self.sample_buffer.as_mut().unwrap(); // guaranteed above
                sample_buffer.copy_interleaved_ref(decoded);
                let samples = AudioPacket::Samples(sample_buffer.samples().to_vec());
                Ok(Some((position_ms, samples)))
            }
            // Also propagate `ResetRequired` errors from the decoder to the player,
            // so that it will skip to the next track and reload the entire Symphonia decoder.
            Err(err) => Err(err.into()),
        }
    }
}
