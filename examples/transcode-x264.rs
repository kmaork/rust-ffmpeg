// Given an input file, transcode all video streams into H.264 (using libx264)
// while copying audio and subtitle streams.
//
// Invocation:
//
//   transcode-x264 <input> <output> [<x264_opts>]
//
// <x264_opts> is a comma-delimited list of key=val. default is "preset=medium".
// See https://ffmpeg.org/ffmpeg-codecs.html#libx264_002c-libx264rgb and
// https://trac.ffmpeg.org/wiki/Encode/H.264 for available and commonly used
// options.
//
// Examples:
//
//   transcode-x264 input.flv output.mp4
//   transcode-x264 input.mkv output.mkv 'preset=veryslow,crf=18'

extern crate ffmpeg_next as ffmpeg;
extern crate multiqueue;

use std::collections::HashMap;
use std::env;
use std::time::Instant;
use multiqueue::broadcast_queue;
use ffmpeg::{
    codec, decoder, encoder, format, frame, log, media, picture, Dictionary, Packet, Rational,
};
use ffmpeg::device::input::audio;

const DEFAULT_X264_OPTS: &str = "preset=medium";

struct VideoEncoder {
    ost_index: usize,
    encoder: encoder::video::Video,
    logging_enabled: bool,
    frame_count: usize,
    last_log_frame_count: usize,
    starting_time: Instant,
    last_log_time: Instant,
}

impl VideoEncoder {
    fn new(
        width: u32,
        height: u32,
        frame_rate: Rational,
        octx: &mut format::context::Output,
        ost_index: usize,
        x264_opts: Dictionary,
        enable_logging: bool,
    ) -> Result<Self, ffmpeg::Error> {
        let global_header = octx.format().flags().contains(format::Flags::GLOBAL_HEADER);
        let mut ost = octx.add_stream(encoder::find(codec::Id::H264))?;
        let mut encoder = ost.codec().encoder().video()?;
        encoder.set_height(height);
        encoder.set_width(width);
        //encoder.set_aspect_ratio(decoder.aspect_ratio());
        encoder.set_format(format::Pixel::RGBA);
        encoder.set_frame_rate(Some(frame_rate));
        encoder.set_time_base(frame_rate.invert());
        if global_header {
            encoder.set_flags(codec::Flags::GLOBAL_HEADER);
        }
        encoder
            .open_with(x264_opts)
            .expect("error opening libx264 encoder with supplied settings");
        encoder = ost.codec().encoder().video()?;
        ost.set_parameters(encoder);
        Ok(Self {
            ost_index,
            encoder: ost.codec().encoder().video()?,
            logging_enabled: enable_logging,
            frame_count: 0,
            last_log_frame_count: 0,
            starting_time: Instant::now(),
            last_log_time: Instant::now(),
        })
    }

    fn encode_frame(
        &mut self,
        octx: &mut format::context::Output,
        ost_time_base: Rational,
        ist_time_base: Rational,
        frame: frame::Video,
    ) {
        // frame.set_pts(timestamp);
        // frame.set_kind(picture::Type::None);
        self.send_frame_to_encoder(&frame);
        self.receive_and_process_encoded_packets(octx, ost_time_base, ist_time_base);
    }

    fn send_frame_to_encoder(&mut self, frame: &frame::Video) {
        self.encoder.send_frame(frame).unwrap();
    }

    fn send_eof_to_encoder(&mut self) {
        self.encoder.send_eof().unwrap();
    }

    fn receive_and_process_encoded_packets(
        &mut self,
        octx: &mut format::context::Output,
        ost_time_base: Rational,
        ist_time_base: Rational,
    ) {
        let mut encoded = Packet::empty();
        while self.encoder.receive_packet(&mut encoded).is_ok() {
            encoded.set_stream(self.ost_index);
            encoded.rescale_ts(ist_time_base, ost_time_base);
            encoded.write_interleaved(octx).unwrap();
        }
    }
}

fn parse_opts<'a>(s: String) -> Option<Dictionary<'a>> {
    let mut dict = Dictionary::new();
    for keyval in s.split_terminator(',') {
        let tokens: Vec<&str> = keyval.split('=').collect();
        match tokens[..] {
            [key, val] => dict.set(key, val),
            _ => return None,
        }
    }
    Some(dict)
}

struct AudioDecoder {
    decoder: decoder::Audio
}

