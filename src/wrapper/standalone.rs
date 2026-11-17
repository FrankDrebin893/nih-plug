//! A standalone plugin target that directly connects to the system's audio and MIDI ports instead
//! of relying on a plugin host. This is mostly useful for quickly testing GUI changes.

use clap::{CommandFactory, FromArgMatches};

use self::backend::Backend;
use self::config::WrapperConfig;
use self::wrapper::{Wrapper, WrapperError};
use super::util::setup_logger;
use crate::prelude::Plugin;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;

mod backend;
mod config;
mod context;
#[cfg(test)]
mod tests;
mod wrapper;

static TEST_TONE: AtomicBool = AtomicBool::new(false);

/// Turns the standalone host's test tone on or off, from any thread. While it is on, a quiet
/// 440 Hz sine replaces what the plugin writes to its main output, so the player can check which
/// outputs of the audio interface they hear. It does nothing outside the standalone host.
pub fn nih_standalone_set_test_tone(on: bool) {
    TEST_TONE.store(on, Ordering::Relaxed);
}

/// Is the standalone host's test tone on
pub fn nih_standalone_test_tone() -> bool {
    TEST_TONE.load(Ordering::Relaxed)
}

/// Which audio the standalone host is running on, for an editor to say so when there is none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StandaloneAudioStatus {
    /// A real backend (`"JACK"`, `"ALSA"`, `"WASAPI"`, `"CoreAudio"`, `"ASIO"`) opened its devices.
    Running { backend: &'static str },
    /// The dummy backend: no audio and no MIDI. `reason` is the error the host logged when it fell
    /// back to it, and `None` when the dummy backend was asked for with `--backend dummy`.
    Dummy { reason: Option<String> },
}

static AUDIO_STATUS: Mutex<Option<StandaloneAudioStatus>> = Mutex::new(None);

fn set_audio_status(status: StandaloneAudioStatus) {
    *AUDIO_STATUS.lock().unwrap_or_else(|e| e.into_inner()) = Some(status);
}

/// The audio the standalone host runs on, set once the backend is chosen and readable from any
/// thread. `None` before that, and always `None` outside the standalone host (in a DAW).
pub fn nih_standalone_audio_status() -> Option<StandaloneAudioStatus> {
    AUDIO_STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// What [`nih_standalone_reconfigure()`] changes: the audio devices, the channels of them the
/// plugin reads and writes, the sample rate and the period size. The driver stays as it is.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StandaloneDevices {
    /// The input device by name. `None` connects no input.
    pub input_device: Option<String>,
    /// The output device by name. `None` is the driver's default output.
    pub output_device: Option<String>,
    /// The first channel (counted from 1) of the input device the plugin reads. 0 counts as 1.
    pub input_channel: u32,
    /// The first channel (counted from 1) of the output device the plugin writes. 0 counts as 1.
    pub output_channel: u32,
    /// The sample rate in Hz. `None` is the output device's own, as without `--sample-rate`.
    pub sample_rate: Option<f32>,
    /// The period size in samples. `None` is the default, as without `--period-size`.
    pub period_size: Option<u32>,
}

/// A change of devices that waits for the running backend to take it.
pub(crate) struct ReconfigureRequest {
    pub devices: StandaloneDevices,
    pub reply: mpsc::Sender<Result<(), String>>,
}

static RECONFIGURE: Mutex<Option<ReconfigureRequest>> = Mutex::new(None);
/// Wakes the running backend, which exists only while it can take a request
static RECONFIGURE_WAKER: Mutex<Option<crossbeam::sync::Unparker>> = Mutex::new(None);

/// Asks the running standalone host to close its audio streams and open them again on other
/// devices, channels, sample rate or period size, while the plugin keeps running (the same
/// instance, no state lost, the process callback only misses the blocks of the gap). Returns at
/// once. The plugin is initialised again, in the gap and with the new `BufferConfig`, only when the
/// sample rate or the period size the streams open at is not the one that played before.
///
/// - `Err` at once: this backend cannot do it (JACK, or no backend running, or another request is
///   still being taken). The caller keeps the old devices and may start the program again
///   instead
/// - `Ok(receiver)`: the answer comes on `receiver` once the streams are open on the new devices
///   (`Ok`), or when they would not open (`Err` with what went wrong), in which case the old
///   devices have been opened again, so a bad choice never leaves the plugin silent
pub fn nih_standalone_reconfigure(
    devices: StandaloneDevices,
) -> Result<mpsc::Receiver<Result<(), String>>, String> {
    let waker = RECONFIGURE_WAKER.lock().unwrap_or_else(|e| e.into_inner());
    let Some(waker) = waker.as_ref() else {
        return Err("this audio driver cannot change devices while running".into());
    };
    let mut pending = RECONFIGURE.lock().unwrap_or_else(|e| e.into_inner());
    if pending.is_some() {
        return Err("the audio devices are already being changed".into());
    }
    let (reply, receiver) = mpsc::channel();
    *pending = Some(ReconfigureRequest { devices, reply });
    waker.unpark();
    Ok(receiver)
}

