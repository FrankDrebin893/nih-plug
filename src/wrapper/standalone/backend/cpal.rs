use anyhow::{Context, Result};
use cpal::{
    traits::*, Device, FromSample, InputCallbackInfo, OutputCallbackInfo, Sample, SampleFormat,
    Stream, StreamConfig,
};
use crossbeam::sync::{Parker, Unparker};
use midir::{
    MidiInput, MidiInputConnection, MidiInputPort, MidiOutput, MidiOutputConnection, MidiOutputPort,
};
use parking_lot::Mutex;
use rtrb::RingBuffer;
use std::borrow::Borrow;
use std::num::NonZeroU32;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::ScopedJoinHandle;

use super::super::config::WrapperConfig;
use super::Backend;
use crate::midi::MidiResult;
use crate::prelude::{
    AudioIOLayout, AuxiliaryBuffers, Buffer, MidiConfig, NoteEvent, Plugin, PluginNoteEvent,
    Transport,
};
use crate::wrapper::util::buffer_management::{BufferManager, ChannelPointers};

/// TEMPORARY, to find a hang on a change of devices: a timestamped trace line (stderr and file)
macro_rules! rtrace {
    ($($t:tt)*) => { super::super::trace(format_args!($($t)*)) };
}

fn device_name(device: &Device) -> String {
    device.name().unwrap_or_else(|_| "?".into())
}

const MIDI_EVENT_QUEUE_CAPACITY: usize = 2048;

/// An ASIO stream takes over every channel it opens, so it opens only the ones the plugin uses
/// (the others stay for other programs, as a DAW leaves them); other drivers keep the whole
/// device's channels
fn asio_channels(host: cpal::HostId, all: cpal::ChannelCount, used: usize) -> cpal::ChannelCount {
    #[cfg(feature = "asio")]
    if host == cpal::HostId::Asio {
        return (used.max(1) as cpal::ChannelCount).min(all);
    }
    let _ = (host, used);
    all
}

/// Opens the devices a configuration names, with the stream configuration each takes. The input is
/// `None` unless one is named, to keep a microphone from feeding back
fn open_devices(
    config: &mut WrapperConfig,
    audio_io_layout: &AudioIOLayout,
    cpal_host_id: cpal::HostId,
) -> Result<(Option<CpalDevice>, CpalDevice)> {
    let host = cpal::host_from_id(cpal_host_id).context("The Audio API is unavailable")?;
    let audio_io_layout = *audio_io_layout;

    // No input device is connected unless requested by the user to avoid feedback loops
    let input_device = config
        .input_device
        .as_ref()
        .map(|name| -> Result<Device> {
            let device = host
                .input_devices()
                .context("No audio input devices available")?
                // `.name()` returns a `Result` with a non-Eq error type so you can't compare this
                // directly
                .find(|d| d.name().as_deref().map(|n| n == name).unwrap_or(false))
                .with_context(|| {
                    // This is a bit awkward, but instead of adding a dedicated option we'll just
                    // list all of the available devices in the error message when the chosen device
                    // does not exist
                    let mut message =
                        format!("Unknown input device '{name}'. Available devices are:");
                    for device_name in host.input_devices().unwrap().flat_map(|d| d.name()) {
                        message.push_str(&format!("\n{device_name}"))
                    }

                    message
                })?;

            Ok(device)
        })
        .transpose()?;

    let output_device = match config.output_device.as_ref() {
        Some(name) => host
            .output_devices()
            .context("No audio output devices available")?
            .find(|d| d.name().as_deref().map(|n| n == name).unwrap_or(false))
            .with_context(|| {
                let mut message = format!("Unknown output device '{name}'. Available devices are:");
                for device_name in host.output_devices().unwrap().flat_map(|d| d.name()) {
                    message.push_str(&format!("\n{device_name}"))
                }

                message
            })?,
        None => host
            .default_output_device()
            .context("No default audio output device available")?,
    };

    // An ASIO driver is opened once and takes one set of buffers for its inputs and its
    // outputs together. Two `Device`s for the same driver each prepare buffers of their own,
    // and the second takes the first's away, so the input never delivers a block. With one
    // driver for both, input and output share one `Device`
    #[cfg(feature = "asio")]
    let output_device = match (&input_device, cpal_host_id) {
        (Some(input), cpal::HostId::Asio)
            if input.name().ok().is_some() && input.name().ok() == output_device.name().ok() =>
        {
            input.clone()
        }
        _ => output_device,
    };

    // The channels are counted from 1 on the command line, from 0 here
    let input_first_channel = config.input_channel.saturating_sub(1) as usize;
    let output_first_channel = config.output_channel.saturating_sub(1) as usize;
    let requested_sample_rate = cpal::SampleRate(config.sample_rate as u32);
    // An ASIO driver has the buffer size it was set to in its own control panel (a Focusrite
    // reports 64 as both its smallest and its largest while it is set to 64), so the size
    // asked for is brought into the driver's range instead of failing to start
    #[cfg(feature = "asio")]
    if cpal_host_id == cpal::HostId::Asio {
        if let Some(cpal::SupportedBufferSize::Range { min, max }) = output_device
            .supported_output_configs()
            .ok()
            .and_then(|mut configs| configs.next())
            .map(|c| *c.buffer_size())
        {
            let size = config.period_size.clamp(min, max);
            if size != config.period_size {
                nih_log!(
                        "The ASIO driver takes buffers of {min} to {max} samples: using {size}                          instead of {}",
                        config.period_size
                    );
                config.period_size = size;
            }
        }
    }
    let requested_buffer_size = cpal::BufferSize::Fixed(config.period_size);
    let num_input_channels = audio_io_layout
        .main_input_channels
        .map(NonZeroU32::get)
        .unwrap_or_default() as usize;
    let input = input_device
        .map(|device| -> Result<CpalDevice> {
            let mut input_configs: Vec<_> = device
                .supported_input_configs()
                .context("Could not get supported audio input configurations")?
                .filter(|c| match c.buffer_size() {
                    cpal::SupportedBufferSize::Range { min, max } => {
                        c.channels() as usize >= input_first_channel + num_input_channels
                            && (c.min_sample_rate()..=c.max_sample_rate())
                                .contains(&requested_sample_rate)
                            && (min..=max).contains(&&config.period_size)
                    }
                    cpal::SupportedBufferSize::Unknown => false,
                })
                .collect();
            // The fewest channels that reach the chosen ones
            input_configs.sort_by_key(|c| c.channels());
            let input_config_range = input_configs
                .iter()
                // Prefer floating point samples to avoid conversions
                .find(|c| {
                    c.sample_format() == SampleFormat::F32
                        && c.channels() == input_configs[0].channels()
                })
                .or_else(|| input_configs.first())
                .cloned()
                .with_context(|| {
                    format!(
                        "The audio input device does not have {} audio channels from \
                             channel {} at a sample rate of {} Hz and a period size of {} samples",
                        num_input_channels,
                        config.input_channel,
                        config.sample_rate,
                        config.period_size,
                    )
                })?;

            // We already checked that these settings are valid
            let input_config = StreamConfig {
                channels: asio_channels(
                    cpal_host_id,
                    input_config_range.channels(),
                    input_first_channel + num_input_channels,
                ),
                sample_rate: requested_sample_rate,
                buffer_size: requested_buffer_size,
            };
            let input_sample_format = input_config_range.sample_format();

            Ok(CpalDevice {
                device,
                config: input_config,
                sample_format: input_sample_format,
            })
        })
        .transpose()?;

    let num_output_channels = audio_io_layout
        .main_output_channels
        .map(NonZeroU32::get)
        .unwrap_or_default() as usize;
    let output = {
        let mut output_configs: Vec<_> = output_device
            .supported_output_configs()
            .context("Could not get supported audio output configurations")?
            .filter(|c| match c.buffer_size() {
                cpal::SupportedBufferSize::Range { min, max } => {
                    c.channels() as usize >= output_first_channel + num_output_channels
                        && (c.min_sample_rate()..=c.max_sample_rate())
                            .contains(&requested_sample_rate)
                        && (min..=max).contains(&&config.period_size)
                }
                cpal::SupportedBufferSize::Unknown => false,
            })
            .collect();
        output_configs.sort_by_key(|c| c.channels());
        let output_config_range = output_configs
            .iter()
            .find(|c| {
                c.sample_format() == SampleFormat::F32
                    && c.channels() == output_configs[0].channels()
            })
            .or_else(|| output_configs.first())
            .cloned()
            .with_context(|| {
                format!(
                    "The audio output device does not have {} audio channels from channel {} \
                         at a sample rate of {} Hz and a period size of {} samples",
                    num_output_channels,
                    config.output_channel,
                    config.sample_rate,
                    config.period_size,
                )
            })?;
        let output_config = StreamConfig {
            channels: asio_channels(
                cpal_host_id,
                output_config_range.channels(),
                output_first_channel + num_output_channels,
            ),
            sample_rate: requested_sample_rate,
            buffer_size: requested_buffer_size,
        };
        let output_sample_format = output_config_range.sample_format();

        CpalDevice {
            device: output_device,
            config: output_config,
            sample_format: output_sample_format,
        }
    };

    Ok((input, output))
}