impl AudioDecoder {
    fn send_packet_to_decoder(&mut self, packet: &Packet) {
        self.decoder.send_packet(packet).unwrap();
    }

    fn send_eof_to_decoder(&mut self) {
        self.decoder.send_eof().unwrap();
    }

    fn receive_and_process_decoded_frames(&mut self) {
        let mut frame = frame::Audio::empty();
        while self.decoder.receive_frame(&mut frame).is_ok() {
            let timestamp = frame.timestamp();
            self.log_progress(f64::from(
                Rational(timestamp.unwrap_or(0) as i32, 1) * self.decoder.time_base(),
            ));
        }
    }
    fn log_progress(&mut self, timestamp: f64) {
        eprintln!("timestamp: {:8.2}", timestamp);
    }
}

fn main() {
    let input_file = env::args().nth(1).expect("missing input file");
    let output_file = env::args().nth(2).expect("missing output file");
    let x264_opts = parse_opts(
        env::args()
            .nth(3)
            .unwrap_or_else(|| DEFAULT_X264_OPTS.to_string()),
    )
        .expect("invalid x264 options string");

    eprintln!("x264 options: {:?}", x264_opts);

    ffmpeg::init().unwrap();
    log::set_level(log::Level::Info);

    let mut ictx = format::input(&input_file).unwrap();
    let mut octx = format::output(&output_file).unwrap();

    format::context::input::dump(&ictx, 0, Some(&input_file));

    ///////////////////////////////
    let audio_decoder_stream = ictx.streams().best(media::Type::Audio).unwrap();
    let audio_decoder_stream_idx = audio_decoder_stream.index();
    let audio_decoder_stream_time_base = audio_decoder_stream.time_base();
    let mut audio_decoder = AudioDecoder { decoder: audio_decoder_stream.codec().decoder().audio().unwrap() };
    // VideoEncoder::new()
    //////////////////////////////

    // stream_mapping[ist_i] = ost_i
    let mut stream_mapping: Vec<isize> = vec![0; ictx.nb_streams() as _];
    let mut ist_time_bases = vec![Rational(0, 0); ictx.nb_streams() as _];
    let mut ost_time_bases = vec![Rational(0, 0); ictx.nb_streams() as _];

    let mut ost_index = 0;
    for (ist_index, ist) in ictx.streams().enumerate() {
        if ist.codec().medium() == media::Type::Video {
            stream_mapping[ist_index] = -1;
            continue;
        }
        stream_mapping[ist_index] = ost_index;
        ist_time_bases[ist_index] = ist.time_base();
        // Set up for stream copy for non-video stream.
        let mut ost = octx.add_stream(encoder::find(codec::Id::None)).unwrap();
        ost.set_parameters(ist.parameters());
        // We need to set codec_tag to 0 lest we run into incompatible codec tag
        // issues when muxing into a different container format. Unfortunately
        // there's no high level API to do this (yet).
        unsafe {
            (*ost.parameters().as_mut_ptr()).codec_tag = 0;
        }
        ost_index += 1;
    }

    octx.set_metadata(ictx.metadata().to_owned());
    format::context::output::dump(&octx, 0, Some(&output_file));
    octx.write_header().unwrap();

    for (ost_index, stream) in octx.streams().enumerate() {
        ost_time_bases[ost_index] = stream.time_base();
    }

    for (stream, mut packet) in ictx.packets() {
        let ist_index = stream.index();
        let ost_index = stream_mapping[ist_index];
        if ost_index < 0 {
            continue;
        }
        let ost_time_base = ost_time_bases[ost_index as usize];
        packet.rescale_ts(ist_time_bases[ist_index], ost_time_base);
        packet.set_position(-1);
        packet.set_stream(ost_index as _);
        packet.write_interleaved(&mut octx).unwrap();
        // if ist_index == audio_decoder_stream_idx {
        //     packet.rescale_ts(stream.time_base(), audio_decoder_stream_time_base);
        //     audio_decoder.send_packet_to_decoder(&packet);
        //     audio_decoder.receive_and_process_decoded_frames();
        // }
    }

    // Flush encoders and decoders.
    audio_decoder.send_eof_to_decoder();
    audio_decoder.receive_and_process_decoded_frames();
    // video_encoder.send_eof_to_encoder();
    // video_encoder.receive_and_process_encoded_packets(&mut octx, ost_time_base);

    octx.write_trailer().unwrap();
}
