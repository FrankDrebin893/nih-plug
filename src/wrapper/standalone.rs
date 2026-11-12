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

static TRACE_FILE: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);

/// TEMPORARY, to find a hang on a change of devices: where [`trace`] writes besides stderr
pub fn nih_standalone_set_trace_file(path: std::path::PathBuf) {
    *TRACE_FILE.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
}

/// TEMPORARY, to find a hang on a change of devices: a timestamped line on stderr and appended to
/// the trace file (`nih_standalone_set_trace_file()`, else `nih-reconfigure-trace.log` in the
/// temporary folder). Always on for now.
pub fn trace(args: std::fmt::Arguments) {
    use std::io::Write;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let thread = std::thread::current();
    let line = format!(
        "{}.{:03} [{}] fork: {}\n",
        now.as_secs(),
        now.subsec_millis(),
        thread.name().unwrap_or("unnamed"),
        args
    );
    eprint!("{line}");
    let path = TRACE_FILE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("nih-reconfigure-trace.log"));
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// What [`nih_standalone_reconfigure()`] changes: the audio devices and the channels of them the
/// plugin reads and writes. The driver, the sample rate and the period size stay as they are.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StandaloneDevices {
    /// The input device by name. `None` connects no input.
    pub input_device: Option<String>,
    /// The output device by name. `None` is the driver's default output.
    pub output_device: Option<String>,
    /// The first channel (counted from 1) of the input device the plugin reads. 0 counts as 1.
    pub input_channel: u32,
    /// The first channel (counted from 1) of the output device the plugin writes. 0 counts as 1.
    pub output_channel: u32,
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
/// devices or channels, while the plugin keeps running (the same instance: no `initialize`, no
/// state lost, the process callback only misses the blocks of the gap). Returns at once.
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
    trace(format_args!(
        "nih_standalone_reconfigure called: {devices:?}"
    ));
    let waker = RECONFIGURE_WAKER.lock().unwrap_or_else(|e| e.into_inner());
    let Some(waker) = waker.as_ref() else {
        trace(format_args!(
            "no waker: the running backend takes no requests"
        ));
        return Err("this audio driver cannot change devices while running".into());
    };
    let mut pending = RECONFIGURE.lock().unwrap_or_else(|e| e.into_inner());
    if pending.is_some() {
        trace(format_args!("a request is already waiting"));
        return Err("the audio devices are already being changed".into());
    }
    let (reply, receiver) = mpsc::channel();
    *pending = Some(ReconfigureRequest { devices, reply });
    waker.unpark();
    trace(format_args!("request stored, waker fired"));
    Ok(receiver)
}

/// The backend says it takes requests while `waker` is `Some`
pub(crate) fn set_reconfigure_waker(waker: Option<crossbeam::sync::Unparker>) {
    trace(format_args!(
        "backend waker {}",
        if waker.is_some() { "set" } else { "cleared" }
    ));
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
    let request = RECONFIGURE.lock().unwrap_or_else(|e| e.into_inner()).take();
    if request.is_some() {
        trace(format_args!("backend took the request"));
    }
    request
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
            let result = backend::Jack::new::<P>(config.clone()).map(|backend| {
                nih_log!("Using the JACK backend");
                run_wrapper::<P, _>(backend, config.clone())
            });

            #[cfg(target_os = "linux")]
            let result = result.or_else(|_| {
                match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::Alsa) {
                    Ok(backend) => {
                        nih_log!("Using the ALSA backend");
                        Ok(run_wrapper::<P, _>(backend, config.clone()))
                    }
                    Err(err) => {
                        nih_error!(
                            "Could not initialize either the JACK or the ALSA backends, falling \
                             back to the dummy audio backend: {err:#}"
                        );
                        Err(())
                    }
                }
            });
            #[cfg(target_os = "macos")]
            let result = result.or_else(|_| {
                match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::CoreAudio) {
                    Ok(backend) => {
                        nih_log!("Using the CoreAudio backend");
                        Ok(run_wrapper::<P, _>(backend, config.clone()))
                    }
                    Err(err) => {
                        nih_error!(
                            "Could not initialize either the JACK or the CoreAudio backends, \
                             falling back to the dummy audio backend: {err:#}"
                        );
                        Err(())
                    }
                }
            });
            #[cfg(target_os = "windows")]
            let result = result.or_else(|_| {
                match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::Wasapi) {
                    Ok(backend) => {
                        nih_log!("Using the WASAPI backend");
                        Ok(run_wrapper::<P, _>(backend, config.clone()))
                    }
                    Err(err) => {
                        nih_error!(
                            "Could not initialize either the JACK or the WASAPI backends, falling \
                             back to the dummy audio backend: {err:#}"
                        );
                        Err(())
                    }
                }
            });

            result.unwrap_or_else(|_| {
                nih_error!("Falling back to the dummy audio backend, audio and MIDI will not work");
                run_wrapper::<P, _>(backend::Dummy::new::<P>(config.clone()), config)
            })
        }
        config::BackendType::Jack => match backend::Jack::new::<P>(config.clone()) {
            Ok(backend) => run_wrapper::<P, _>(backend, config),
            Err(err) => {
                nih_error!("Could not initialize the JACK backend: {:#}", err);
                false
            }
        },
        #[cfg(target_os = "linux")]
        config::BackendType::Alsa => {
            match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::Alsa) {
                Ok(backend) => run_wrapper::<P, _>(backend, config),
                Err(err) => {
                    nih_error!("Could not initialize the ALSA backend: {:#}", err);
                    false
                }
            }
        }
        #[cfg(target_os = "macos")]
        config::BackendType::CoreAudio => {
            match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::CoreAudio) {
                Ok(backend) => run_wrapper::<P, _>(backend, config),
                Err(err) => {
                    nih_error!("Could not initialize the CoreAudio backend: {:#}", err);
                    false
                }
            }
        }
        #[cfg(target_os = "windows")]
        config::BackendType::Wasapi => {
            match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::Wasapi) {
                Ok(backend) => run_wrapper::<P, _>(backend, config),
                Err(err) => {
                    nih_error!("Could not initialize the WASAPI backend: {:#}", err);
                    false
                }
            }
        }
        #[cfg(all(target_os = "windows", feature = "asio"))]
        config::BackendType::Asio => {
            match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::Asio) {
                Ok(backend) => run_wrapper::<P, _>(backend, config),
                Err(err) => {
                    nih_error!("Could not initialize the ASIO backend: {:#}", err);
                    false
                }
            }
        }
        config::BackendType::Dummy => {
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

    /// One test, since the host and the request slot are process-wide
    #[test]
    fn reconfigure_keeps_the_plugin_running_and_initialised_once() {
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
