//! Audio capture (WASAPI) and AAC encoding (Media Foundation).
//!
//! Sources:
//! - System: loopback of the default output device (everything you hear).
//! - Game: process loopback of the game and its child processes only (Windows 10 2004+),
//!   so Discord calls and music stay out of clips.
//! - Microphone: default capture device, mixed in.
//!
//! Every source is placed on one 48 kHz timeline derived from the QPC clock, the same clock
//! as video timestamps. Gaps (silence, devices that only deliver data while something
//! plays, unplugged headsets) are filled with silence so audio never drifts from video.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::{implement, Interface, Result, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, BLOB, CLSCTX_ALL};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;

use super::recorder::Shared;
use super::util::{err_text, now_hns};
use crate::core::clip::AudioParams;
use crate::core::resample::Resampler;
use crate::core::ring::{Packet, HNS};
use crate::core::settings::AUDIO_BITRATE;

pub const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
const AAC_FRAME: usize = 1024;
const WAVE_FORMAT_IEEE_FLOAT: u32 = 3;
const WAVE_FORMAT_EXTENSIBLE: u32 = 0xFFFE;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SourceKind {
    System,
    Process(u32),
    Mic,
}

#[derive(Clone, Copy)]
enum SampleFormat {
    F32,
    I16,
    I32,
}

struct Stream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    channels: usize,
    format: SampleFormat,
    resampler: Resampler,
    event: Option<HANDLE>,
    device_id: Option<String>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            if let Some(e) = self.event {
                let _ = CloseHandle(e);
            }
        }
    }
}

struct Source {
    kind: SourceKind,
    stream: Option<Stream>,
    /// Frames (at 48 kHz) placed on the timeline so far; the FIFO holds those not yet mixed.
    emitted: u64,
    fifo: VecDeque<f32>,
    retry_at: Instant,
    last_device_check: Instant,
    gain: f32,
    warned: bool,
}

fn frames_at(hns: i64) -> u64 {
    (hns.max(0) as i128 * RATE as i128 / HNS as i128) as u64
}

unsafe fn default_device(flow: EDataFlow) -> Result<IMMDevice> {
    let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    en.GetDefaultAudioEndpoint(flow, eConsole)
}

unsafe fn device_id(d: &IMMDevice) -> Option<String> {
    let p = d.GetId().ok()?;
    let s = p.to_string().ok();
    CoTaskMemFree(Some(p.0 as *const _));
    s
}

/// Parses a WAVEFORMATEX(TENSIBLE) into channel count and sample format.
unsafe fn parse_format(fmt: *const WAVEFORMATEX) -> Option<(u32, usize, SampleFormat)> {
    let f = &*fmt;
    let mut is_float = f.wFormatTag as u32 == WAVE_FORMAT_IEEE_FLOAT;
    let is_ext = f.wFormatTag as u32 == WAVE_FORMAT_EXTENSIBLE;
    if is_ext {
        let ext = &*(fmt as *const WAVEFORMATEXTENSIBLE);
        let sub = ext.SubFormat;
        is_float = sub.data1 == 3; // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    }
    let format = match (is_float, f.wBitsPerSample) {
        (true, 32) => SampleFormat::F32,
        (false, 16) => SampleFormat::I16,
        (false, 32) => SampleFormat::I32,
        _ => return None,
    };
    Some((f.nSamplesPerSec, f.nChannels as usize, format))
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    done: HANDLE,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(&self, _op: windows::core::Ref<IActivateAudioInterfaceAsyncOperation>) -> Result<()> {
        unsafe { SetEvent(self.done) }
    }
}

/// Activates a process-loopback audio client for `pid` and its child processes.
unsafe fn process_loopback_client(pid: u32) -> Result<IAudioClient> {
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    let pv = PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: &mut params as *mut _ as *mut u8,
                    },
                },
            }),
        },
    };
    let done = CreateEventW(None, true, false, PCWSTR::null())?;
    let handler: IActivateAudioInterfaceCompletionHandler = ActivationHandler { done }.into();
    let result = (|| {
        let op = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&pv), &handler)?;
        if WaitForSingleObject(done, 5000) != WAIT_OBJECT_0 {
            log!("process loopback activation timed out");
            return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::ERROR_TIMEOUT.to_hresult()));
        }
        let mut hr = windows::core::HRESULT(0);
        let mut unk = None;
        op.GetActivateResult(&mut hr, &mut unk)?;
        hr.ok()?;
        unk.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_NOINTERFACE))?.cast::<IAudioClient>()
    })();
    // The PROPVARIANT points at stack memory; never let it be freed as a real blob.
    std::mem::forget(pv);
    let _ = CloseHandle(done);
    result
}

