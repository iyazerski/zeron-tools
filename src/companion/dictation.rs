//! Dictation with Apple's speech recognizer: the microphone feeds it, and it
//! reports what it hears so far. Winit's text input drops most of what macOS
//! system dictation types, so the companion listens itself.

use std::cell::RefCell;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::Bool;
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
use objc2_avf_audio::{AVAudioEngine, AVAudioPCMBuffer, AVAudioTime};
use objc2_foundation::NSError;
use objc2_speech::{
    SFSpeechAudioBufferRecognitionRequest, SFSpeechRecognitionResult, SFSpeechRecognitionTask,
    SFSpeechRecognizer, SFSpeechRecognizerAuthorizationStatus,
};

/// The recognizer's error for a session that ended before any speech.
const NO_SPEECH: isize = 1110;
/// Where System Settings lets the user change their answer.
const SETTINGS: &str = "Allow zeron-tools in System Settings ▸ Privacy & Security";

/// What a session reports, on the main thread.
pub enum Heard {
    /// Everything heard since the session started, as text.
    Words(String),
    /// The session is over and its words are final.
    Done,
    Failed(String),
}

/// A running session; dropping it stops listening and drops what's unheard.
pub struct Listening {
    engine: Retained<AVAudioEngine>,
    request: Retained<SFSpeechAudioBufferRecognitionRequest>,
    task: Retained<SFSpeechRecognitionTask>,
}

/// Ask for speech recognition and the microphone, then call `done` on any
/// thread with why dictation can't run, if it can't.
pub fn authorize(done: impl FnOnce(Allowed) + Send + 'static) {
    let done = Once::new(done);
    // SAFETY: the status is a plain class property.
    match unsafe { SFSpeechRecognizer::authorizationStatus() } {
        SFSpeechRecognizerAuthorizationStatus::Authorized => microphone(done),
        SFSpeechRecognizerAuthorizationStatus::NotDetermined => {
            let asked = RcBlock::new(move |status: SFSpeechRecognizerAuthorizationStatus| {
                if status == SFSpeechRecognizerAuthorizationStatus::Authorized {
                    microphone(done.clone());
                } else {
                    done.call(Err(format!("Speech recognition is off. {SETTINGS}.")));
                }
            });
            // SAFETY: the binary's Info.plist explains the request, which
            // macOS requires; the block is copied and may run on any queue.
            unsafe { SFSpeechRecognizer::requestAuthorization(&asked) };
        }
        _ => done.call(Err(format!("Speech recognition is off. {SETTINGS}."))),
    }
}

/// Ask for the microphone, then finish `done`.
fn microphone(done: Once) {
    // SAFETY: AVFoundation defines the audio media type for the app's life.
    let Some(audio) = (unsafe { AVMediaTypeAudio }) else {
        return done.call(Err("No audio input on this Mac".into()));
    };
    let denied = || Err(format!("The microphone is off. {SETTINGS}."));
    // SAFETY: a class property, read with a valid media type.
    match unsafe { AVCaptureDevice::authorizationStatusForMediaType(audio) } {
        AVAuthorizationStatus::Authorized => done.call(Ok(())),
        AVAuthorizationStatus::NotDetermined => {
            let asked = RcBlock::new(move |granted: Bool| {
                done.call(if granted.as_bool() { Ok(()) } else { denied() });
            });
            // SAFETY: as for speech recognition: the Info.plist explains it,
            // and the block may run on any queue.
            unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(audio, &asked) };
        }
        _ => done.call(denied()),
    }
}

impl Listening {
    /// Listen to the microphone, calling `heard` on the main thread with what
    /// it hears until the session ends. Needs `authorize` to have passed.
    pub fn start(heard: impl Fn(Heard) + 'static) -> Result<Self, String> {
        // SAFETY: plain Speech and AVFAudio calls on the main thread, with
        // valid objects. The recognizer reports on the main queue, its default.
        unsafe {
            let recognizer = SFSpeechRecognizer::new();
            if !recognizer.isAvailable() {
                return Err("Speech recognition isn't available right now".into());
            }
            let request = SFSpeechAudioBufferRecognitionRequest::new();
            request.setShouldReportPartialResults(true);
            request.setAddsPunctuation(true);
            // On the Mac, when it can: private, and with no time limit.
            if recognizer.supportsOnDeviceRecognition() {
                request.setRequiresOnDeviceRecognition(true);
            }
            let engine = AVAudioEngine::new();
            let input = engine.inputNode();
            let format = input.outputFormatForBus(0);
            let feed = request.clone();
            let tap = RcBlock::new(
                move |buffer: NonNull<AVAudioPCMBuffer>, _: NonNull<AVAudioTime>| {
                    feed.appendAudioPCMBuffer(buffer.as_ref());
                },
            );
            input.installTapOnBus_bufferSize_format_block(
                0,
                1024,
                Some(&format),
                RcBlock::as_ptr(&tap),
            );
            // Each pause starts a new transcription; earlier ones are kept here.
            let said = RefCell::new(String::new());
            let handler = RcBlock::new(
                move |result: *mut SFSpeechRecognitionResult, error: *mut NSError| {
                    if let Some(result) = result.as_ref() {
                        let current = result.bestTranscription().formattedString().to_string();
                        let mut said = said.borrow_mut();
                        let words = join(&said, &current);
                        // Metadata comes with the last result before a pause.
                        if result.speechRecognitionMetadata().is_some() {
                            *said = words.clone();
                        }
                        heard(Heard::Words(words));
                        if result.isFinal() {
                            heard(Heard::Done);
                        }
                    } else if let Some(error) = error.as_ref() {
                        heard(if error.code() == NO_SPEECH {
                            Heard::Done
                        } else {
                            Heard::Failed(error.localizedDescription().to_string())
                        });
                    }
                },
            );
            let task = recognizer.recognitionTaskWithRequest_resultHandler(&request, &handler);
            engine.prepare();
            if let Err(e) = engine.startAndReturnError() {
                input.removeTapOnBus(0);
                task.cancel();
                return Err(e.localizedDescription().to_string());
            }
            Ok(Self {
                engine,
                request,
                task,
            })
        }
    }

    /// Stop listening; the last words and `Heard::Done` follow.
    pub fn stop(&self) {
        // SAFETY: valid objects; stopping twice is harmless.
        unsafe {
            self.engine.stop();
            self.engine.inputNode().removeTapOnBus(0);
            self.request.endAudio();
        }
    }
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.stop();
        // SAFETY: a valid task; cancelling a finished one does nothing.
        unsafe { self.task.cancel() };
    }
}

/// `before`, then `after`, with a space between when both have text.
pub fn join(before: &str, after: &str) -> String {
    match (before.trim_end(), after.trim_start()) {
        ("", after) => after.to_owned(),
        (before, "") => before.to_owned(),
        (before, after) => format!("{before} {after}"),
    }
}

/// Why dictation can't run, if it can't.
type Allowed = Result<(), String>;
type Callback = Box<dyn FnOnce(Allowed) + Send>;

/// `authorize`'s callback, shared by blocks that AppKit may copy and call
/// more than once; it runs once.
#[derive(Clone)]
struct Once(Arc<Mutex<Option<Callback>>>);

impl Once {
    fn new(f: impl FnOnce(Allowed) + Send + 'static) -> Self {
        Self(Arc::new(Mutex::new(Some(Box::new(f)))))
    }

    /// Run the callback, if it hasn't run yet.
    fn call(&self, allowed: Allowed) {
        let f = self.0.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(f) = f {
            f(allowed);
        }
    }
}
