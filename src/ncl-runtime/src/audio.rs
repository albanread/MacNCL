//! Lisp-side audio: shims around the NewAudio sibling crate.
//!
//! NewAudio (E:\NewAudio\NewAudio) is a pure-Rust port of the
//! winscheme_sound module — PCM synthesis, ABC parser, MIDI
//! generator, and a Win32 `waveOut` + `midiOut` runtime mixer.
//! This module wraps the slice we need into JIT-callable shims so
//! Lisp programs can synthesize a sound, play it, or load an ABC
//! tune string and let it play through the GM synth.
//!
//! Lifecycle: a single Mixer is started lazily on the first audio
//! call and lives for the rest of the process. The mixer owns its
//! own threads (the PCM and MIDI runtimes spawn their own worker
//! threads); from NCL's side the only state we hold is the
//! `OnceLock<Mixer>` plus a `Mutex<Engine>` for the synthesis
//! engine (preset rendering mutates an internal RNG, so it can't
//! be `&` across threads).
//!
//! Windows-only. Non-Windows builds get a no-op stub shaped like
//! the same shim signatures (every call signals
//! "audio: Windows-only" as a condition).

use crate::word::{Tag, Word};

// Tagged-fixnum helpers used by both Windows and stub paths.
#[inline]
fn fixnum_from(n: u32) -> u64 {
    Word::fixnum(n as i64).raw()
}

#[cfg(windows)]
mod backend {
    use super::*;
    use newaudio::{Buffer, Config, Engine, Mixer, Waveform};
    use std::sync::{Mutex, OnceLock};

    /// Singleton mixer. `audio_start_shim` is the explicit entry
    /// point but every other shim transparently starts the mixer
    /// on first use too — Lisp code that just calls
    /// `(audio-coin 0.4)` and then `(audio-play it)` shouldn't
    /// have to remember a setup step.
    static MIXER: OnceLock<Option<Mixer>> = OnceLock::new();

    /// Synthesis engine. Behind a mutex because preset rendering
    /// (`coin`, `jump`, `tone`, …) advances an internal RNG, and
    /// shims may be called from multiple Lisp threads.
    static ENGINE: OnceLock<Mutex<Engine>> = OnceLock::new();