/// Uses CPAL for audio and midir for MIDI.
pub struct CpalMidir {
    config: WrapperConfig,
    audio_io_layout: AudioIOLayout,
    host_id: cpal::HostId,

    input: Option<CpalDevice>,
    output: CpalDevice,

    midi_input: Mutex<Option<MidirInputDevice>>,
    midi_output: Mutex<Option<MidirOutputDevice>>,

    /// The capture and playback callbacks run one after the other on the driver's thread
    /// (ASIO), so the capture callback must never wait for the playback one
    one_callback_thread: bool,
}

/// All data needed for a CPAL input or output stream.
struct CpalDevice {
    pub device: Device,
    pub config: StreamConfig,
    pub sample_format: SampleFormat,
}

/// All data needed to create a Midir input stream.
struct MidirInputDevice {
    pub backend: MidiInput,
    pub port: MidiInputPort,
}

/// An active `MidirInputDevice`. Transformed back and from this during the `.run()` function.
struct ActiveMidirInputDevice {
    pub connection: MidiInputConnection<()>,
    pub port: MidiInputPort,
}

/// All data needed to create a Midir output stream.
struct MidirOutputDevice {
    pub backend: MidiOutput,
    pub port: MidiOutputPort,
}

/// An active `MidirOutputDevice`. Transformed back and from this during the `.run()` function.
struct ActiveMidirOutputDevice {
    pub connection: MidiOutputConnection,
    pub port: MidiOutputPort,
}

/// Send+Sync wrapper for `Vec<*mut f32>` so we can preallocate channel pointer vectors for use with
/// the `BufferManager` API.
struct ChannelPointerVec(Vec<*mut f32>);

unsafe impl Send for ChannelPointerVec {}
unsafe impl Sync for ChannelPointerVec {}

impl ChannelPointerVec {
    // If you directly access the `.0` field then it will try to move it out of the struct which
    // undoes the Send+Sync impl.
    pub fn get(&mut self) -> &mut Vec<*mut f32> {
        &mut self.0
    }
}

/// A task for the MIDI output thread.
enum MidiOutputTask<P: Plugin> {
    /// Send an event as MIDI data.
    Send(PluginNoteEvent<P>),
    /// Terminate the thread, stopping it from blocking and allowing it to be joined.
    Terminate,
}

impl<P: Plugin> Backend<P> for CpalMidir {
    fn run(
        &mut self,
        cb: impl FnMut(
                &mut Buffer,
                &mut AuxiliaryBuffers,
                Transport,
                &[PluginNoteEvent<P>],
                &mut Vec<PluginNoteEvent<P>>,
            ) -> bool
            + 'static
            + Send,
    ) {
        // The process callback outlives one set of streams: a request to change the devices
        // (`nih_standalone_reconfigure()`) closes the streams, opens the new ones and gives them
        // the same callback. Only an output callback calls it, and only one set of streams
        // exists at a time, so the lock is never contended
        let cb = Arc::new(Mutex::new(cb));
        rtrace!(
            "run: start, host {:?}, input {:?}, output {}",
            self.host_id,
            self.input.as_ref().map(|d| device_name(&d.device)),
            device_name(&self.output.device)
        );
        let parker = Parker::new();
        super::super::set_reconfigure_waker(Some(parker.unparker().clone()));

        let mut trying: Option<Trying> = None;
        let mut retries = 0;
        let mut opened_once = false;
        loop {
            rtrace!(
                "run: opening streams (opened before: {opened_once}, change in progress: {})",
                trying.is_some()
            );
            match self.run_once::<P, _>(&cb, &parker, &mut trying) {
                Outcome::Finished => {
                    rtrace!("run: the streams finished");
                    break;
                }
                Outcome::Reconfigure(request) => {
                    rtrace!("run: streams closed for a request: {:?}", request.devices);
                    opened_once = true;
                    retries = 0;
                    let mut config = self.config.clone();
                    config.input_device = request.devices.input_device.clone();
                    config.output_device = request.devices.output_device.clone();
                    config.input_channel = request.devices.input_channel.max(1);
                    config.output_channel = request.devices.output_channel.max(1);
                    rtrace!("run: resolving the new devices");
                    match open_devices(&mut config, &self.audio_io_layout, self.host_id) {
                        Ok((input, output)) => {
                            rtrace!(
                                "run: devices resolved, input {:?}, output {}",
                                input.as_ref().map(|d| device_name(&d.device)),
                                device_name(&output.device)
                            );
                            let saved = Saved {
                                config: std::mem::replace(&mut self.config, config),
                                input: std::mem::replace(&mut self.input, input),
                                output: std::mem::replace(&mut self.output, output),
                            };
                            trying = Some(Trying {
                                reply: request.reply,
                                saved: Some(saved),
                                result: Ok(()),
                            });
                        }
                        // The old devices have been let go, so they are opened again
                        Err(err) => {
                            rtrace!("run: the new devices could not be resolved: {err:#}");
                            let _ = request.reply.send(Err(format!("{err:#}")));
                        }
                    }
                }
                Outcome::OpenFailed(err, worth_retrying) => {
                    rtrace!(
                        "run: open failed ({err:#}), worth retrying {worth_retrying}, retry {retries}"
                    );
                    if !opened_once {
                        super::super::set_reconfigure_waker(None);
                        panic!("Fatal error opening the audio streams: {err:#}");
                    }
                    nih_error!("Could not open the audio streams: {err:#}");
                    // An ASIO driver may need a moment to let go of what it had before
                    if worth_retrying && retries < OPEN_RETRIES {
                        retries += 1;
                        std::thread::sleep(OPEN_RETRY_WAIT);
                        continue;
                    }
                    retries = 0;
                    match trying.take() {
                        // The new devices will not open: the old ones are opened again, and the
                        // answer is the error once that has worked
                        Some(Trying {
                            reply,
                            saved: Some(saved),
                            ..
                        }) => {
                            rtrace!("run: the new devices failed, the old ones are opened again");
                            self.config = saved.config;
                            self.input = saved.input;
                            self.output = saved.output;
                            trying = Some(Trying {
                                reply,
                                saved: None,
                                result: Err(format!("{err:#}")),
                            });
                        }
                        // The old ones will not open either: say so, and end, instead of
                        // going silent without a word
                        Some(Trying {
                            reply,
                            saved: None,
                            result,
                        }) => {
                            rtrace!("run: the old devices failed too");
                            let first = result.err().unwrap_or_default();
                            let _ = reply.send(Err(format!(
                                "{first}; the old devices would not open again either: {err:#}"
                            )));
                            break;
                        }
                        None => break,
                    }
                }
            }
        }
        rtrace!("run: end");
        super::super::set_reconfigure_waker(None);
    }
}

