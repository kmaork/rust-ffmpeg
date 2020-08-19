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

use std::collections::HashMap;
use std::{env, thread};
use std::time::Instant;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use ffmpeg::{codec, decoder, encoder, format, frame, log, media, Dictionary, Packet, Rational, Stream};
use ffmpeg::format::context;
use ffmpeg::frame::Audio;

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
            self.log_progress(&frame);
        }
    }
    fn log_progress(&mut self, frame: &Audio) {
        let ts = f64::from(
            Rational(frame.timestamp().unwrap_or(0) as i32, 1) * self.decoder.time_base(),
        );
        eprintln!("timestamp: {:8.2}, planes: {}, data: {:?}", ts, frame.planes(), &frame.data(0)[..10]);
    }

    fn decode(mut self, packet_receiver: Receiver<Packet>) {
        for packet in packet_receiver.iter() {
            self.send_packet_to_decoder(&packet);
            self.receive_and_process_decoded_frames();
        }
        self.send_eof_to_decoder();
        self.receive_and_process_decoded_frames();
    }
}

struct DumperStream {
    output_stream: usize,
    in_time_base: Rational,
    out_time_base: Rational,
    pkt_sender: SyncSender<Packet>,
}

impl DumperStream {
    fn send_pkt(&self, mut packet: Packet) {
        packet.rescale_ts(self.in_time_base, self.out_time_base);
        packet.set_stream(self.output_stream);
        self.pkt_sender.send(packet).unwrap();
    }
}

struct Dumper {
    octx: context::Output,
    pkt_sender: SyncSender<Packet>,
    pkt_receiver: Receiver<Packet>,
}

impl Dumper {
    fn new(ictx: &context::Input, output_file: String) -> Self {
        let mut octx = format::output(&output_file).unwrap();
        octx.set_metadata(ictx.metadata().to_owned());
        format::context::output::dump(&octx, 0, Some(&output_file));
        let (pkt_sender, pkt_receiver) = sync_channel(1000);
        Self { octx, pkt_sender, pkt_receiver }
    }
    fn add_stream(&mut self, ist: &Stream) -> DumperStream {
        // Set up for stream copy for non-video stream.
        let mut ost = self.octx.add_stream(encoder::find(codec::Id::None)).unwrap();
        ost.set_parameters(ist.parameters());
        ost.set_time_base(ist.time_base());
        // We need to set codec_tag to 0 lest we run into incompatible codec tag
        // issues when muxing into a different container format. Unfortunately
        // there's no high level API to do this (yet).
        unsafe { (*ost.parameters().as_mut_ptr()).codec_tag = 0; }
        DumperStream { output_stream: ost.index(), in_time_base: ist.time_base(), out_time_base: ost.time_base(), pkt_sender: self.pkt_sender.clone() }
    }
    fn dump(mut self) {
        drop(self.pkt_sender);
        self.octx.write_header().unwrap();
        for packet in self.pkt_receiver.iter() {
            packet.write_interleaved(&mut self.octx).unwrap();
        }
        self.octx.write_trailer().unwrap();
    }
}


fn main() {
    let input_file = env::args().nth(1).expect("missing input file");
    let output_file = env::args().nth(2).expect("missing output file");
    let x264_opts = parse_opts(
        env::args()
            .nth(3)
            .unwrap_or_else(|| DEFAULT_X264_OPTS.to_string()),
    ).expect("invalid x264 options string");

    eprintln!("x264 options: {:?}", x264_opts);

    ffmpeg::init().unwrap();
    log::set_level(log::Level::Info);

    let mut ictx = format::input(&input_file).unwrap();
    format::context::input::dump(&ictx, 0, Some(&input_file));
    let mut dumper = Dumper::new(&ictx, output_file.clone());

    ///////////////////////////////
    let audio_decoder_stream = ictx.streams().best(media::Type::Audio).unwrap();
    let audio_decoder_stream_idx = audio_decoder_stream.index();
    let mut audio_decoder = AudioDecoder { decoder: audio_decoder_stream.codec().decoder().audio().unwrap() };
    let (decoder_sender, decoder_receiver) = sync_channel(100);
    let j = thread::spawn(move || audio_decoder.decode(decoder_receiver));
    // VideoEncoder::new()
    //////////////////////////////
    let dumper_streams: HashMap<_, _> = ictx.streams()
        .filter(|ist| ist.codec().medium() != media::Type::Video)
        .map(|ist| (ist.index(), dumper.add_stream(&ist)))
        .collect();

    let j2 = { thread::spawn(move || dumper.dump()) };

    for (stream, mut packet) in ictx.packets() {
        let ist_index = stream.index();
        if let Some(dumper_stream) = dumper_streams.get(&ist_index) {
            packet.set_position(-1);
            if ist_index == audio_decoder_stream_idx {
                decoder_sender.send(packet.clone()).unwrap();
            }
            dumper_stream.send_pkt(packet);
        }
    }

    // video_encoder.send_eof_to_encoder();
    // video_encoder.receive_and_process_encoded_packets(&mut octx, ost_time_base);

    drop(decoder_sender);
    drop(dumper_streams);
    j.join().unwrap();
    j2.join().unwrap();
    println!("{}", output_file);
}