/// The backend says it takes requests while `waker` is `Some`
pub(crate) fn set_reconfigure_waker(waker: Option<crossbeam::sync::Unparker>) {
    *RECONFIGURE_WAKER.lock().unwrap_or_else(|e| e.into_inner()) = waker;
    if waker_is_none() {
        // A request nobody will take is answered, not left hanging
        if let Some(request) = RECONFIGURE.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = request
                .reply
                .send(Err("the audio backend has stopped".into()));
        }
    }
}

fn waker_is_none() -> bool {
    RECONFIGURE_WAKER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_none()
}

/// The request waiting for the backend, if any
pub(crate) fn take_reconfigure_request() -> Option<ReconfigureRequest> {
    RECONFIGURE.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Open an NIH-plug plugin as a standalone application. If the plugin has an editor, this will open
/// the editor and block until the editor is closed. Otherwise this will block until SIGINT is
/// received. This is mainly useful for quickly testing plugin GUIs. In order to use this, you will
/// first need to make your plugin's main struct `pub` and expose a `lib` artifact in addition to
/// your plugin's `cdylib`:
///
/// ```toml
/// # Cargo.toml
///
/// [lib]
/// # The `lib` artifact is needed for the standalone target
/// crate-type = ["cdylib", "lib"]
/// ```
///
/// You can then create a `src/main.rs` file that calls this function:
///
/// ```ignore
/// // src/main.rs
///
/// use nih_plug::prelude::*;
///
/// use plugin_name::PluginName;
///
/// fn main() {
///     nih_export_standalone::<PluginName>();
/// }
/// ```
///
/// By default this will connect to the 'default' audio and MIDI ports. Use the command line options
/// to change this. `--help` lists all available options.
///
/// If the wrapped plugin fails to initialize or throws an error during audio processing, then this
/// function will return `false`.
pub fn nih_export_standalone<P: Plugin>() -> bool {
    // TODO: If the backend fails to initialize then the standalones will exit normally instead of
    //       with an error code. This should probably be changed.
    nih_export_standalone_with_args::<P, _>(std::env::args())
}

/// The same as [`nih_export_standalone()`], but with the arguments taken from an iterator instead
/// of using [`std::env::args()`].
pub fn nih_export_standalone_with_args<P: Plugin, Args: IntoIterator<Item = String>>(
    args: Args,
) -> bool {
    setup_logger();

    // Instead of parsing this directly, we need to take a bit of a roundabout approach to get the
    // plugin's name and vendor in here since they'd otherwise be taken from NIH-plug's own
    // `Cargo.toml` file.
    let config = WrapperConfig::from_arg_matches(
        &WrapperConfig::command()
            .name(P::NAME)
            .author(P::VENDOR)
            .get_matches_from(args),
    )
    .unwrap_or_else(|err| err.exit());

    match config.backend {
        config::BackendType::Auto => {
            // What the last backend that was tried said, for the dummy backend's status
            let reason = std::cell::RefCell::new(None::<String>);
            let result = backend::Jack::new::<P>(config.clone()).map(|backend| {
                nih_log!("Using the JACK backend");
                set_audio_status(StandaloneAudioStatus::Running { backend: "JACK" });
                run_wrapper::<P, _>(backend, config.clone())
            });

            #[cfg(target_os = "linux")]
            let result = result.or_else(|_| {
                match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::Alsa) {
                    Ok(backend) => {
                        nih_log!("Using the ALSA backend");
                        set_audio_status(StandaloneAudioStatus::Running { backend: "ALSA" });
                        Ok(run_wrapper::<P, _>(backend, config.clone()))
                    }
                    Err(err) => {
                        nih_error!(
                            "Could not initialize either the JACK or the ALSA backends, falling \
                             back to the dummy audio backend: {err:#}"
                        );
                        *reason.borrow_mut() = Some(format!("{err:#}"));
                        Err(())
                    }
                }
            });
            #[cfg(target_os = "macos")]
            let result = result.or_else(|_| {
                match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::CoreAudio) {
                    Ok(backend) => {
                        nih_log!("Using the CoreAudio backend");
                        set_audio_status(StandaloneAudioStatus::Running { backend: "CoreAudio" });
                        Ok(run_wrapper::<P, _>(backend, config.clone()))
                    }
                    Err(err) => {
                        nih_error!(
                            "Could not initialize either the JACK or the CoreAudio backends, \
                             falling back to the dummy audio backend: {err:#}"
                        );
                        *reason.borrow_mut() = Some(format!("{err:#}"));
                        Err(())
                    }
                }
            });
            #[cfg(target_os = "windows")]
            let result = result.or_else(|_| {
                match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::Wasapi) {
                    Ok(backend) => {
                        nih_log!("Using the WASAPI backend");
                        set_audio_status(StandaloneAudioStatus::Running { backend: "WASAPI" });
                        Ok(run_wrapper::<P, _>(backend, config.clone()))
                    }
                    Err(err) => {
                        nih_error!(
                            "Could not initialize either the JACK or the WASAPI backends, falling \
                             back to the dummy audio backend: {err:#}"
                        );
                        *reason.borrow_mut() = Some(format!("{err:#}"));
                        Err(())
                    }
                }
            });

            result.unwrap_or_else(|_| {
                nih_error!("Falling back to the dummy audio backend, audio and MIDI will not work");
                set_audio_status(StandaloneAudioStatus::Dummy { reason: reason.take() });
                run_wrapper::<P, _>(backend::Dummy::new::<P>(config.clone()), config)
            })
        }
        config::BackendType::Jack => match backend::Jack::new::<P>(config.clone()) {
            Ok(backend) => {
                set_audio_status(StandaloneAudioStatus::Running { backend: "JACK" });
                run_wrapper::<P, _>(backend, config)
            }
            Err(err) => {
                nih_error!("Could not initialize the JACK backend: {:#}", err);
                false
            }
        },
        #[cfg(target_os = "linux")]
        config::BackendType::Alsa => {
            match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::Alsa) {
                Ok(backend) => {
                    set_audio_status(StandaloneAudioStatus::Running { backend: "ALSA" });
                    run_wrapper::<P, _>(backend, config)
                }
                Err(err) => {
                    nih_error!("Could not initialize the ALSA backend: {:#}", err);
                    false
                }
            }
        }
        #[cfg(target_os = "macos")]
        config::BackendType::CoreAudio => {
            match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::CoreAudio) {
                Ok(backend) => {
                    set_audio_status(StandaloneAudioStatus::Running { backend: "CoreAudio" });
                    run_wrapper::<P, _>(backend, config)
                }
                Err(err) => {
                    nih_error!("Could not initialize the CoreAudio backend: {:#}", err);
                    false
                }
            }
        }
        #[cfg(target_os = "windows")]
        config::BackendType::Wasapi => {
            match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::Wasapi) {
                Ok(backend) => {
                    set_audio_status(StandaloneAudioStatus::Running { backend: "WASAPI" });
                    run_wrapper::<P, _>(backend, config)
                }
                Err(err) => {
                    nih_error!("Could not initialize the WASAPI backend: {:#}", err);
                    false
                }
            }
        }
        #[cfg(all(target_os = "windows", feature = "asio"))]
        config::BackendType::Asio => {
            match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::Asio) {
                Ok(backend) => {
                    set_audio_status(StandaloneAudioStatus::Running { backend: "ASIO" });
                    run_wrapper::<P, _>(backend, config)
                }
                Err(err) => {
                    nih_error!("Could not initialize the ASIO backend: {:#}", err);
                    false
                }
            }
        }
        config::BackendType::Dummy => {
            set_audio_status(StandaloneAudioStatus::Dummy { reason: None });
            run_wrapper::<P, _>(backend::Dummy::new::<P>(config.clone()), config)
        }
    }
}

