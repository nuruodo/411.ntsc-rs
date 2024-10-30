use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex, OnceLock},
};

use eframe::egui;
use gstreamer::prelude::*;
use gstreamer_video::{VideoFormat, VideoInterlaceMode};
use log::debug;
use snafu::ResultExt;

use crate::{
    app::render_settings::RenderPipelineSettings,
    gst_utils::{
        gstreamer_error::GstreamerError,
        ntsc_pipeline::{NtscPipeline, VideoElemMetadata, VideoScale},
        ntscrs_filter::NtscFilterSettings,
    },
};

use super::{
    error::{ApplicationError, CreatePipelineSnafu, CreateRenderJobSnafu},
    executor::AppTaskSpawner,
    render_settings::{
        Ffv1BitDepth, H264Settings, PngSettings, RenderInterlaceMode, RenderPipelineCodec,
        StillImageSettings,
    },
    NtscApp,
};

#[derive(Debug, Clone)]
pub enum RenderJobState {
    Waiting,
    Rendering,
    Paused,
    Complete { end_time: f64 },
    Error(Arc<GstreamerError>),
}

#[derive(Debug)]
pub struct RenderJob {
    pub settings: RenderPipelineSettings,
    pub pipeline: NtscPipeline,
    pub state: Arc<Mutex<RenderJobState>>,
    pub last_progress: f64,
    /// Used for estimating time remaining. A queue that holds (progress, timestamp) pairs.
    progress_samples: VecDeque<(f64, f64)>,
    pub start_time: Option<f64>,
    pause_time: Option<f64>,
    pub estimated_time_remaining: Option<f64>,
}

impl RenderJob {
    fn new(
        settings: RenderPipelineSettings,
        pipeline: NtscPipeline,
        state: Arc<Mutex<RenderJobState>>,
    ) -> Self {
        Self {
            settings,
            pipeline,
            state,
            last_progress: 0.0,
            progress_samples: VecDeque::new(),
            start_time: None,
            pause_time: None,
            estimated_time_remaining: None,
        }
    }