/// How long a change of devices waits for the new capture device's first block
const CAPTURE_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// How often and how long an opening is tried again after a change of devices
const OPEN_RETRIES: u32 = 10;
const OPEN_RETRY_WAIT: std::time::Duration = std::time::Duration::from_millis(100);

/// What a run of one set of streams ended with
enum Outcome {
    /// The plugin or the driver ended it
    Finished,
    /// Somebody asked for other devices; this set of streams is closed
    Reconfigure(super::super::ReconfigureRequest),
    /// The streams would not open
    /// The streams would not open. Whether a short wait and another try may help: not for a
    /// capture device that is open but delivers nothing, which took seconds to find out
    OpenFailed(anyhow::Error, bool),
}

/// The old devices of a change in progress, kept to be opened again if the new ones fail
struct Saved {
    config: WrapperConfig,
    input: Option<CpalDevice>,
    output: CpalDevice,
}

/// A change of devices whose answer has not been sent yet. It is sent once streams are open
/// again: `result` is what it says then (`Ok` for the new devices; an error when the new ones
/// failed and the old ones are being opened again). `saved` holds the old devices while the new
/// ones are tried
struct Trying {
    reply: std::sync::mpsc::Sender<Result<(), String>>,
    saved: Option<Saved>,
    result: Result<(), String>,
}

