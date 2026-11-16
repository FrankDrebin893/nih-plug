//! Changing the sample rate and the period size of the running standalone host, on the dummy
//! backend (no audio device needed). The requests and the plugin's initialisation are global to
//! the process, so the tests take turns.

use clap::{CommandFactory, FromArgMatches};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::backend::{Backend, Dummy};
use super::config::WrapperConfig;
use super::wrapper::Wrapper;
use super::{nih_standalone_reconfigure, StandaloneDevices};
use crate::prelude::*;

/// Every `initialize` of the test plugin: the sample rate and the buffer size it was told
static INITS: Mutex<Vec<(f32, u32)>> = Mutex::new(Vec::new());
/// The test plugin refuses this sample rate
const REFUSED_RATE: f32 = 22050.0;

#[derive(Default)]
struct TestPlugin {
    params: Arc<TestParams>,
}

#[derive(Default)]
struct TestParams;

// SAFETY: no parameters, so no pointers
unsafe impl Params for TestParams {
    fn param_map(&self) -> Vec<(String, ParamPtr, String)> {
        Vec::new()
    }
}

impl Plugin for TestPlugin {
    const NAME: &'static str = "Test";
    const VENDOR: &'static str = "Test";
    const URL: &'static str = "";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = "0";
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: NonZeroU32::new(2),
        main_output_channels: NonZeroU32::new(2),
        aux_input_ports: &[],
        aux_output_ports: &[],
        names: PortNames::const_default(),
    }];
    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn initialize(
        &mut self,
        _: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _: &mut impl InitContext<Self>,
    ) -> bool {
        INITS
            .lock()
            .unwrap()
            .push((buffer_config.sample_rate, buffer_config.max_buffer_size));
        buffer_config.sample_rate != REFUSED_RATE
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

fn config() -> WrapperConfig {
    WrapperConfig::from_arg_matches(&WrapperConfig::command().get_matches_from([
        "test",
        "--backend",
        "dummy",
    ]))
    .unwrap()
}

fn request(sample_rate: f32, period_size: u32) -> StandaloneDevices {
    StandaloneDevices {
        sample_rate: Some(sample_rate),
        period_size: Some(period_size),
        ..Default::default()
    }
}

fn answer(devices: StandaloneDevices) -> Result<(), String> {
    nih_standalone_reconfigure(devices)
        .expect("the backend takes requests")
        .recv_timeout(Duration::from_secs(5))
        .expect("the backend answers")
}

fn inits() -> Vec<(f32, u32)> {
    INITS.lock().unwrap().clone()
}

/// Waits until `blocks` has grown by at least 3 from now: the process callback is still called
fn still_plays(blocks: &AtomicUsize) {
    let before = blocks.load(Ordering::SeqCst);
    let start = Instant::now();
    while blocks.load(Ordering::SeqCst) < before + 3 {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the callback stopped"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn the_process_callback_survives_a_change_of_rate_and_period_and_initialize_runs_only_for_those() {
    let _turn = super::TEST_TURN.lock().unwrap_or_else(|e| e.into_inner());
    INITS.lock().unwrap().clear();
    let mut backend = Dummy::new::<TestPlugin>(config());
    let blocks = Arc::new(AtomicUsize::new(0));
    let size = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let (blocks, size, stop) = (blocks.clone(), size.clone(), stop.clone());
        std::thread::spawn(move || {
            <Dummy as Backend<TestPlugin>>::run(
                &mut backend,
                move |buffer, _, _, _, _| {
                    blocks.fetch_add(1, Ordering::SeqCst);
                    size.store(buffer.samples(), Ordering::SeqCst);
                    !stop.load(Ordering::SeqCst)
                },
                |rate, period| {
                    INITS.lock().unwrap().push((rate, period));
                    rate != REFUSED_RATE
                },
            )
        })
    };
    still_plays(&blocks);
    assert_eq!(size.load(Ordering::SeqCst), 512);

    // The rate and the period it plays at: devices change, the plugin is not initialised again
    assert_eq!(answer(request(48000.0, 512)), Ok(()));
    assert_eq!(answer(StandaloneDevices::default()), Ok(()));
    assert_eq!(inits(), vec![]);
    still_plays(&blocks);

    // Another rate: initialised again with it, and the callback goes on
    assert_eq!(answer(request(44100.0, 512)), Ok(()));
    assert_eq!(inits(), vec![(44100.0, 512)]);
    still_plays(&blocks);

    // Another period size: initialised again, and the buffers are of that size
    assert_eq!(answer(request(44100.0, 128)), Ok(()));
    assert_eq!(inits(), vec![(44100.0, 512), (44100.0, 128)]);
    still_plays(&blocks);
    assert_eq!(size.load(Ordering::SeqCst), 128);

    // A plugin that will not take a rate: an error, and it is told the old settings again
    let refused = answer(request(REFUSED_RATE, 128));
    assert!(refused.unwrap_err().contains("did not take"));
    assert_eq!(
        inits()[2..],
        [(REFUSED_RATE, 128), (44100.0, 128)],
        "the old settings are initialised again"
    );
    still_plays(&blocks);
    assert_eq!(size.load(Ordering::SeqCst), 128);

    // A device that is not there: an error before the plugin hears of anything
    let before = inits().len();
    let unknown = StandaloneDevices {
        output_device: Some("Nowhere".into()),
        ..request(96000.0, 64)
    };
    assert!(answer(unknown).is_err());
    assert_eq!(inits().len(), before);
    still_plays(&blocks);

    stop.store(true, Ordering::SeqCst);
    thread.join().unwrap();
}

#[test]
fn the_wrapper_initialises_the_plugin_with_the_new_buffer_config_and_keeps_the_old_on_a_refusal() {
    let _turn = super::TEST_TURN.lock().unwrap_or_else(|e| e.into_inner());
    INITS.lock().unwrap().clear();
    let wrapper = Wrapper::<TestPlugin, _>::new(Dummy::new::<TestPlugin>(config()), config())
        .unwrap_or_else(|_| panic!("the plugin starts"));
    assert_eq!(inits(), vec![(48000.0, 512)]);

    assert!(wrapper.reinitialize(96000.0, 64));
    assert_eq!(inits().last(), Some(&(96000.0, 64)));
    let now = wrapper.buffer_config();
    assert_eq!((now.sample_rate, now.max_buffer_size), (96000.0, 64));

    assert!(!wrapper.reinitialize(REFUSED_RATE, 256));
    let now = wrapper.buffer_config();
    assert_eq!(
        (now.sample_rate, now.max_buffer_size),
        (96000.0, 64),
        "a refusal leaves the old settings"
    );
}