unsafe fn open_stream(kind: SourceKind) -> Result<Stream> {
    const BUFFER: i64 = 2_000_000; // 200 ms
    match kind {
        SourceKind::Process(pid) => {
            let client = process_loopback_client(pid)?;
            // Process loopback has no mix format; ask for 48 kHz stereo 16-bit and let
            // the audio engine convert.
            let fmt = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_PCM as u16,
                nChannels: 2,
                nSamplesPerSec: RATE,
                nAvgBytesPerSec: RATE * 4,
                nBlockAlign: 4,
                wBitsPerSample: 16,
                cbSize: 0,
            };
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                BUFFER,
                0,
                &fmt,
                None,
            )?;
            let event = CreateEventW(None, false, false, PCWSTR::null())?;
            client.SetEventHandle(event)?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok(Stream { client, capture, channels: 2, format: SampleFormat::I16, resampler: Resampler::new(RATE), event: Some(event), device_id: None })
        }
        SourceKind::System | SourceKind::Mic => {
            let flow = if kind == SourceKind::Mic { eCapture } else { eRender };
            let device = default_device(flow)?;
            let id = device_id(&device);
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let mix = client.GetMixFormat()?;
            let parsed = parse_format(mix);
            let flags = if kind == SourceKind::System { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
            let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER, 0, mix, None);
            CoTaskMemFree(Some(mix as *const _));
            init?;
            let (rate, channels, format) = parsed.ok_or_else(|| {
                windows::core::Error::from_hresult(AUDCLNT_E_UNSUPPORTED_FORMAT)
            })?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok(Stream { client, capture, channels, format, resampler: Resampler::new(rate), event: None, device_id: id })
        }
    }
}

impl Source {
    fn new(kind: SourceKind, gain: f32) -> Source {
        let now = Instant::now();
        Source { kind, stream: None, emitted: 0, fifo: VecDeque::new(), retry_at: now, last_device_check: now, gain, warned: false }
    }

    fn silence(&mut self, frames: u64) {
        self.fifo.extend(std::iter::repeat(0.0).take(frames as usize * CHANNELS));
        self.emitted += frames;
    }

    /// Reads everything available and keeps the timeline up to `now`.
    fn pump(&mut self, start: i64, now: i64, notes: &mut Vec<String>) {
        if self.stream.is_none() && Instant::now() >= self.retry_at {
            match unsafe { open_stream(self.kind) } {
                Ok(s) => {
                    log!("audio source {:?} started", self.kind);
                    self.stream = Some(s);
                    self.warned = false;
                }
                Err(e) => {
                    if !self.warned {
                        log!("audio source {:?} unavailable: {}", self.kind, err_text(&e));
                        notes.push(match self.kind {
                            SourceKind::Mic => "Microphone unavailable".into(),
                            SourceKind::Process(_) => "Game-only audio unavailable".into(),
                            SourceKind::System => "No audio output device".into(),
                        });
                        self.warned = true;
                    }
                    self.retry_at = Instant::now() + Duration::from_secs(3);
                }
            }
        }
        // Follow the default device when the user switches outputs or microphones.
        if self.last_device_check.elapsed() > Duration::from_secs(3) {
            self.last_device_check = Instant::now();
            if let Some(s) = &self.stream {
                if let Some(old) = &s.device_id {
                    let flow = if self.kind == SourceKind::Mic { eCapture } else { eRender };
                    let current = unsafe { default_device(flow).ok().and_then(|d| device_id(&d)) };
                    if current.as_ref().map(|c| c != old).unwrap_or(false) {
                        log!("default audio device changed; reopening {:?}", self.kind);
                        self.stream = None;
                        self.retry_at = Instant::now();
                    }
                }
            }
        }

        let mut got = false;
        if let Some(stream) = self.stream.as_mut() {
            if let Err(e) = read_packets(stream, start, &mut self.emitted, &mut self.fifo, self.gain, &mut got) {
                log!("audio source {:?} lost: {}", self.kind, err_text(&e));
                self.stream = None;
                self.retry_at = Instant::now() + Duration::from_millis(500);
            }
        }
        // Devices deliver nothing while silent: advance the timeline with silence, staying
        // 200 ms behind real time so late packets still land in the right place.
        let floor = frames_at(now - start - 2_000_000);
        if floor > self.emitted && (!got || self.stream.is_none()) {
            let n = floor - self.emitted;
            self.silence(n);
        }
    }
}