/// Sets the flag when dropped
struct CloseOnDrop(Arc<AtomicBool>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Ends the wait of `run` for good, as opposed to waking it for a request
#[derive(Clone)]
struct Wake {
    stopped: Arc<AtomicBool>,
    /// Set before any stream is dropped. A callback that waits for the other (a full ring buffer,
    /// an empty one) stops waiting then, since dropping a stream waits for its callback to return
    closing: Arc<AtomicBool>,
    unparker: Unparker,
}

impl Wake {
    fn unpark(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.unparker.unpark();
    }
}

impl CpalMidir {
    /// Opens the streams, plays until the plugin or the driver ends it or somebody asks for other
    /// devices, and closes everything it opened
    fn run_once<P, C>(
        &mut self,
        cb: &Arc<Mutex<C>>,
        parker: &Parker,
        trying: &mut Option<Trying>,
    ) -> Outcome
    where
        P: Plugin,
        C: FnMut(
                &mut Buffer,
                &mut AuxiliaryBuffers,
                Transport,
                &[PluginNoteEvent<P>],
                &mut Vec<PluginNoteEvent<P>>,
            ) -> bool
            + 'static
            + Send,
    {
        // So this is a lot of fun. There are up to four separate streams here, all using their own
        // callbacks. The audio output stream acts as the primary stream, and everything else either
        // sends data to it or (in the case of the MIDI output stream) receives data from it using
        // channels.
        //
        // Audio input is read from the input device (if configured), and is send at a period at a
        // time to the output stream in an interleaved format. Because of that the audio output
        // stream is delayed for one period using a parker to you don't immediately get xruns. CPAL
        // audio devices may also not accept floating point samples, so all of the actual audio
        // handling and buffer management handles in the `build_*_data_callback()` functions defined
        // below.
        //
        // MIDI input is parsed in the Midir callback and the events are sent over a callback to the
        // output audio thread where the process callback happens. If that process callback outputs
        // events then those are sent over another ringbuffer to a thread that handles MIDI output.
        // Both MIDI input and MIDI output are disabled by default.
        //
        // The thread scope is needed to accomodate the midir MIDI output API. Outputting MIDI is
        // realtime unsafe, and to be able to output MIDI with midir you need to transform between
        // `MidiOutputPort` and `MidiOutputPortConnection` types by taking values out of an
        // `Option`.
        let closing = Arc::new(AtomicBool::new(false));
        std::thread::scope(|s| -> Outcome {
            let mut _input_stream: Option<Stream> = None;
            // Declared after the stream, so dropped before it: however this scope ends (an early
            // return included), `closing` is set before the capture stream is dropped
            let _close_on_drop = CloseOnDrop(closing.clone());
            let mut input_rb_consumer: Option<rtrb::Consumer<f32>> = None;
            if let Some(input) = &self.input {
                // Data is sent to the output data callback using a wait-free ring buffer
                // Only the channels the plugin reads go through it
                let (rb_producer, rb_consumer) = RingBuffer::new(
                    num_input_channels_of(&self.audio_io_layout).max(1)
                        * self.config.period_size as usize,
                );
                input_rb_consumer = Some(rb_consumer);

                let input_parker = Parker::new();
                let input_unparker = input_parker.unparker().clone();
                let error_cb = {
                    let input_unparker = input_unparker.clone();
                    move |err| {
                        nih_error!("Error during capture: {err:#}");
                        input_unparker.clone().unpark();
                    }
                };

                macro_rules! build_input_streams {
                    ($sample_format:expr, $(($format:path, $primitive_type:ty)),*) => {
                        match $sample_format {
                            $($format => input.device.build_input_stream(
                                &input.config,
                                self.build_input_data_callback::<$primitive_type>(
                                    input_unparker,
                                    rb_producer,
                                    input.config.channels as usize,
                                    closing.clone(),
                                ),
                                error_cb,
                                None,
                            ),)*
                            format => todo!("Unsupported sample format {format}"),
                        }
                    }
                }
                rtrace!(
                    "capture: building a stream on {} ({:?}, {:?})",
                    device_name(&input.device),
                    input.config,
                    input.sample_format
                );
                let stream = build_input_streams!(
                    input.sample_format,
                    (SampleFormat::I8, i8),
                    (SampleFormat::I16, i16),
                    (SampleFormat::I32, i32),
                    (SampleFormat::I64, i64),
                    (SampleFormat::U8, u8),
                    (SampleFormat::U16, u16),
                    (SampleFormat::U32, u32),
                    (SampleFormat::U64, u64),
                    (SampleFormat::F32, f32),
                    (SampleFormat::F64, f64)
                )
                .map_err(anyhow::Error::from)
                .and_then(|stream| {
                    rtrace!("capture: stream built, play() called");
                    stream.play().map_err(anyhow::Error::from)?;
                    rtrace!("capture: play() returned");
                    Ok(stream)
                });
                let stream = match stream {
                    Ok(stream) => stream,
                    Err(err) => {
                        return Outcome::OpenFailed(err.context("The capture stream"), true)
                    }
                };
                _input_stream = Some(stream);

                // Playback is delayed one period if we're capturing audio so it has something to
                // process. When the devices are being changed a capture device that delivers
                // nothing is an error to answer, not a wait for ever
                rtrace!("capture: waiting for the first block");
                if trying.is_some() {
                    input_parker.park_timeout(CAPTURE_START_TIMEOUT);
                    rtrace!(
                        "capture: wait over, ring empty: {}",
                        input_rb_consumer.as_ref().is_some_and(|rb| rb.is_empty())
                    );
                    if input_rb_consumer.as_ref().is_some_and(|rb| rb.is_empty()) {
                        return Outcome::OpenFailed(
                            anyhow::anyhow!("The capture device delivered no audio"),
                            false,
                        );
                    }
                } else {
                    input_parker.park();
                    rtrace!("capture: the first block came");
                }
            }

            // The output callback can read input events from this ringbuffer
            let mut midi_input_rb_consumer: Option<rtrb::Consumer<PluginNoteEvent<P>>> = None;
            let midi_input_connection: Option<ActiveMidirInputDevice> =
                self.midi_input.lock().take().and_then(|midi_input| {
                    // Data is sent to the output data callback using a wait-free ring buffer
                    let (rb_producer, rb_consumer) = RingBuffer::new(MIDI_EVENT_QUEUE_CAPACITY);
                    midi_input_rb_consumer = Some(rb_consumer);

                    let result = midi_input.backend.connect(
                        &midi_input.port,
                        "MIDI input",
                        self.build_midi_input_thread::<P>(rb_producer),
                        (),
                    );

                    match result {
                        Ok(connection) => Some(ActiveMidirInputDevice {
                            connection,
                            port: midi_input.port,
                        }),
                        Err(err) => {
                            // We won't retry once this fails
                            nih_error!("Could not create the MIDI input connection: {err:#}");
                            midi_input_rb_consumer = None;

                            None
                        }
                    }
                });

            // The output callback can also emit MIDI events. To handle these we'll need to spawn
            // our own thread. This can be simplified a lot by using the `MidiOutputConnection`
            // directly inside the audio output callback, but looking at the implementation sending
            // MIDI events is not realtime safe in most midir backends.
            // NOTE: This uses crossbeam channels instead of rtrb specifically for the optional
            //        blocking API. This lets the MIDI sending thread sleep when there's no work to
            //        do.
            let mut midi_output_rb_producer: Option<crossbeam::channel::Sender<MidiOutputTask<P>>> =
                None;
            let midi_output_connection: Option<ScopedJoinHandle<ActiveMidirOutputDevice>> =
                self.midi_output.lock().take().and_then(|midi_output| {
                    // This uses crossbeam channels for the reason mentioned above, but to keep
                    // things cohesive we'll use the same naming scheme as we use for rtrb
                    let (sender, receiver) = crossbeam::channel::bounded(MIDI_EVENT_QUEUE_CAPACITY);
                    midi_output_rb_producer = Some(sender);

                    let result = midi_output
                        .backend
                        .connect(&midi_output.port, "MIDI output");

                    match result {
                        Ok(mut connection) => Some(s.spawn(move || {
                            while let Ok(task) = receiver.recv() {
                                match task {
                                    MidiOutputTask::Send(event) => match event.as_midi() {
                                        Some(MidiResult::Basic(midi_data)) => {
                                            if let Err(err) = connection.send(&midi_data) {
                                                nih_error!("Could not send MIDI event: {err}");
                                            }
                                        }
                                        Some(MidiResult::SysEx(padded_sysex_buffer, length)) => {
                                            // The SysEx buffer may contain padding
                                            let padded_sysex_buffer = padded_sysex_buffer.borrow();
                                            nih_debug_assert!(length <= padded_sysex_buffer.len());

                                            if let Err(err) =
                                                connection.send(&padded_sysex_buffer[..length])
                                            {
                                                nih_error!("Could not send MIDI event: {err}");
                                            }
                                        }
                                        None => (),
                                    },
                                    MidiOutputTask::Terminate => break,
                                }
                            }

                            // We'll return the same value from the join handle as what ends up
                            // being stored in `midi_input_connection` to keep this symmetrical with
                            // the input handling
                            ActiveMidirOutputDevice {
                                connection,
                                port: midi_output.port,
                            }
                        })),
                        Err(err) => {
                            nih_error!("Could not create the MIDI output connection: {err:#}");
                            midi_output_rb_producer = None;

                            None
                        }
                    }
                });

            // This thread needs to be blocked until audio processing ends as CPAL processes the
            // streams on another thread instead of blocking
            let wake = Wake {
                stopped: Arc::new(AtomicBool::new(false)),
                closing: closing.clone(),
                unparker: parker.unparker().clone(),
            };
            let unparker = wake.clone();
            let error_cb = {
                let unparker = wake.clone();
                move |err| {
                    nih_error!("Error during playback: {err:#}");
                    unparker.unpark();
                }
            };
            let shared_cb = {
                let cb = cb.clone();
                move |buffer: &mut Buffer,
                      aux: &mut AuxiliaryBuffers,
                      transport: Transport,
                      input_events: &[PluginNoteEvent<P>],
                      output_events: &mut Vec<PluginNoteEvent<P>>| {
                    match cb.try_lock() {
                        Some(mut cb) => cb(buffer, aux, transport, input_events, output_events),
                        // Never waits: silence for this block instead
                        None => {
                            for channel in buffer.as_slice() {
                                channel.fill(0.0);
                            }
                            true
                        }
                    }
                }
            };

            macro_rules! build_output_streams {
                ($sample_format:expr, $(($format:path, $primitive_type:ty)),*) => {
                    match $sample_format {
                        $($format => self.output.device.build_output_stream(
                            &self.output.config,
                            self.build_output_data_callback::<P, $primitive_type>(
                                unparker,
                                input_rb_consumer,
                                midi_input_rb_consumer,
                                // This is a MPMC crossbeam channel instead of an rtrb ringbuffer, and we
                                // also need it to terminate the thread
                                midi_output_rb_producer.clone(),
                                shared_cb,
                            ),
                            error_cb,
                            None,
                        ),)*
                        format => todo!("Unsupported sample format {format}"),
                    }
                }
            }
            rtrace!(
                "playback: building a stream on {} ({:?}, {:?})",
                device_name(&self.output.device),
                self.output.config,
                self.output.sample_format
            );
            let output_stream = build_output_streams!(
                self.output.sample_format,
                (SampleFormat::I8, i8),
                (SampleFormat::I16, i16),
                (SampleFormat::I32, i32),
                (SampleFormat::I64, i64),
                (SampleFormat::U8, u8),
                (SampleFormat::U16, u16),
                (SampleFormat::U32, u32),
                (SampleFormat::U64, u64),
                (SampleFormat::F32, f32),
                (SampleFormat::F64, f64)
            )
            .map_err(anyhow::Error::from)
            .and_then(|stream| {
                // TODO: Wait a period before doing this when also reading the input
                rtrace!("playback: stream built, play() called");
                stream.play().map_err(anyhow::Error::from)?;
                rtrace!("playback: play() returned");
                Ok(stream)
            });

            let outcome = match &output_stream {
                Ok(_) => {
                    // The change that was asked for is in place
                    if let Some(Trying { reply, result, .. }) = trying.take() {
                        rtrace!("playback: answer sent: {result:?}");
                        let _ = reply.send(result);
                    }
                    rtrace!("playback: waiting for the end or a request");

                    // Wait for the audio thread to exit, or for a request for other devices
                    loop {
                        parker.park();
                        rtrace!(
                            "playback: woke, stopped: {}",
                            wake.stopped.load(Ordering::SeqCst)
                        );
                        if wake.stopped.load(Ordering::SeqCst) {
                            break Outcome::Finished;
                        }
                        if let Some(request) = super::super::take_reconfigure_request() {
                            break Outcome::Reconfigure(request);
                        }
                    }
                }
                Err(err) => {
                    rtrace!("playback: opening failed: {err:#}");
                    Outcome::OpenFailed(anyhow::anyhow!("{err:#}"), true)
                }
            };

            // Nothing waits on another callback from here on: dropping a stream waits for its
            // callback to return, and a capture callback may be waiting for room that the output
            // callback (dropped first, or never started) no longer makes
            closing.store(true, Ordering::SeqCst);
            rtrace!("closing set, MIDI teardown");

            // The Midir API requires us to take things out of Options and transform between these
            // structs
            *self.midi_input.lock() =
                midi_input_connection.map(|midi_input_connection| MidirInputDevice {
                    backend: midi_input_connection.connection.close().0,
                    port: midi_input_connection.port,
                });
            *self.midi_output.lock() =
                midi_output_connection.map(move |midi_output_connection_handle| {
                    // The thread needs to be terminated first
                    midi_output_rb_producer
                        .expect("Inconsistent internal MIDI output state")
                        .send(MidiOutputTask::Terminate)
                        .expect("Could not terminate the MIDI output thread");

                    let midi_output_connection = midi_output_connection_handle
                        .join()
                        .expect("MIDI output thread panicked");

                    MidirOutputDevice {
                        backend: midi_output_connection.connection.close(),
                        port: midi_output_connection.port,
                    }
                });

            // The streams are closed
            rtrace!("MIDI done, dropping the playback stream");
            drop(output_stream);
            rtrace!("playback stream dropped, dropping the capture stream");
            drop(_input_stream.take());
            rtrace!("capture stream dropped");
            outcome
        })
    }
}

impl CpalMidir {
    /// Initialize the backend with the specified host. Returns an error if this failed for whatever
    /// reason.
    pub fn new<P: Plugin>(mut config: WrapperConfig, cpal_host_id: cpal::HostId) -> Result<Self> {
        let audio_io_layout = config.audio_io_layout_or_exit::<P>();
        cpal::host_from_id(cpal_host_id).context("The Audio API is unavailable")?;

        if config.input_device.is_none() && audio_io_layout.main_input_channels.is_some() {
            nih_log!(
                "Audio inputs are not connected automatically to prevent feedback. Use the \
                 '--input-device' option to choose an input device."
            )
        }

        if config.midi_input.is_none() && P::MIDI_INPUT >= MidiConfig::Basic {
            nih_log!("Use the '--midi-input' option to select a MIDI input device.")
        }
        if config.midi_output.is_none() && P::MIDI_OUTPUT >= MidiConfig::Basic {
            nih_log!("Use the '--midi-output' option to select a MIDI output device.")
        }

        let (input, output) = open_devices(&mut config, &audio_io_layout, cpal_host_id)?;

        // There's no obvious way to do sidechain inputs and additional outputs with the CPAL
        // backends like there is with JACK. So we'll just provide empty buffers instead.
        if !audio_io_layout.aux_input_ports.is_empty() {
            nih_warn!("Sidechain inputs are not supported with this audio backend");
        }
        if !audio_io_layout.aux_output_ports.is_empty() {
            nih_warn!("Auxiliary outputs are not supported with this audio backend");
        }

        let midi_input = match &config.midi_input {
            Some(midi_input_name) => {
                // Midir lets us preemptively ignore MIDI messages we'll never use like active
                // sensing and timing, but for maximum flexibility with NIH-plug's SysEx parsing
                // types (which could technically be used to also parse those things) we won't do
                // that.
                let midi_backend = MidiInput::new(P::NAME)
                    .context("Could not initialize the MIDI input backend")?;
                let available_ports = midi_backend.ports();

                // In case there somehow is a MIDI port with an empty name, we'll still want to
                // preserve the behavior of an empty argument resulting in a listing of options.
                let found_port = if !midi_input_name.is_empty() {
                    // This API is a bit weird
                    available_ports
                        .iter()
                        .find(|port| midi_backend.port_name(port).as_deref() == Ok(midi_input_name))
                } else {
                    None
                };

                match found_port {
                    Some(port) => Some(MidirInputDevice {
                        backend: midi_backend,
                        port: port.clone(),
                    }),
                    None => {
                        let mut message = format!(
                            "Unknown input MIDI device '{midi_input_name}'. Available devices are:"
                        );
                        for port in available_ports {
                            match midi_backend.port_name(&port) {
                                Ok(device_name) => message.push_str(&format!("\n{device_name}")),
                                Err(err) => message.push_str(&format!("\nERROR: {err:#}")),
                            }
                        }

                        anyhow::bail!(message);
                    }
                }
            }
            None => None,
        };

        let midi_output = match &config.midi_output {
            Some(midi_output_name) => {
                let midi_backend = MidiOutput::new(P::NAME)
                    .context("Could not initialize the MIDI output backend")?;
                let available_ports = midi_backend.ports();

                let found_port = if !midi_output_name.is_empty() {
                    available_ports.iter().find(|port| {
                        midi_backend.port_name(port).as_deref() == Ok(midi_output_name)
                    })
                } else {
                    None
                };

                match found_port {
                    Some(port) => Some(MidirOutputDevice {
                        backend: midi_backend,
                        port: port.clone(),
                    }),
                    None => {
                        let mut message = format!(
                            "Unknown output MIDI device '{midi_output_name}'. Available devices \
                             are:"
                        );
                        for port in available_ports {
                            match midi_backend.port_name(&port) {
                                Ok(device_name) => message.push_str(&format!("\n{device_name}")),
                                Err(err) => message.push_str(&format!("\nERROR: {err:#}")),
                            }
                        }

                        anyhow::bail!(message);
                    }
                }
            }
            None => None,
        };

        Ok(CpalMidir {
            config,
            audio_io_layout,

            host_id: cpal_host_id,
            input,
            output,

            midi_input: Mutex::new(midi_input),
            midi_output: Mutex::new(midi_output),

            one_callback_thread: {
                #[cfg(feature = "asio")]
                let asio = cpal_host_id == cpal::HostId::Asio;
                #[cfg(not(feature = "asio"))]
                let asio = false;
                asio
            },
        })
    }

    fn build_input_data_callback<T>(
        &self,
        input_unparker: Unparker,
        mut input_rb_producer: rtrb::Producer<f32>,
        stream_channels: usize,
        closing: Arc<AtomicBool>,
    ) -> impl FnMut(&[T], &InputCallbackInfo) + Send + 'static
    where
        T: Sample,
        // The CPAL update made the whole interface more complicated by switching to dasp's sample
        // trait, and then they also forgot to expose the `ToSample` trait so now you need to do
        // this
        f32: FromSample<T>,
    {
        // This callback needs to copy input samples to a ring buffer that can be read from in the
        // output data callback
        // Of each frame of the stream only the channels the plugin reads are kept
        let first = self.config.input_channel.saturating_sub(1) as usize;
        let width = self
            .audio_io_layout
            .main_input_channels
            .map(NonZeroU32::get)
            .unwrap_or(0) as usize;
        let one_callback_thread = self.one_callback_thread;
        let mut first_block = true;
        move |data, _info| {
            if first_block {
                first_block = false;
                rtrace!("capture callback: first block, {} samples", data.len());
            }
            for frame in data.chunks_exact(stream_channels) {
                for sample in &frame[first..first + width] {
                    // If for whatever reason the input callback is fired twice before an output
                    // callback, then just spin on this until the push succeeds
                    // (With one thread for both callbacks that would wait for ever: the playback
                    // callback that frees the room runs after this one, so a sample that does
                    // not fit is dropped)
                    push_sample(
                        &mut input_rb_producer,
                        sample.to_sample(),
                        one_callback_thread,
                        &closing,
                    );
                }
            }

            // The run function is blocked until a single period has been processed here. After this
            // point output playback can start.
            input_unparker.unpark();
        }
    }

    fn build_midi_input_thread<P: Plugin>(
        &self,
        mut midi_input_rb_producer: rtrb::Producer<PluginNoteEvent<P>>,
    ) -> impl FnMut(u64, &[u8], &mut ()) + Send + 'static {
        // This callback parses the received MIDI bytes and sends them to a ring buffer
        move |_timing, midi_data, _data| {
            // Since this is system MIDI there's no real useful timing information and we'll set all
            // the timings to the first sample in the buffer
            if let Ok(event) = NoteEvent::from_midi(0, midi_data) {
                if midi_input_rb_producer.push(event).is_err() {
                    nih_error!("The MIDI input event queue was full, dropping event");
                }
            }
        }
    }

    fn build_output_data_callback<P, T>(
        &self,
        unparker: Wake,
        mut input_rb_consumer: Option<rtrb::Consumer<f32>>,
        mut input_event_rb_consumer: Option<rtrb::Consumer<PluginNoteEvent<P>>>,
        mut output_event_rb_producer: Option<crossbeam::channel::Sender<MidiOutputTask<P>>>,
        mut cb: impl FnMut(
                &mut Buffer,
                &mut AuxiliaryBuffers,
                Transport,
                &[PluginNoteEvent<P>],
                &mut Vec<PluginNoteEvent<P>>,
            ) -> bool
            + 'static
            + Send,
    ) -> impl FnMut(&mut [T], &OutputCallbackInfo) + Send + 'static
    where
        P: Plugin,
        T: Sample + FromSample<f32>,
    {
        // We'll receive interlaced input samples from CPAL. These need to converted to deinterlaced
        // channels, processed, and then copied those back to an interlaced buffer for the output.
        let buffer_size = self.config.period_size as usize;
        let num_output_channels = self
            .audio_io_layout
            .main_output_channels
            .map(NonZeroU32::get)
            .unwrap_or(0) as usize;
        let num_input_channels = self
            .audio_io_layout
            .main_input_channels
            .map(NonZeroU32::get)
            .unwrap_or(0) as usize;
        // This may contain excess unused space at the end if we get fewer samples than configured
        // from CPAL
        let mut main_io_storage = vec![vec![0.0f32; buffer_size]; num_output_channels];

        // This backend does not support auxiliary inputs and outputs, so in order to have the same
        // behavior as the other backends we'll provide some dummy buffers that we'll zero out every
        // time
        let mut aux_input_storage: Vec<Vec<Vec<f32>>> = Vec::new();
        for channel_count in self.audio_io_layout.aux_input_ports {
            aux_input_storage.push(vec![
                vec![0.0f32; buffer_size];
                channel_count.get() as usize
            ]);
        }

        let mut aux_output_storage: Vec<Vec<Vec<f32>>> = Vec::new();
        for channel_count in self.audio_io_layout.aux_output_ports {
            aux_output_storage.push(vec![
                vec![0.0f32; buffer_size];
                channel_count.get() as usize
            ]);
        }

        // The actual buffer management here works the same as in the JACK backend. See that
        // implementation for more information.
        let mut buffer_manager =
            BufferManager::for_audio_io_layout(buffer_size, self.audio_io_layout);
        let mut main_io_channel_pointers = ChannelPointerVec(Vec::with_capacity(
            self.audio_io_layout
                .main_output_channels
                .map(NonZeroU32::get)
                .unwrap_or(0) as usize,
        ));
        let mut aux_input_channel_pointers =
            Vec::with_capacity(self.audio_io_layout.aux_input_ports.len());
        for channel_count in self.audio_io_layout.aux_input_ports {
            aux_input_channel_pointers.push(ChannelPointerVec(Vec::with_capacity(
                channel_count.get() as usize,
            )));
        }
        let mut aux_output_channel_pointers =
            Vec::with_capacity(self.audio_io_layout.aux_output_ports.len());
        for channel_count in self.audio_io_layout.aux_output_ports {
            aux_output_channel_pointers.push(ChannelPointerVec(Vec::with_capacity(
                channel_count.get() as usize,
            )));
        }

        let mut midi_input_events = Vec::with_capacity(MIDI_EVENT_QUEUE_CAPACITY);
        let mut midi_output_events = Vec::with_capacity(MIDI_EVENT_QUEUE_CAPACITY);

        let device_channels = self.output.config.channels as usize;
        let first_output_channel = self.config.output_channel.saturating_sub(1) as usize;
        // Can't borrow from `self` in the callback
        let config = self.config.clone();
        let mut num_processed_samples = 0usize;
        let mut first_block = true;
        let mut starved_traced = false;
        move |data, _info| {
            if first_block {
                first_block = false;
                rtrace!(
                    "playback callback: first block, {} samples, capture ring has {:?}",
                    data.len(),
                    input_rb_consumer.as_ref().map(|rb| rb.slots())
                );
            }
            // A shared-mode driver (WASAPI) may hand out more frames than the `buffer_size` we
            // asked for, and the count may change between calls. The callback is therefore
            // processed in chunks of at most `buffer_size` frames.
            let total_frames = data.len() / device_channels;
            let mut frame_offset = 0usize;
            for chunk_len in chunk_lengths(total_frames, buffer_size) {
                let data = &mut data
                    [frame_offset * device_channels..(frame_offset + chunk_len) * device_channels];
                frame_offset += chunk_len;
                let mut transport = Transport::new(config.sample_rate);
                transport.pos_samples = Some(num_processed_samples as i64);
                transport.tempo = Some(config.tempo as f64);
                transport.time_sig_numerator = Some(config.timesig_num as i32);
                transport.time_sig_denominator = Some(config.timesig_denom as i32);
                transport.playing = true;

                // If an input was configured, then the output buffer is filled with (interleaved) input
                // samples. Otherwise it gets filled with silence. There is no need to zero out any of
                // the other buffers. The `BufferManager` will copy the auxiliary input data to its own
                // storage buffers because it cannot assume that these buffers are safe to write to.
                // Because of that we'll never need to reinitialize these, and the output storage is
                // write-only (with `BufferManager` always zeroing them out when creating the buffers).
                //
                // The input ring buffer holds one frame of the plugin's input channels after the other
                match &mut input_rb_consumer {
                    Some(input_rb_consumer) => {
                        let frames = chunk_len;
                        // Never waits long for capture: a stream's own thread answers play() and
                        // drop only when this callback returns
                        let mut starved = false;
                        for frame in 0..frames {
                            for channel in main_io_storage.iter_mut().take(num_input_channels) {
                                channel[frame] =
                                    pop_sample(input_rb_consumer, &unparker.closing, &mut starved);
                            }
                        }
                        if starved && !starved_traced {
                            starved_traced = true;
                            rtrace!("playback callback: capture had no data in time (silence)");
                        }
                    }
                    None => {
                        for channel in main_io_storage.iter_mut() {
                            channel.fill(0.0);
                        }
                    }
                }

                // Things may have been moved in between callbacks, so these pointers need to be set up
                // again on each invocation
                main_io_channel_pointers.get().clear();
                for channel in main_io_storage.iter_mut() {
                    assert!(channel.len() == buffer_size);

                    main_io_channel_pointers.get().push(channel.as_mut_ptr());
                }

                for (input_channel_pointers, input_storage) in aux_input_channel_pointers
                    .iter_mut()
                    .zip(aux_input_storage.iter_mut())
                {
                    input_channel_pointers.get().clear();
                    for channel in input_storage.iter_mut() {
                        assert!(channel.len() == buffer_size);

                        input_channel_pointers.get().push(channel.as_mut_ptr());
                    }
                }

                for (output_channel_pointers, output_storage) in aux_output_channel_pointers
                    .iter_mut()
                    .zip(aux_output_storage.iter_mut())
                {
                    output_channel_pointers.get().clear();
                    for channel in output_storage.iter_mut() {
                        assert!(channel.len() == buffer_size);

                        output_channel_pointers.get().push(channel.as_mut_ptr());
                    }
                }

                {
                    let actual_sample_count = chunk_len;
                    let buffers = unsafe {
                        buffer_manager.create_buffers(0, actual_sample_count, |buffer_sources| {
                            *buffer_sources.main_output_channel_pointers = Some(ChannelPointers {
                                ptrs: NonNull::new(main_io_channel_pointers.get().as_mut_ptr())
                                    .unwrap(),
                                num_channels: main_io_channel_pointers.get().len(),
                            });
                            *buffer_sources.main_input_channel_pointers = Some(ChannelPointers {
                                ptrs: NonNull::new(main_io_channel_pointers.get().as_mut_ptr())
                                    .unwrap(),
                                num_channels: num_input_channels
                                    .min(main_io_channel_pointers.get().len()),
                            });

                            for (input_source_channel_pointers, input_channel_pointers) in
                                buffer_sources
                                    .aux_input_channel_pointers
                                    .iter_mut()
                                    .zip(aux_input_channel_pointers.iter_mut())
                            {
                                *input_source_channel_pointers = Some(ChannelPointers {
                                    ptrs: NonNull::new(input_channel_pointers.get().as_mut_ptr())
                                        .unwrap(),
                                    num_channels: input_channel_pointers.get().len(),
                                });
                            }

                            for (output_source_channel_pointers, output_channel_pointers) in
                                buffer_sources
                                    .aux_output_channel_pointers
                                    .iter_mut()
                                    .zip(aux_output_channel_pointers.iter_mut())
                            {
                                *output_source_channel_pointers = Some(ChannelPointers {
                                    ptrs: NonNull::new(output_channel_pointers.get().as_mut_ptr())
                                        .unwrap(),
                                    num_channels: output_channel_pointers.get().len(),
                                });
                            }
                        })
                    };

                    midi_input_events.clear();
                    if let Some(input_event_rb_consumer) = &mut input_event_rb_consumer {
                        if let Ok(event) = input_event_rb_consumer.pop() {
                            midi_input_events.push(event);
                        }
                    }

                    midi_output_events.clear();
                    let mut aux = AuxiliaryBuffers {
                        inputs: buffers.aux_inputs,
                        outputs: buffers.aux_outputs,
                    };
                    if !cb(
                        buffers.main_buffer,
                        &mut aux,
                        transport,
                        &midi_input_events,
                        &mut midi_output_events,
                    ) {
                        // TODO: Some way to immediately terminate the stream here would be nice
                        unparker.unpark();
                        return;
                    }
                }

                // The buffer's samples need to be written to `data` in an interlaced format
                // SAFETY: Dropping `buffers` allows us to borrow `main_io_storage` again
                // The device's channels outside the chosen ones stay silent
                for (n, frame) in data.chunks_exact_mut(device_channels).enumerate() {
                    for (ch, output_sample) in frame.iter_mut().enumerate() {
                        *output_sample = match ch.checked_sub(first_output_channel) {
                            Some(plugin_ch) if plugin_ch < num_output_channels => {
                                T::from_sample(main_io_storage[plugin_ch][n])
                            }
                            _ => T::from_sample(0.0),
                        };
                    }
                }

                if let Some(output_event_rb_producer) = &mut output_event_rb_producer {
                    for event in midi_output_events.drain(..) {
                        if output_event_rb_producer
                            .try_send(MidiOutputTask::Send(event))
                            .is_err()
                        {
                            nih_error!("The MIDI output event queue was full, dropping event");
                            break;
                        }
                    }
                }

                num_processed_samples += chunk_len;
            }
        }
    }
}

