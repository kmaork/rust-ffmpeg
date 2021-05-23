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

use ffmpeg::codec::Parameters;
use ffmpeg::format::context;
use ffmpeg::frame::Audio;
use ffmpeg::{
    codec, decoder, encoder, format, frame, log, media, software::scaling, Codec, Dictionary,
    Packet, Rational, Stream,
};
use std::collections::HashMap;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::{env, thread};

const DEFAULT_X264_OPTS: &str = "preset=medium";

struct VideoEncoderBuilder {
    dumper_stream_slot: DumperStreamSlot,
    encoder: encoder::Video,
}

impl VideoEncoderBuilder {
    fn new(
        width: u32,
        height: u32,
        frame_rate: Rational,
        dumper: &mut DumperBuilder,
        x264_opts: Dictionary,
    ) -> Result<Self, ffmpeg::Error> {
        let time_base = frame_rate.invert();
        let global_header = dumper.has_global_header();
        let (dumper_stream_slot, context) =
            dumper.add_video_encoder(encoder::find(codec::Id::H264).unwrap(), time_base);
        let mut encoder: encoder::video::Video = context.encoder().video()?;
        encoder.set_height(height);
        encoder.set_width(width);
        //encoder.set_aspect_ratio(decoder.aspect_ratio());
        encoder.set_format(format::Pixel::YUV420P);
        encoder.set_frame_rate(Some(frame_rate));
        encoder.set_time_base(time_base);
        if global_header {
            encoder.set_flags(codec::Flags::GLOBAL_HEADER);
        }
        let encoder = encoder
            .open_with(x264_opts)
            .expect("error opening libx264 encoder with supplied settings");
        Ok(Self {
            dumper_stream_slot,
            encoder,
        })
    }

    fn build(self, dumper: &Dumper) -> VideoEncoder {
        VideoEncoder {
            dumper_stream: dumper.link(self.dumper_stream_slot),
            encoder: self.encoder,
        }
    }
}

struct VideoEncoder {
    dumper_stream: DumperStream,
    encoder: encoder::Video,
}

impl VideoEncoder {
    fn gen_frame(&mut self, timestamp: i64) {
        let mut iframe = frame::Video::new(
            format::Pixel::RGB24,
            self.encoder.width(),
            self.encoder.height(),
        );
        let p: &mut [[u8; 3]] = iframe.plane_mut(0);
        for c in p {
            c[0] = 255;
            c[1] = 100;
            c[2] = 0;
        }
        let mut frame = frame::Video::new(
            self.encoder.format(),
            self.encoder.width(),
            self.encoder.height(),
        );
        scaling::Context::get(
            iframe.format(),
            iframe.width(),
            iframe.height(),
            frame.format(),
            frame.width(),
            frame.height(),
            scaling::Flags::empty(),
        )
        .unwrap()
        .run(&iframe, &mut frame)
        .unwrap();
        frame.set_pts(Some(timestamp));
        self.encoder.send_frame(&frame).unwrap();
        self.receive_and_process_encoded_packets();
    }

    fn receive_and_process_encoded_packets(&mut self) {
        let mut encoded = Packet::empty();
        while self.encoder.receive_packet(&mut encoded).is_ok() {
            self.dumper_stream.send_pkt(encoded.clone());
        }
    }

    fn gen_frames(mut self) {
        for i in 0..100 {
            self.gen_frame(i);
        }
        self.encoder.send_eof().unwrap();
        self.receive_and_process_encoded_packets();
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
    decoder: decoder::Audio,
    in_time_base: Rational,
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
        eprintln!(
            "timestamp: {:8.2}, planes: {}, data: {:?}",
            ts,
            frame.planes(),
            &frame.data(0)[..10]
        );
    }

    fn decode(mut self, packet_receiver: Receiver<Packet>) {
        for mut packet in packet_receiver.iter() {
            packet.rescale_ts(self.in_time_base, self.decoder.time_base());
            self.send_packet_to_decoder(&packet);
            self.receive_and_process_decoded_frames();
        }
        self.send_eof_to_decoder();
        self.receive_and_process_decoded_frames();
    }
}

struct DumperStreamSlot {
    output_stream: usize,
    in_time_base: Rational,
    pkt_sender: SyncSender<Packet>,
}

impl DumperStreamSlot {
    fn output_stream(&self) -> usize {
        self.output_stream
    }
}

struct DumperStream {
    dumper_stream_slot: DumperStreamSlot,
    out_time_base: Rational,
}

impl DumperStream {
    fn send_pkt(&self, mut packet: Packet) {
        packet.rescale_ts(self.dumper_stream_slot.in_time_base, self.out_time_base);
        packet.set_stream(self.dumper_stream_slot.output_stream);
        self.dumper_stream_slot.pkt_sender.send(packet).unwrap();
    }
}