    pub fn create(
        executor: AppTaskSpawner,
        ctx: &egui::Context,
        src_path: &Path,
        settings: RenderPipelineSettings,
        still_image_settings: &StillImageSettings,
        scale: Option<VideoScale>,
    ) -> Result<Self, GstreamerError> {
        let src = gstreamer::ElementFactory::make("filesrc")
            .property("location", src_path)
            .build()?;

        let settings = Arc::new(settings);
        let settings_audio_closure = Arc::clone(&settings);
        let settings_video_closure = Arc::clone(&settings);
        let seek_to = match settings.codec_settings {
            RenderPipelineCodec::Png(PngSettings { seek_to, .. }) => Some(seek_to),
            _ => None,
        };

        let output_elems_cell = Arc::new(OnceLock::new());
        let output_elems_cell_video = Arc::clone(&output_elems_cell);
        let closure_settings = settings.clone();
        let create_output_elems = move |pipeline: &gstreamer::Pipeline| -> Result<
            (Option<gstreamer::Element>, gstreamer::Element),
            GstreamerError,
        > {
            let video_mux = match &closure_settings.codec_settings {
                RenderPipelineCodec::H264(_) => Some(
                    gstreamer::ElementFactory::make("mp4mux")
                        .name("output_muxer")
                        .build()?,
                ),
                RenderPipelineCodec::Ffv1(_) => Some(
                    gstreamer::ElementFactory::make("matroskamux")
                        .name("output_muxer")
                        .build()?,
                ),
                RenderPipelineCodec::Png(_) => None,
            };

            let file_sink = gstreamer::ElementFactory::make("filesink")
                .property("location", closure_settings.output_path.as_path())
                .build()?;

            pipeline.add(&file_sink)?;
            file_sink.sync_state_with_parent()?;

            if let Some(video_mux) = video_mux {
                pipeline.add(&video_mux)?;
                video_mux.link(&file_sink)?;
                video_mux.sync_state_with_parent()?;

                Ok((Some(video_mux.clone()), video_mux))
            } else {
                Ok((None, file_sink))
            }
        };

        let create_output_elems_audio = create_output_elems.clone();
        let create_output_elems_video = create_output_elems.clone();

        let job_state = Arc::new(Mutex::new(RenderJobState::Waiting));
        let job_state_for_handler = Arc::clone(&job_state);
        let exec = executor.clone();
        let exec_for_handler = executor.clone();
        let ctx_for_handler = ctx.clone();

        let is_png = matches!(settings.codec_settings, RenderPipelineCodec::Png(_));

        let pipeline = NtscPipeline::try_new(
            src,
            move |pipeline| {
                let (audio_out, _) = output_elems_cell
                    .get_or_init(|| create_output_elems_audio(pipeline))
                    .as_ref()
                    .map_err(|err| err.clone())?;
                if let Some(audio_out) = audio_out {
                    let audio_enc = match settings_audio_closure.codec_settings {
                        RenderPipelineCodec::H264(_) => {
                            gstreamer::ElementFactory::make("avenc_aac").build()?
                        }
                        RenderPipelineCodec::Ffv1(_) => {
                            gstreamer::ElementFactory::make("flacenc").build()?
                        }
                        RenderPipelineCodec::Png(_) => return Ok(None),
                    };

                    pipeline.add(&audio_enc)?;
                    audio_enc.link(audio_out)?;
                    audio_enc.sync_state_with_parent()?;
                    Ok(Some(audio_enc))
                } else {
                    Ok(None)
                }
            },
            move |pipeline, VideoElemMetadata { interlace_mode, .. }| {
                let (_, video_out) = output_elems_cell_video
                    .get_or_init(|| create_output_elems_video(pipeline))
                    .as_ref()
                    .map_err(|err| err.clone())?;

                let (video_enc, pixel_formats) = match &settings_video_closure.codec_settings {
                    RenderPipelineCodec::H264(h264_settings) => {
                        // Load the x264enc plugin so the enum classes exist. Nothing seems to work except actually instantiating an Element.
                        let _ = gstreamer::ElementFactory::make("x264enc").build().unwrap();
                        #[allow(non_snake_case)]
                        let GstX264EncPass = gstreamer::glib::EnumClass::with_type(
                            gstreamer::glib::Type::from_name("GstX264EncPass").unwrap(),
                        )
                        .unwrap();
                        #[allow(non_snake_case)]
                        let GstX264EncPreset = gstreamer::glib::EnumClass::with_type(
                            gstreamer::glib::Type::from_name("GstX264EncPreset").unwrap(),
                        )
                        .unwrap();

                        let video_enc = gstreamer::ElementFactory::make("x264enc")
                            // CRF mode
                            .property("pass", GstX264EncPass.to_value_by_nick("quant").unwrap())
                            // invert CRF (so that low numbers = low quality)
                            .property("quantizer", 50 - h264_settings.crf as u32)
                            .property(
                                "speed-preset",
                                GstX264EncPreset
                                    .to_value(9 - h264_settings.encode_speed as i32)
                                    .unwrap(),
                            )
                            .build()?;

                        let pixel_formats = Self::pixel_formats_for(
                            if h264_settings.ten_bit { 10 } else { 8 },
                            h264_settings.chroma_subsampling,
                        );

                        (video_enc, pixel_formats)
                    }
                    RenderPipelineCodec::Ffv1(ffv1_settings) => {
                        // Load the plugin so the avcodeccontext-threads enum class exists
                        let _ = gstreamer::ElementFactory::make("avenc_ffv1")
                            .build()
                            .unwrap();
                        let avcodeccontext_threads = gstreamer::glib::EnumClass::with_type(
                            gstreamer::glib::Type::from_name("avcodeccontext-threads").unwrap(),
                        )
                        .unwrap();

                        let video_enc = gstreamer::ElementFactory::make("avenc_ffv1")
                            // Enable multithreaded encoding (0 means "auto-detect number of threads")
                            .property("threads", avcodeccontext_threads.to_value(0).unwrap())
                            // 16 slices (improves multithreading capability)
                            .property("slices", 16i32)
                            .build()?;

                        let pixel_formats = Self::pixel_formats_for(
                            match ffv1_settings.bit_depth {
                                Ffv1BitDepth::Bits8 => 8,
                                Ffv1BitDepth::Bits10 => 10,
                                Ffv1BitDepth::Bits12 => 12,
                            },
                            ffv1_settings.chroma_subsampling,
                        );

                        (video_enc, pixel_formats)
                    }
                    RenderPipelineCodec::Png(_) => {
                        let video_enc = gstreamer::ElementFactory::make("pngenc")
                            .property("snapshot", true)
                            .build()?;

                        let pixel_formats: &[VideoFormat] = &[VideoFormat::Rgb];

                        (video_enc, pixel_formats)
                    }
                };

                let mut elems = Vec::<gstreamer::Element>::new();

                let video_ntsc = gstreamer::ElementFactory::make("ntscfilter")
                    .property(
                        "settings",
                        NtscFilterSettings(settings_video_closure.effect_settings.clone()),
                    )
                    .build()?;
                elems.push(video_ntsc.clone());

                // libx264 can't encode 4:2:0 subsampled videos with odd dimensions. Pad them out to even dimensions.
                if let RenderPipelineCodec::H264(H264Settings {
                    chroma_subsampling: true,
                    ..
                }) = &settings_video_closure.codec_settings
                {
                    let video_padding =
                        gstreamer::ElementFactory::make("videopadfilter").build()?;
                    elems.push(video_padding);
                }

                let ntsc_caps_filter = gstreamer::ElementFactory::make("capsfilter")
                    .property(
                        "caps",
                        gstreamer_video::VideoCapsBuilder::new()
                            .format(gstreamer_video::VideoFormat::Argb64)
                            .build(),
                    )
                    .build()?;
                elems.push(ntsc_caps_filter);

                let video_convert = gstreamer::ElementFactory::make("videoconvert").build()?;
                elems.push(video_convert);

                if settings_video_closure.interlacing != RenderInterlaceMode::Progressive
                    && !matches!(interlace_mode, Some(VideoInterlaceMode::Progressive))
                {
                    // Load the interlace plugin so the enum class exists. Nothing seems to work except actually instantiating an Element.
                    let _ = gstreamer::ElementFactory::make("interlace")
                        .build()
                        .unwrap();
                    #[allow(non_snake_case)]
                    let GstInterlacePattern = gstreamer::glib::EnumClass::with_type(
                        gstreamer::glib::Type::from_name("GstInterlacePattern").unwrap(),
                    )
                    .unwrap();

                    let interlace = gstreamer::ElementFactory::make("interlace")
                        .property(
                            "field-pattern",
                            GstInterlacePattern.to_value_by_nick("2:2").unwrap(),
                        )
                        .property(
                            "top-field-first",
                            settings_video_closure.interlacing
                                == RenderInterlaceMode::TopFieldFirst,
                        )
                        .build()?;
                    elems.push(interlace);
                }

                let video_caps = gstreamer_video::VideoCapsBuilder::new()
                    .format_list(pixel_formats.iter().copied())
                    .build();
                let caps_filter = gstreamer::ElementFactory::make("capsfilter")
                    .property("caps", &video_caps)
                    .build()?;
                elems.push(caps_filter);

                elems.push(video_enc.clone());

                pipeline.add_many(elems.iter())?;
                gstreamer::Element::link_many(elems.iter())?;

                video_enc.link(video_out)?;

                for elem in elems.iter() {
                    elem.sync_state_with_parent()?;
                }
                video_enc.sync_state_with_parent()?;

                Ok(video_ntsc)
            },
            move |bus, msg| {
                let job_state = &job_state_for_handler;
                let exec = &exec;
                let ctx = &ctx_for_handler;

                let handle_msg = move |_bus, msg: &gstreamer::Message| -> Option<()> {
                    debug!("{:?}", msg);
                    let src = msg.src()?;

                    if let gstreamer::MessageView::Error(err) = msg.view() {
                        let mut job_state = job_state.lock().unwrap();
                        if !matches!(*job_state, RenderJobState::Error(_)) {
                            *job_state = RenderJobState::Error(Arc::new(err.error().into()));
                            ctx.request_repaint();
                        }
                    }

                    // Make sure we're listening to a pipeline event
                    if let Some(pipeline) = src.downcast_ref::<gstreamer::Pipeline>() {
                        let pipeline_for_handler = pipeline.clone();
                        if let gstreamer::MessageView::Eos(_) = msg.view() {
                            let job_state_inner = Arc::clone(job_state);
                            let end_time = ctx.input(|input| input.time);
                            exec.spawn(async move {
                                let _ = pipeline_for_handler.set_state(gstreamer::State::Null);
                                *job_state_inner.lock().unwrap() =
                                    RenderJobState::Complete { end_time };
                                None
                            })
                        }

                        if let gstreamer::MessageView::StateChanged(state_changed) = msg.view() {
                            if state_changed.pending() == gstreamer::State::Null {
                                let end_time = ctx.input(|input| input.time);
                                *job_state.lock().unwrap() = RenderJobState::Complete { end_time };
                            } else {
                                *job_state.lock().unwrap() = match state_changed.current() {
                                    gstreamer::State::Paused => RenderJobState::Paused,
                                    gstreamer::State::Playing => RenderJobState::Rendering,
                                    gstreamer::State::Ready => RenderJobState::Waiting,
                                    gstreamer::State::Null => {
                                        let end_time = ctx.input(|input| input.time);
                                        RenderJobState::Complete { end_time }
                                    }
                                    gstreamer::State::VoidPending => {
                                        unreachable!("current state should never be VOID_PENDING")
                                    }
                                };
                            }
                            ctx.request_repaint();
                        }
                    }

                    Some(())
                };

                handle_msg(bus, msg);

                gstreamer::BusSyncReply::Drop
            },
            if is_png {
                None
            } else {
                Some(still_image_settings.duration)
            },
            scale,
            still_image_settings.framerate,
            Some(move |p: Result<gstreamer::Pipeline, _>| {
                exec_for_handler.spawn(async move {
                    Some(
                        Box::new(move |_: &mut NtscApp| -> Result<(), ApplicationError> {
                            let pipeline = p.context(CreatePipelineSnafu)?;

                            if let Some(seek_to) = seek_to {
                                pipeline
                                    .seek_simple(
                                        gstreamer::SeekFlags::FLUSH
                                            | gstreamer::SeekFlags::ACCURATE,
                                        seek_to,
                                    )
                                    .map_err(|e| e.into())
                                    .context(CreateRenderJobSnafu)?;
                            }

                            pipeline
                                .set_state(gstreamer::State::Playing)
                                .map_err(|e| e.into())
                                .context(CreateRenderJobSnafu)?;
                            Ok(())
                        }) as _,
                    )
                });
            }),
        )?;

        pipeline.set_state(gstreamer::State::Paused)?;

        Ok(RenderJob::new(
            settings.as_ref().clone(),
            pipeline,
            job_state,
        ))
    }