    fn mixer() -> Option<&'static Mixer> {
        let cell = MIXER.get_or_init(|| Mixer::start().ok());
        cell.as_ref()
    }

    fn engine() -> &'static Mutex<Engine> {
        ENGINE.get_or_init(|| Mutex::new(Engine::new(Config::default())))
    }

    /// Register a freshly-synthesised buffer with the PCM runtime
    /// and return its SoundId as a tagged fixnum, or NIL on
    /// "no mixer" (e.g. waveOut open failed).
    fn register(buf: Buffer) -> u64 {
        match mixer() {
            Some(m) => {
                let id = m.pcm().register_sound(buf);
                fixnum_from(id as u32)
            }
            None => Word::NIL.raw(),
        }
    }

    pub fn start() -> u64 {
        match mixer() {
            Some(_) => Word::T.raw(),
            None => Word::NIL.raw(),
        }
    }

    pub fn stop_all() -> u64 {
        if let Some(m) = mixer() {
            m.pcm().stop_all();
            m.midi().stop_all();
        }
        Word::NIL.raw()
    }

    pub fn play(id_word: Word) -> u64 {
        let id = match id_word.as_fixnum() {
            Some(n) if n >= 0 => n as u32,
            _ => return Word::NIL.raw(),
        };
        if let Some(m) = mixer() {
            m.pcm().play_simple(id);
        }
        Word::NIL.raw()
    }

    pub fn play_vol(id_word: Word, vol_word: Word, pan_word: Word) -> u64 {
        let id = match id_word.as_fixnum() {
            Some(n) if n >= 0 => n as u32,
            _ => return Word::NIL.raw(),
        };
        let v = crate::float::to_f64(vol_word).unwrap_or(1.0) as f32;
        let p = crate::float::to_f64(pan_word).unwrap_or(0.0) as f32;
        if let Some(m) = mixer() {
            m.pcm().play(id, v, p);
        }
        Word::NIL.raw()
    }

    pub fn master_volume(v_word: Word) -> u64 {
        let v = crate::float::to_f64(v_word).unwrap_or(1.0) as f32;
        if let Some(m) = mixer() {
            m.pcm().set_master_volume(v);
            m.midi().set_master_volume(v);
        }
        Word::NIL.raw()
    }

    // -- Synthesis: each preset returns a SoundId fixnum --------------------

    pub fn tone(freq_word: Word, dur_word: Word) -> u64 {
        let freq = crate::float::to_f64(freq_word).unwrap_or(440.0) as f32;
        let dur  = crate::float::to_f64(dur_word).unwrap_or(0.25) as f32;
        let buf  = engine().lock().unwrap().tone(freq, dur, Waveform::Sine);
        register(buf)
    }

    pub fn beep(freq_word: Word, dur_word: Word) -> u64 {
        let freq = crate::float::to_f64(freq_word).unwrap_or(880.0) as f32;
        let dur  = crate::float::to_f64(dur_word).unwrap_or(0.1) as f32;
        let buf  = engine().lock().unwrap().beep(freq, dur);
        register(buf)
    }

    pub fn coin(dur_word: Word) -> u64 {
        let dur = crate::float::to_f64(dur_word).unwrap_or(0.4) as f32;
        let buf = engine().lock().unwrap().coin(1.0, dur);
        register(buf)
    }

    pub fn jump(dur_word: Word) -> u64 {
        let dur = crate::float::to_f64(dur_word).unwrap_or(0.3) as f32;
        let buf = engine().lock().unwrap().jump(1.0, dur);
        register(buf)
    }

    pub fn zap(dur_word: Word) -> u64 {
        let dur = crate::float::to_f64(dur_word).unwrap_or(0.3) as f32;
        let buf = engine().lock().unwrap().zap(1.0, dur);
        register(buf)
    }

    pub fn hit(dur_word: Word) -> u64 {
        let dur = crate::float::to_f64(dur_word).unwrap_or(0.15) as f32;
        let buf = engine().lock().unwrap().bang(1.0, dur);
        register(buf)
    }

    pub fn click(dur_word: Word) -> u64 {
        let dur = crate::float::to_f64(dur_word).unwrap_or(0.05) as f32;
        let buf = engine().lock().unwrap().click(1.0, dur);
        register(buf)
    }

    pub fn blip(freq_word: Word, dur_word: Word) -> u64 {
        let freq = crate::float::to_f64(freq_word).unwrap_or(880.0) as f32;
        let dur  = crate::float::to_f64(dur_word).unwrap_or(0.1) as f32;
        let buf  = engine().lock().unwrap().blip(freq, dur);
        register(buf)
    }

    // -- ABC playback: parse + load + play in one call ---------------------

    pub fn abc_play(src_word: Word) -> u64 {
        if src_word.tag() != Tag::String {
            return Word::NIL.raw();
        }
        let src: String = crate::gc_string::chars_of(src_word).collect();
        let mut parser = newaudio::AbcParser::new();
        let tune = match parser.parse(&src) {
            Ok(t) => t,
            Err(_) => return Word::NIL.raw(),
        };
        let Some(m) = mixer() else {
            return Word::NIL.raw();
        };
        let id = m.midi().load(&tune);
        m.midi().play_simple(id);
        fixnum_from(id as u32)
    }

    pub fn abc_stop() -> u64 {
        if let Some(m) = mixer() {
            m.midi().stop_all();
        }
        Word::NIL.raw()
    }
}

#[cfg(not(windows))]
mod backend {
    use super::*;