/// Puts one captured sample in the ring buffer, waiting for room if the output callback has not
/// made it yet. The wait ends when the streams are closing (the sample is dropped): the output
/// callback that makes room may be gone, and dropping the capture stream waits for this callback
/// to return. With one thread for both callbacks a sample that does not fit is dropped at once.
/// Returns whether the sample went in.
fn push_sample(
    producer: &mut rtrb::Producer<f32>,
    sample: f32,
    one_callback_thread: bool,
    closing: &AtomicBool,
) -> bool {
    while producer.push(sample).is_err() {
        if one_callback_thread || closing.load(Ordering::Relaxed) {
            return false;
        }
        std::hint::spin_loop();
    }
    true
}

/// How long the output callback waits for one captured sample at most, once per block
const SAMPLE_WAIT: std::time::Duration = std::time::Duration::from_micros(500);

/// The next captured sample for the output callback. If capture has not delivered it yet the
/// callback waits for it a short while (the capture callback is usually just behind), and then
/// plays silence for the rest of the block (`starved`): a stream's own thread answers `play()` and
/// the drop of the stream only when the callback returns, so it must never wait for capture for
/// long. Silence at once when the streams are closing.
fn pop_sample(consumer: &mut rtrb::Consumer<f32>, closing: &AtomicBool, starved: &mut bool) -> f32 {
    if let Ok(sample) = consumer.pop() {
        return sample;
    }
    if *starved || closing.load(Ordering::Relaxed) {
        return 0.0;
    }
    let start = std::time::Instant::now();
    loop {
        if let Ok(sample) = consumer.pop() {
            return sample;
        }
        if closing.load(Ordering::Relaxed) || start.elapsed() >= SAMPLE_WAIT {
            *starved = true;
            return 0.0;
        }
        std::hint::spin_loop();
    }
}