    fn pixel_formats_for(bit_depth: usize, chroma_subsampling: bool) -> &'static [VideoFormat] {
        match (bit_depth, chroma_subsampling) {
            (8, false) => &[
                VideoFormat::Y444,
                VideoFormat::V308,
                VideoFormat::Iyu2,
                VideoFormat::Nv24,
            ],
            (8, true) => &[
                VideoFormat::I420,
                VideoFormat::Yv12,
                VideoFormat::Nv12,
                VideoFormat::Nv21,
            ],
            (10, false) => &[VideoFormat::Y44410be, VideoFormat::Y44410le],
            (10, true) => &[VideoFormat::I42010be, VideoFormat::I42010le],
            (12, false) => &[VideoFormat::Y44412be, VideoFormat::Y44412le],
            (12, true) => &[VideoFormat::I42012be, VideoFormat::I42012le],
            _ => panic!("No pixel format for bit depth {bit_depth}"),
        }
    }

    pub fn update_estimated_time_remaining(&mut self, progress: f64, current_time: f64) {
        const NUM_PROGRESS_SAMPLES: usize = 5;
        const PROGRESS_SAMPLE_TIME_DELTA: f64 = 1.0;

        let most_recent_sample = self.progress_samples.back().copied();
        let should_update_estimate = if let Some((_, sample_time)) = most_recent_sample {
            current_time - sample_time > PROGRESS_SAMPLE_TIME_DELTA
        } else {
            true
        };
        if should_update_estimate {
            if self.start_time.is_none() {
                self.start_time = Some(current_time);
            }
            let new_sample = (progress, current_time);
            let oldest_sample = if self.progress_samples.len() >= NUM_PROGRESS_SAMPLES {
                self.progress_samples.pop_front()
            } else {
                self.progress_samples.front().copied()
            };
            self.progress_samples.push_back(new_sample);
            if let Some((old_progress, old_sample_time)) = oldest_sample {
                let time_estimate = (current_time - old_sample_time) / (progress - old_progress)
                    + self.start_time.unwrap();
                if time_estimate.is_finite() {
                    self.estimated_time_remaining = Some((time_estimate - current_time).max(0.0));
                }
            }
        }
    }

    pub fn set_pause_time(&mut self, time: f64) {
        self.pause_time = Some(time);
    }

    pub fn resume_at_time(&mut self, time: f64) {
        let pause_time = self.pause_time.unwrap_or_default();
        let time_paused = time - pause_time;
        self.start_time = Some(self.start_time.unwrap_or_default() + time_paused);
        for (_, timestamp) in &mut self.progress_samples {
            *timestamp += time_paused;
        }
    }
}

impl Drop for RenderJob {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gstreamer::State::Null);
    }
}