    // Non-Windows stubs: every shim returns NIL. Sound is currently
    // Windows-only (newaudio-win wraps waveOut/midiOut); when a
    // cross-platform backend lands these become the real path.
    pub fn start() -> u64 { Word::NIL.raw() }
    pub fn stop_all() -> u64 { Word::NIL.raw() }
    pub fn play(_: Word) -> u64 { Word::NIL.raw() }
    pub fn play_vol(_: Word, _: Word, _: Word) -> u64 { Word::NIL.raw() }
    pub fn master_volume(_: Word) -> u64 { Word::NIL.raw() }
    pub fn tone(_: Word, _: Word) -> u64 { Word::NIL.raw() }
    pub fn beep(_: Word, _: Word) -> u64 { Word::NIL.raw() }
    pub fn coin(_: Word) -> u64 { Word::NIL.raw() }
    pub fn jump(_: Word) -> u64 { Word::NIL.raw() }
    pub fn zap(_: Word) -> u64 { Word::NIL.raw() }
    pub fn hit(_: Word) -> u64 { Word::NIL.raw() }
    pub fn click(_: Word) -> u64 { Word::NIL.raw() }
    pub fn blip(_: Word, _: Word) -> u64 { Word::NIL.raw() }
    pub fn abc_play(_: Word) -> u64 { Word::NIL.raw() }
    pub fn abc_stop() -> u64 { Word::NIL.raw() }
}

// ─── Lisp-ABI shims ─────────────────────────────────────────────────
//
// Every shim has the standard `extern "C-unwind" fn(mutator, env,
// args, n_args) -> u64` signature so `install_native` can drop it
// into a symbol's function cell.

macro_rules! audio_shim {
    ($name:ident, $body:expr) => {
        #[unsafe(no_mangle)]
        pub extern "C-unwind" fn $name(
            _mutator: *mut crate::mutator::MutatorState,
            _env: u64,
            _args: *const u64,
            _n_args: u64,
        ) -> u64 {
            $body
        }
    };
}

audio_shim!(audio_start_shim,    backend::start());
audio_shim!(audio_stop_all_shim, backend::stop_all());
audio_shim!(audio_abc_stop_shim, backend::abc_stop());

#[unsafe(no_mangle)]
pub extern "C-unwind" fn audio_play_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args < 1 { return Word::NIL.raw(); }
    backend::play(Word::from_raw(unsafe { *args }))
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn audio_play_vol_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args < 3 { return Word::NIL.raw(); }
    let id  = Word::from_raw(unsafe { *args });
    let vol = Word::from_raw(unsafe { *args.add(1) });
    let pan = Word::from_raw(unsafe { *args.add(2) });
    backend::play_vol(id, vol, pan)
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn audio_master_volume_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args < 1 { return Word::NIL.raw(); }
    backend::master_volume(Word::from_raw(unsafe { *args }))
}

// Two-arg synth shims (freq, dur).
macro_rules! two_arg_shim {
    ($name:ident, $body:path) => {
        #[unsafe(no_mangle)]
        pub extern "C-unwind" fn $name(
            _mutator: *mut crate::mutator::MutatorState,
            _env: u64,
            args: *const u64,
            n_args: u64,
        ) -> u64 {
            if n_args < 2 { return Word::NIL.raw(); }
            let a = Word::from_raw(unsafe { *args });
            let b = Word::from_raw(unsafe { *args.add(1) });
            $body(a, b)
        }
    };
}

two_arg_shim!(audio_tone_shim, backend::tone);
two_arg_shim!(audio_beep_shim, backend::beep);
two_arg_shim!(audio_blip_shim, backend::blip);

// One-arg preset shims (dur).
macro_rules! one_arg_shim {
    ($name:ident, $body:path) => {
        #[unsafe(no_mangle)]
        pub extern "C-unwind" fn $name(
            _mutator: *mut crate::mutator::MutatorState,
            _env: u64,
            args: *const u64,
            n_args: u64,
        ) -> u64 {
            if n_args < 1 { return Word::NIL.raw(); }
            $body(Word::from_raw(unsafe { *args }))
        }
    };
}

one_arg_shim!(audio_coin_shim,  backend::coin);
one_arg_shim!(audio_jump_shim,  backend::jump);
one_arg_shim!(audio_zap_shim,   backend::zap);
one_arg_shim!(audio_hit_shim,   backend::hit);
one_arg_shim!(audio_click_shim, backend::click);
one_arg_shim!(audio_abc_play_shim, backend::abc_play);