fn num_input_channels_of(layout: &AudioIOLayout) -> usize {
    layout.main_input_channels.map(NonZeroU32::get).unwrap_or(0) as usize
}

/// The lengths of the chunks a callback of `frames` frames is processed in: every one at most
/// `max` long, in order, together exactly `frames`. A shared-mode driver may give more frames than
/// the buffer size asked for.
fn chunk_lengths(frames: usize, max: usize) -> impl Iterator<Item = usize> {
    let max = max.max(1);
    let mut left = frames;
    std::iter::from_fn(move || {
        if left == 0 {
            None
        } else {
            let n = left.min(max);
            left -= n;
            Some(n)
        }
    })
}

#[cfg(test)]
mod chunk_tests {
    use super::chunk_lengths;

    #[test]
    fn chunks_cover_every_frame_in_order() {
        assert_eq!(chunk_lengths(528, 512).collect::<Vec<_>>(), vec![512, 16]);
        assert_eq!(chunk_lengths(100, 512).collect::<Vec<_>>(), vec![100]);
        assert_eq!(chunk_lengths(512, 512).collect::<Vec<_>>(), vec![512]);
        assert_eq!(
            chunk_lengths(1100, 512).collect::<Vec<_>>(),
            vec![512, 512, 76]
        );
        assert_eq!(chunk_lengths(0, 512).count(), 0);
    }
}