struct DumperBuilder {
    octx: context::Output,
    pkt_sender: SyncSender<Packet>,
    pkt_receiver: Receiver<Packet>,
}

impl DumperBuilder {
    fn new(ictx: &context::Input, output_file: String) -> Self {
        let mut octx = format::output(&output_file).unwrap();
        octx.set_metadata(ictx.metadata().to_owned());
        format::context::output::dump(&octx, 0, Some(&output_file));
        let (pkt_sender, pkt_receiver) = sync_channel(1000);
        Self {
            octx,
            pkt_sender,
            pkt_receiver,
        }
    }
    fn add_stream(
        &mut self,
        codec: Option<Codec>,
        parameters: Parameters,
        in_time_base: Rational,
    ) -> DumperStreamSlot {
        // Set up for stream copy for non-video stream.
        let mut ost = self.octx.add_stream(codec).unwrap();
        ost.set_parameters(parameters);
        // We need to set codec_tag to 0 lest we run into incompatible codec tag
        // issues when muxing into a different container format. Unfortunately
        // there's no high level API to do this (yet).
        unsafe {
            (*ost.parameters().as_mut_ptr()).codec_tag = 0;
        }
        DumperStreamSlot {
            output_stream: ost.index(),
            in_time_base,
            pkt_sender: self.pkt_sender.clone(),
        }
    }
    fn add_stream_from_in_stream(&mut self, ist: &Stream) -> DumperStreamSlot {
        self.add_stream(
            encoder::find(codec::Id::None),
            ist.parameters(),
            ist.time_base(),
        )
    }
    fn add_video_encoder(
        &mut self,
        codec: Codec,
        time_base: Rational,
    ) -> (DumperStreamSlot, codec::Context) {
        let ost = self.octx.add_stream(Some(codec)).unwrap();
        let context = ost.codec(); //.encoder().video()?;
                                   //TODO: ost.set_parameters(encoder);
        (
            DumperStreamSlot {
                output_stream: ost.index(),
                in_time_base: time_base,
                pkt_sender: self.pkt_sender.clone(),
            },
            context,
        )
    }
    fn has_global_header(&self) -> bool {
        self.octx
            .format()
            .flags()
            .contains(format::Flags::GLOBAL_HEADER)
    }
    fn build(mut self) -> Dumper {
        self.octx.write_header().unwrap();
        Dumper {
            pkt_receiver: self.pkt_receiver,
            octx: self.octx,
        }
    }
}

struct Dumper {
    octx: context::Output,
    pkt_receiver: Receiver<Packet>,
}

impl Dumper {
    fn link(&self, dumper_stream_slot: DumperStreamSlot) -> DumperStream {
        let ost = dumper_stream_slot.output_stream();
        DumperStream {
            dumper_stream_slot,
            out_time_base: self.octx.stream(ost).unwrap().time_base(),
        }
    }
    fn dump(mut self) {
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
    )
    .expect("invalid x264 options string");

    eprintln!("x264 options: {:?}", x264_opts);

    ffmpeg::init().unwrap();
    log::set_level(log::Level::Info);

    let mut ictx = format::input(&input_file).unwrap();
    format::context::input::dump(&ictx, 0, Some(&input_file));
    let mut dumper_builder = DumperBuilder::new(&ictx, output_file.clone());

    let audio_decoder_stream = ictx.streams().best(media::Type::Audio).unwrap();
    let audio_decoder_stream_idx = audio_decoder_stream.index();
    let audio_decoder = AudioDecoder {
        in_time_base: audio_decoder_stream.time_base(),
        decoder: audio_decoder_stream.codec().decoder().audio().unwrap(),
    };
    let (decoder_sender, decoder_receiver) = sync_channel(100);
    let j = thread::spawn(move || audio_decoder.decode(decoder_receiver));
    let vb = VideoEncoderBuilder::new(700, 500, Rational(60, 1), &mut dumper_builder, x264_opts)
        .unwrap();

    let dumper_stream_slots: HashMap<_, _> = ictx
        .streams()
        .filter(|ist| ist.codec().medium() != media::Type::Video)
        .map(|ist| (ist.index(), dumper_builder.add_stream_from_in_stream(&ist)))
        .collect();
    let dumper = dumper_builder.build();
    let dumper_streams: HashMap<_, _> = dumper_stream_slots
        .into_iter()
        .map(|(ist, slot)| (ist, dumper.link(slot)))
        .collect();
    let v = vb.build(&dumper);

    let j2 = thread::spawn(move || dumper.dump());
    let j3 = thread::spawn(move || v.gen_frames());
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

    drop(decoder_sender);
    drop(dumper_streams);
    j.join().unwrap();
    j3.join().unwrap();
    j2.join().unwrap();
    println!("{}", output_file);
}

// todo: set time base and frame times accroding to decoder