fn read_packets(s: &mut Stream, start: i64, emitted: &mut u64, fifo: &mut VecDeque<f32>, gain: f32, got: &mut bool) -> Result<()> {
    let mut scratch: Vec<f32> = Vec::new();
    unsafe {
        if let Some(e) = s.event {
            let _ = WaitForSingleObject(e, 0);
        }
        loop {
            let n = s.capture.GetNextPacketSize()?;
            if n == 0 {
                return Ok(());
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            let mut qpc = 0u64;
            s.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
            let count = frames as usize;
            scratch.clear();
            scratch.reserve(count * 2);
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();
            let ch = s.channels.max(1);
            for i in 0..count {
                if silent {
                    scratch.extend_from_slice(&[0.0, 0.0]);
                    continue;
                }
                let sample = |c: usize| -> f32 {
                    let idx = i * ch + c.min(ch - 1);
                    match s.format {
                        SampleFormat::F32 => *(data as *const f32).add(idx),
                        SampleFormat::I16 => *(data as *const i16).add(idx) as f32 / 32768.0,
                        SampleFormat::I32 => *(data as *const i32).add(idx) as f32 / 2_147_483_648.0,
                    }
                };
                scratch.push(sample(0) * gain);
                scratch.push(sample(1) * gain);
            }
            s.capture.ReleaseBuffer(frames)?;
            *got = true;

            // Place the packet on the shared timeline using its QPC capture time.
            let expected = if qpc != 0 { frames_at(qpc as i64 - start) } else { *emitted };
            let mut skip = 0usize;
            if expected > *emitted + 960 {
                let gap = expected - *emitted;
                fifo.extend(std::iter::repeat(0.0).take(gap as usize * CHANNELS));
                *emitted += gap;
            } else if expected + 2400 < *emitted {
                // We are ahead (silence was filled where this packet belongs): drop the overlap.
                skip = ((*emitted - expected) as usize).min(count) * 2;
            }
            let produced = s.resampler.run(&scratch[skip..], fifo);
            *emitted += produced as u64;
        }
    }
}

/// Media Foundation AAC-LC encoder (ships with Windows).
struct AacEncoder {
    mft: IMFTransform,
    out_sample: IMFSample,
}

impl AacEncoder {
    unsafe fn new() -> Result<AacEncoder> {
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_PCM };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_AAC };
        let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(MFT_CATEGORY_AUDIO_ENCODER, MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER, Some(&input), Some(&output), &mut ptr, &mut count)?;
        let mut activates = Vec::new();
        for i in 0..count as usize {
            if let Some(a) = std::ptr::read(ptr.add(i)) {
                activates.push(a);
            }
        }
        if !ptr.is_null() {
            CoTaskMemFree(Some(ptr as *const _));
        }
        let a = activates
            .into_iter()
            .next()
            .ok_or_else(|| windows::core::Error::from_hresult(MF_E_TOPO_CODEC_NOT_FOUND))?;
        let mft: IMFTransform = a.ActivateObject()?;

        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
        out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE)?;
        out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
        out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_BITRATE / 8)?;
        out.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?; // raw AAC, no ADTS headers
        out.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
        mft.SetOutputType(0, &out, 0)?;

        let inp = MFCreateMediaType()?;
        inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        inp.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        inp.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        inp.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE)?;
        inp.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
        inp.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, (CHANNELS * 2) as u32)?;
        inp.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, RATE * CHANNELS as u32 * 2)?;
        mft.SetInputType(0, &inp, 0)?;

        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        let size = mft.GetOutputStreamInfo(0)?.cbSize.max(8192);
        let out_sample = MFCreateSample()?;
        out_sample.AddBuffer(&MFCreateMemoryBuffer(size)?)?;
        Ok(AacEncoder { mft, out_sample })
    }

    /// Encodes interleaved 16-bit PCM; appends finished raw AAC frames to `out`.
    unsafe fn encode(&mut self, pcm: &[i16], pts: i64, out: &mut Vec<Vec<u8>>) -> Result<()> {
        let bytes = pcm.len() * 2;
        let buffer = MFCreateMemoryBuffer(bytes as u32)?;
        let mut dst: *mut u8 = std::ptr::null_mut();
        buffer.Lock(&mut dst, None, None)?;
        std::ptr::copy_nonoverlapping(pcm.as_ptr() as *const u8, dst, bytes);
        buffer.Unlock()?;
        buffer.SetCurrentLength(bytes as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(pts)?;
        sample.SetSampleDuration((pcm.len() / CHANNELS) as i64 * HNS / RATE as i64)?;
        self.mft.ProcessInput(0, &sample, 0)?;
        loop {
            self.out_sample.GetBufferByIndex(0)?.SetCurrentLength(0)?;
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(Some(self.out_sample.clone())),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0;
            let r = self.mft.ProcessOutput(0, &mut buffers, &mut status);
            drop(ManuallyDrop::take(&mut buffers[0].pSample));
            drop(ManuallyDrop::take(&mut buffers[0].pEvents));
            match r {
                Ok(()) => {
                    let b = self.out_sample.ConvertToContiguousBuffer()?;
                    let mut p: *mut u8 = std::ptr::null_mut();
                    let mut len = 0u32;
                    b.Lock(&mut p, None, Some(&mut len))?;
                    let data = std::slice::from_raw_parts(p, len as usize).to_vec();
                    b.Unlock()?;
                    if !data.is_empty() {
                        out.push(data);
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }
}

/// Audio thread body: runs until `shared.stop` is set.
pub fn run(shared: Arc<Shared>, sources: Vec<SourceKind>) {
    log!("audio: starting AAC encoder for {:?}", sources);
    let mut encoder = match unsafe { AacEncoder::new() } {
        Ok(e) => e,
        Err(e) => {
            log!("AAC encoder unavailable: {}", err_text(&e));
            shared.set_note("Audio disabled: no AAC encoder (Windows N editions need the Media Feature Pack)");
            return;
        }
    };
    *shared.audio.lock().unwrap() =
        Some(AudioParams { sample_rate: RATE, channels: CHANNELS as u16, bitrate: AUDIO_BITRATE, frame_len: AAC_FRAME as u32 });

    let start = shared.start_hns;
    // Microphones are usually much quieter than game audio in loopback; keep both at unity
    // gain and clamp on mix rather than guessing levels.
    let mut srcs: Vec<Source> = sources.iter().map(|&k| Source::new(k, 1.0)).collect();
    let mut mixed: u64 = 0;
    // Output frame k covers input samples [k*1024, (k+1)*1024) of the session timeline.
    let mut encoded: i64 = 0;
    let mut pcm: Vec<i16> = Vec::with_capacity(AAC_FRAME * CHANNELS);
    let mut packets = Vec::new();
    let frame_dur = AAC_FRAME as i64 * HNS / RATE as i64;

    while !shared.stop.load(Ordering::Relaxed) {
        let now = now_hns();
        let mut notes = Vec::new();
        for s in srcs.iter_mut() {
            s.pump(start, now, &mut notes);
        }
        for n in notes {
            shared.set_note(&n);
        }
        // Mix in AAC-frame-sized chunks once every source has data for them.
        loop {
            let ready = srcs.iter().map(|s| s.fifo.len() / CHANNELS).min().unwrap_or(0);
            if ready < AAC_FRAME {
                break;
            }
            pcm.clear();
            for _ in 0..AAC_FRAME * CHANNELS {
                let mut v = 0.0f32;
                for s in srcs.iter_mut() {
                    v += s.fifo.pop_front().unwrap_or(0.0);
                }
                pcm.push((v.clamp(-1.0, 1.0) * 32767.0) as i16);
            }
            let pts = (mixed as i128 * HNS as i128 / RATE as i128) as i64;
            mixed += AAC_FRAME as u64;
            packets.clear();
            if let Err(e) = unsafe { encoder.encode(&pcm, pts, &mut packets) } {
                log!("AAC encode failed: {}", err_text(&e));
                shared.set_note("Audio encoder failed; audio disabled for this session");
                *shared.audio.lock().unwrap() = None;
                shared.ring.lock().unwrap().clear();
                return;
            }
            let mut ring = shared.ring.lock().unwrap();
            for data in packets.drain(..) {
                ring.push_audio(Packet { pts: encoded * AAC_FRAME as i64 * HNS / RATE as i64, dur: frame_dur, key: true, data: Arc::from(data) });
                encoded += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