fn run_wrapper<P: Plugin, B: Backend<P>>(backend: B, config: WrapperConfig) -> bool {
    let wrapper = match Wrapper::<P, _>::new(backend, config) {
        Ok(wrapper) => wrapper,
        Err(err) => {
            print_error(err);
            return false;
        }
    };

    // TODO: Add a repl while the application is running to interact with parameters
    match wrapper.run() {
        Ok(()) => true,
        Err(err) => {
            print_error(err);
            false
        }
    }
}

fn print_error(error: WrapperError) {
    match error {
        WrapperError::InitializationFailed => {
            nih_error!("The plugin failed to initialize");
        }
    }
}

/// The request slot and the running backend are process-wide, so the tests that use them take turns
#[cfg(test)]
static TEST_TURN: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod reconfigure_tests {
    use super::*;
    use crate::prelude::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    static INITIALIZED: AtomicUsize = AtomicUsize::new(0);
    static BLOCKS: AtomicUsize = AtomicUsize::new(0);
    static STOP: AtomicBool = AtomicBool::new(false);

    struct NoParams;

    // SAFETY: no parameters, so no pointers
    unsafe impl Params for NoParams {
        fn param_map(&self) -> Vec<(String, ParamPtr, String)> {
            Vec::new()
        }
    }

    #[derive(Default)]
    struct Counter;

    impl Plugin for Counter {
        const NAME: &'static str = "Counter";
        const VENDOR: &'static str = "Test";
        const URL: &'static str = "";
        const EMAIL: &'static str = "";
        const VERSION: &'static str = "0";
        const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
            main_input_channels: None,
            main_output_channels: NonZeroU32::new(2),
            ..AudioIOLayout::const_default()
        }];
        type SysExMessage = ();
        type BackgroundTask = ();

        fn params(&self) -> Arc<dyn Params> {
            Arc::new(NoParams)
        }

        fn initialize(
            &mut self,
            _: &AudioIOLayout,
            _: &BufferConfig,
            _: &mut impl InitContext<Self>,
        ) -> bool {
            INITIALIZED.fetch_add(1, Ordering::SeqCst);
            true
        }

        fn process(
            &mut self,
            _: &mut Buffer,
            _: &mut AuxiliaryBuffers,
            _: &mut impl ProcessContext<Self>,
        ) -> ProcessStatus {
            if STOP.load(Ordering::SeqCst) {
                return ProcessStatus::Error("stopped");
            }
            BLOCKS.fetch_add(1, Ordering::SeqCst);
            ProcessStatus::Normal
        }
    }

    use std::num::NonZeroU32;

    fn wait_for(what: impl Fn() -> bool) {
        let start = Instant::now();
        while !what() {
            assert!(start.elapsed() < Duration::from_secs(10), "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The status the host sets when the dummy backend is asked for
    #[test]
    fn the_audio_status_says_dummy_for_the_dummy_backend() {
        let _turn = TEST_TURN.lock().unwrap_or_else(|e| e.into_inner());
        set_audio_status(StandaloneAudioStatus::Running { backend: "TEST" });
        assert_eq!(
            nih_standalone_audio_status(),
            Some(StandaloneAudioStatus::Running { backend: "TEST" })
        );
        STOP.store(false, Ordering::SeqCst);
        let before = BLOCKS.load(Ordering::SeqCst);
        let host = std::thread::spawn(|| {
            nih_export_standalone_with_args::<Counter, _>(
                ["counter", "--backend", "dummy", "-p", "64"].map(String::from),
            )
        });
        wait_for(|| BLOCKS.load(Ordering::SeqCst) > before + 3);
        assert_eq!(
            nih_standalone_audio_status(),
            Some(StandaloneAudioStatus::Dummy { reason: None })
        );
        // The host has no editor, so it blocks until interrupted once the audio has ended: not
        // joined
        STOP.store(true, Ordering::SeqCst);
        wait_for(|| nih_standalone_reconfigure(StandaloneDevices::default()).is_err());
        drop(host);
        STOP.store(false, Ordering::SeqCst);
        // The other test counts from none
        INITIALIZED.store(0, Ordering::SeqCst);
        BLOCKS.store(0, Ordering::SeqCst);
    }

    /// One test, since the host and the request slot are process-wide
    #[test]
    fn reconfigure_keeps_the_plugin_running_and_initialised_once() {
        let _turn = TEST_TURN.lock().unwrap_or_else(|e| e.into_inner());
        let host = std::thread::spawn(|| {
            nih_export_standalone_with_args::<Counter, _>(
                ["counter", "--backend", "dummy", "-p", "64"].map(String::from),
            )
        });
        wait_for(|| BLOCKS.load(Ordering::SeqCst) > 3);

        let devices = StandaloneDevices {
            output_device: Some("Dummy".into()),
            input_channel: 1,
            output_channel: 3,
            // The period size the host was started with: a different one is a new initialize
            period_size: Some(64),
            ..StandaloneDevices::default()
        };
        let answer = nih_standalone_reconfigure(devices).expect("the host takes requests");
        assert_eq!(answer.recv_timeout(Duration::from_secs(5)), Ok(Ok(())));

        // The same callback goes on
        let after = BLOCKS.load(Ordering::SeqCst);
        wait_for(|| BLOCKS.load(Ordering::SeqCst) > after + 3);
        assert_eq!(
            INITIALIZED.load(Ordering::SeqCst),
            1,
            "devices changing is not a new initialize"
        );

        // A device that is not there is an error, and what played goes on playing
        let answer = nih_standalone_reconfigure(StandaloneDevices {
            output_device: Some("Missing".into()),
            period_size: Some(64),
            ..StandaloneDevices::default()
        })
        .expect("the host takes requests");
        let error = answer
            .recv_timeout(Duration::from_secs(5))
            .expect("an answer")
            .expect_err("a device that is not there");
        assert!(error.contains("Missing"), "{error}");
        let after = BLOCKS.load(Ordering::SeqCst);
        wait_for(|| BLOCKS.load(Ordering::SeqCst) > after + 3);
        assert_eq!(INITIALIZED.load(Ordering::SeqCst), 1);

        STOP.store(true, Ordering::SeqCst);
        // The audio ends with the plugin's error; a host without an editor then blocks until it
        // is interrupted, so it is not joined. Nobody takes requests any more
        wait_for(|| nih_standalone_reconfigure(StandaloneDevices::default()).is_err());
        drop(host);
    }
}

#[cfg(test)]
mod sample_rate_tests {
    use super::*;
    use crate::prelude::*;
    use std::num::NonZeroU32;
    use std::sync::Arc;

    static SEEN: Mutex<Option<(f32, u32)>> = Mutex::new(None);

    struct NoParams;

    // SAFETY: no parameters, so no pointers
    unsafe impl Params for NoParams {
        fn param_map(&self) -> Vec<(String, ParamPtr, String)> {
            Vec::new()
        }
    }

    /// Writes down the buffer configuration it is initialized with
    #[derive(Default)]
    struct Recorder;

    impl Plugin for Recorder {
        const NAME: &'static str = "Recorder";
        const VENDOR: &'static str = "Test";
        const URL: &'static str = "";
        const EMAIL: &'static str = "";
        const VERSION: &'static str = "0";
        const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
            main_input_channels: None,
            main_output_channels: NonZeroU32::new(2),
            ..AudioIOLayout::const_default()
        }];
        type SysExMessage = ();
        type BackgroundTask = ();

        fn params(&self) -> Arc<dyn Params> {
            Arc::new(NoParams)
        }

        fn initialize(
            &mut self,
            _: &AudioIOLayout,
            buffer_config: &BufferConfig,
            _: &mut impl InitContext<Self>,
        ) -> bool {
            *SEEN.lock().unwrap() =
                Some((buffer_config.sample_rate, buffer_config.max_buffer_size));
            true
        }

        fn process(
            &mut self,
            _: &mut Buffer,
            _: &mut AuxiliaryBuffers,
            _: &mut impl ProcessContext<Self>,
        ) -> ProcessStatus {
            ProcessStatus::Normal
        }
    }

    /// A backend that opened a device running at 44.1 kHz, whatever the configuration asked for
    struct Device44100;

    impl Backend<Recorder> for Device44100 {
        fn resolve_config(&self, config: &mut WrapperConfig) {
            config.sample_rate = Some(44100.0);
            config.period_size = 256;
        }

        fn run(
            &mut self,
            _: impl FnMut(
                    &mut Buffer,
                    &mut AuxiliaryBuffers,
                    Transport,
                    &[PluginNoteEvent<Recorder>],
                    &mut Vec<PluginNoteEvent<Recorder>>,
                ) -> bool
                + 'static
                + Send,
            _: impl FnMut(f32, u32) -> bool + 'static + Send,
        ) {
        }
    }

    fn config(args: &[&str]) -> WrapperConfig {
        WrapperConfig::from_arg_matches(
            &WrapperConfig::command()
                .get_matches_from(std::iter::once("x").chain(args.iter().copied())),
        )
        .unwrap()
    }

    #[test]
    fn no_sample_rate_asked_for_is_none() {
        assert_eq!(config(&[]).sample_rate, None);
        assert_eq!(
            config(&["--sample-rate", "96000"]).sample_rate,
            Some(96000.0)
        );
    }

    /// The plugin is told the rate the stream runs at, not the one the configuration had before the
    /// devices were opened
    #[test]
    fn the_plugin_gets_the_rate_the_backend_resolved() {
        let _wrapper = Wrapper::<Recorder, _>::new(Device44100, config(&[]))
            .ok()
            .unwrap();
        assert_eq!(*SEEN.lock().unwrap(), Some((44100.0, 256)));
    }
}