#[cfg(test)]
mod push_tests {
    use super::push_sample;
    use rtrb::RingBuffer;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// A capture callback waiting for room in a full ring buffer must return once the streams are
    /// closing, or dropping the capture stream waits for it for ever
    #[test]
    fn a_capture_callback_waiting_on_a_full_buffer_returns_when_closing() {
        let (mut producer, _consumer) = RingBuffer::new(1);
        let closing = Arc::new(AtomicBool::new(false));
        assert!(push_sample(&mut producer, 0.5, false, &closing));

        let (done, finished) = std::sync::mpsc::channel();
        let waiting = {
            let closing = closing.clone();
            std::thread::spawn(move || {
                // The buffer is full and nobody reads it
                let went_in = push_sample(&mut producer, 0.25, false, &closing);
                let _ = done.send(went_in);
            })
        };
        assert!(
            finished.recv_timeout(Duration::from_millis(100)).is_err(),
            "it waits for room while the streams run"
        );
        closing.store(true, Ordering::SeqCst);
        assert_eq!(
            finished.recv_timeout(Duration::from_secs(5)),
            Ok(false),
            "it returns, dropping the sample, once the streams are closing"
        );
        waiting.join().unwrap();
    }

    /// The output callback never waits long for a captured sample: a stream's own thread answers
    /// `play()` and the drop only when the callback returns
    #[test]
    fn an_output_callback_waiting_on_an_empty_buffer_plays_silence_soon() {
        let (mut producer, mut consumer) = RingBuffer::new(2);
        let closing = AtomicBool::new(false);
        let mut starved = false;
        producer.push(0.5).unwrap();
        assert_eq!(
            super::pop_sample(&mut consumer, &closing, &mut starved),
            0.5
        );
        assert!(!starved);

        // Empty: silence after a short wait, and then no more waiting in this block
        let start = std::time::Instant::now();
        assert_eq!(
            super::pop_sample(&mut consumer, &closing, &mut starved),
            0.0
        );
        assert!(starved && start.elapsed() < Duration::from_millis(200));
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            assert_eq!(
                super::pop_sample(&mut consumer, &closing, &mut starved),
                0.0
            );
        }
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "a starved block does not wait"
        );
        // Data that arrives is played again in the next block
        producer.push(0.25).unwrap();
        assert_eq!(super::pop_sample(&mut consumer, &closing, &mut false), 0.25);
    }

    #[test]
    fn an_output_callback_returns_silence_at_once_when_closing() {
        let (_producer, mut consumer) = RingBuffer::new(2);
        let closing = AtomicBool::new(true);
        let start = std::time::Instant::now();
        assert_eq!(super::pop_sample(&mut consumer, &closing, &mut false), 0.0);
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn one_callback_thread_never_waits() {
        let (mut producer, _consumer) = RingBuffer::new(1);
        let closing = AtomicBool::new(false);
        assert!(push_sample(&mut producer, 0.5, true, &closing));
        assert!(!push_sample(&mut producer, 0.25, true, &closing));
    }
}
