//! H.264 encoding with Media Foundation transforms.
//!
//! Hardware encoders (NVIDIA NVENC, AMD AMF/VCN, Intel Quick Sync) are exposed by their
//! drivers as asynchronous MFTs that read D3D11 textures directly, so the frame never
//! touches system memory. If none works, the Microsoft software encoder that ships with
//! Windows is used, fed from a double-buffered GPU readback.

use std::mem::ManuallyDrop;

use windows::core::{Interface, Result, GUID};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;

use super::capture::Gpu;
use super::util::{err_text, pwstr_to_string};

pub struct EncodedFrame {
    /// Annex B bitstream for one access unit.
    pub data: Vec<u8>,
    pub pts: i64,
    pub key: bool,
}

/// GPU -> CPU readback for the software encoder (two staging textures so the GPU copy of
/// frame N overlaps the CPU read of frame N-1 instead of stalling).
struct Readback {
    staging: [ID3D11Texture2D; 2],
    pending: Option<(usize, i64, i64)>,
    cur: usize,
}

pub struct H264Encoder {
    mft: IMFTransform,
    activate: Option<IMFActivate>,
    events: Option<IMFMediaEventGenerator>,
    codec: Option<ICodecAPI>,
    _manager: Option<IMFDXGIDeviceManager>,
    context: ID3D11DeviceContext,
    pub name: String,
    pub hardware: bool,
    in_id: u32,
    out_id: u32,
    need_input: u32,
    provides_samples: bool,
    out_size: u32,
    out_sample: Option<IMFSample>,
    readback: Option<Readback>,
    width: u32,
    height: u32,
    fps: u32,
    frames_since_key: u32,
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

unsafe fn set_codec(codec: &ICodecAPI, api: &GUID, v: VARIANT) {
    let _ = codec.SetValue(api, &v);
}

unsafe fn configure_codec(codec: &ICodecAPI, fps: u32, bitrate: u32) {
    set_codec(codec, &CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32));
    set_codec(codec, &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate));
    set_codec(codec, &CODECAPI_AVEncCommonMaxBitRate, VARIANT::from(bitrate.saturating_mul(3) / 2));
    // One keyframe per second keeps clip cut points close to what the user asked for.
    set_codec(codec, &CODECAPI_AVEncMPVGOPSize, VARIANT::from(fps));
    set_codec(codec, &CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
    set_codec(codec, &CODECAPI_AVLowLatencyMode, VARIANT::from(true));
}

unsafe fn media_types(w: u32, h: u32, fps: u32, bitrate: u32, profile: eAVEncH264VProfile) -> Result<(IMFMediaType, IMFMediaType)> {
    let out = MFCreateMediaType()?;
    out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
    out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
    out.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
    out.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
    out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
    out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    out.SetUINT32(&MF_MT_MPEG2_PROFILE, profile.0 as u32)?;

    let inp = MFCreateMediaType()?;
    inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
    inp.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
    inp.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
    inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
    inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    Ok((out, inp))
}

unsafe fn enum_encoders(flags: MFT_ENUM_FLAG) -> Vec<IMFActivate> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    let mut out = Vec::new();
    if MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &mut ptr, &mut count).is_ok() && !ptr.is_null() {
        for i in 0..count as usize {
            if let Some(a) = std::ptr::read(ptr.add(i)) {
                out.push(a);
            }
        }
        CoTaskMemFree(Some(ptr as *const _));
    }
    out
}

unsafe fn friendly_name(a: &IMFActivate) -> String {
    let mut p = windows::core::PWSTR::null();
    let mut len = 0;
    if a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len).is_ok() {
        let s = pwstr_to_string(p);
        CoTaskMemFree(Some(p.0 as *const _));
        s
    } else {
        "H.264 encoder".into()
    }
}

/// LUID of the adapter a hardware MFT runs on, if it reports one.
unsafe fn adapter_luid(a: &IMFActivate) -> Option<u64> {
    if let Ok(v) = a.GetUINT64(&MFT_ENUM_ADAPTER_LUID) {
        return Some(v);
    }
    let mut buf = [0u8; 8];
    let mut n = 0u32;
    a.GetBlob(&MFT_ENUM_ADAPTER_LUID, &mut buf, Some(&mut n)).ok()?;
    (n == 8).then(|| u64::from_le_bytes(buf))
}

impl H264Encoder {
    /// Creates the best working encoder. `allow_hardware = false` forces the software encoder.
    pub fn new(gpu: &Gpu, w: u32, h: u32, fps: u32, bitrate: u32, allow_hardware: bool) -> std::result::Result<Self, String> {
        let mut errors = Vec::new();
        unsafe {
            if allow_hardware {
                let want = pack(gpu.luid.HighPart as u32, gpu.luid.LowPart);
                let mut candidates = enum_encoders(MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER);
                // Encoders on other GPUs cannot read our textures; try ours first, skip foreign ones.
                candidates.retain(|a| adapter_luid(a).map(|l| l == want).unwrap_or(true));
                candidates.sort_by_key(|a| adapter_luid(a).is_none());
                for a in candidates {
                    let name = friendly_name(&a);
                    match Self::open_hardware(gpu, a, &name, w, h, fps, bitrate) {
                        Ok(e) => return Ok(e),
                        Err(e) => {
                            log!("hardware encoder '{name}' unavailable: {}", err_text(&e));
                            errors.push(format!("{name}: {}", err_text(&e)));
                        }
                    }
                }
            }
            for a in enum_encoders(MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER) {
                let name = friendly_name(&a);
                match Self::open_software(gpu, a, &name, w, h, fps, bitrate) {
                    Ok(e) => return Ok(e),
                    Err(e) => {
                        log!("software encoder '{name}' unavailable: {}", err_text(&e));
                        errors.push(format!("{name}: {}", err_text(&e)));
                    }
                }
            }
        }
        if errors.is_empty() {
            Err("no H.264 encoder is installed (on Windows N editions, install the Media Feature Pack)".into())
        } else {
            Err(format!("no H.264 encoder could be started ({})", errors.join("; ")))
        }
    }

    unsafe fn base(gpu: &Gpu, mft: IMFTransform, activate: IMFActivate, name: &str, w: u32, h: u32, fps: u32) -> Result<Self> {
        let mut ins = [0u32; 1];
        let mut outs = [0u32; 1];
        let (in_id, out_id) = match mft.GetStreamIDs(&mut ins, &mut outs) {
            Ok(()) => (ins[0], outs[0]),
            Err(_) => (0, 0), // E_NOTIMPL: fixed streams numbered from zero
        };
        Ok(H264Encoder {
            mft,
            activate: Some(activate),
            events: None,
            codec: None,
            _manager: None,
            context: gpu.context.clone(),
            name: name.to_string(),
            hardware: false,
            in_id,
            out_id,
            need_input: 0,
            provides_samples: false,
            out_size: 0,
            out_sample: None,
            readback: None,
            width: w,
            height: h,
            fps,
            frames_since_key: 0,
        })
    }

    unsafe fn set_types(&mut self, bitrate: u32) -> Result<()> {
        if let Some(c) = &self.codec {
            configure_codec(c, self.fps, bitrate);
        }
        let mut last = Ok(());
        for profile in [eAVEncH264VProfile_High, eAVEncH264VProfile_Main] {
            let (out, inp) = media_types(self.width, self.height, self.fps, bitrate, profile)?;
            last = self.mft.SetOutputType(self.out_id, &out, 0).and_then(|_| self.mft.SetInputType(self.in_id, &inp, 0));
            if last.is_ok() {
                break;
            }
        }
        last?;
        if let Some(c) = &self.codec {
            configure_codec(c, self.fps, bitrate);
        }
        let info = self.mft.GetOutputStreamInfo(self.out_id)?;
        self.provides_samples = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
            != 0;
        self.out_size = info.cbSize.max(self.width * self.height * 3 / 2);
        self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
        self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        Ok(())
    }

    unsafe fn open_hardware(gpu: &Gpu, a: IMFActivate, name: &str, w: u32, h: u32, fps: u32, bitrate: u32) -> Result<Self> {
        let mft: IMFTransform = match a.ActivateObject() {
            Ok(m) => m,
            Err(e) => {
                let _ = a.ShutdownObject();
                return Err(e);
            }
        };
        let attrs = mft.GetAttributes()?;
        attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
        let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

        let mut token = 0u32;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager = manager.unwrap();
        manager.ResetDevice(&gpu.device, token)?;

        let mut enc = Self::base(gpu, mft, a, name, w, h, fps)?;
        enc.mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
        enc._manager = Some(manager);
        enc.hardware = true;
        enc.codec = enc.mft.cast::<ICodecAPI>().ok();
        enc.events = Some(enc.mft.cast::<IMFMediaEventGenerator>()?);
        enc.set_types(bitrate)?;
        log!("using hardware encoder '{name}' {w}x{h}@{fps} {} kbps", bitrate / 1000);
        Ok(enc)
    }

    unsafe fn open_software(gpu: &Gpu, a: IMFActivate, name: &str, w: u32, h: u32, fps: u32, bitrate: u32) -> Result<Self> {
        let mft: IMFTransform = a.ActivateObject()?;
        let mut enc = Self::base(gpu, mft, a, name, w, h, fps)?;
        enc.codec = enc.mft.cast::<ICodecAPI>().ok();
        enc.set_types(bitrate)?;
        let staging = |dev: &ID3D11Device| -> Result<ID3D11Texture2D> {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut t = None;
            dev.CreateTexture2D(&desc, None, Some(&mut t))?;
            Ok(t.unwrap())
        };
        enc.readback = Some(Readback { staging: [staging(&gpu.device)?, staging(&gpu.device)?], pending: None, cur: 0 });
        enc.need_input = 1;
        log!("using software encoder '{name}' {w}x{h}@{fps} {} kbps", bitrate / 1000);
        Ok(enc)
    }

    fn force_keyframe_if_due(&mut self) {
        // Some encoders ignore the GOP setting; without regular keyframes the replay buffer
        // could not drop old data, so request them explicitly.
        if self.frames_since_key >= self.fps + self.fps / 2 {
            if let Some(c) = &self.codec {
                unsafe { set_codec(c, &CODECAPI_AVEncVideoForceKeyFrame, VARIANT::from(1u32)) };
            }
            self.frames_since_key = 0;
        }
    }

    /// Submits one NV12 frame. Returns false if the encoder was not ready (frame dropped).
    pub fn submit(&mut self, tex: &ID3D11Texture2D, pts: i64, dur: i64, out: &mut Vec<EncodedFrame>) -> Result<bool> {
        if self.readback.is_some() {
            return self.submit_software(tex, pts, dur, out);
        }
        self.poll(out)?;
        if self.need_input == 0 {
            return Ok(false);
        }
        self.force_keyframe_if_due();
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
            if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                if let Ok(len) = b2.GetContiguousLength() {
                    let _ = buffer.SetCurrentLength(len);
                }
            }
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration(dur)?;
            self.mft.ProcessInput(self.in_id, &sample, 0)?;
        }
        self.need_input -= 1;
        self.frames_since_key += 1;
        Ok(true)
    }

    fn submit_software(&mut self, tex: &ID3D11Texture2D, pts: i64, dur: i64, out: &mut Vec<EncodedFrame>) -> Result<bool> {
        let (w, h) = (self.width as usize, self.height as usize);
        let rb = self.readback.as_mut().unwrap();
        let cur = rb.cur;
        unsafe { self.context.CopyResource(&rb.staging[cur], tex) };
        let prev = rb.pending.replace((cur, pts, dur));
        rb.cur ^= 1;
        let Some((idx, ppts, pdur)) = prev else { return Ok(true) };
        let staging = rb.staging[idx].clone();
        self.force_keyframe_if_due();
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let size = w * h * 3 / 2;
            let buffer = MFCreateMemoryBuffer(size as u32);
            let result = buffer.and_then(|buffer| {
                let mut dst: *mut u8 = std::ptr::null_mut();
                buffer.Lock(&mut dst, None, None)?;
                let src = mapped.pData as *const u8;
                let pitch = mapped.RowPitch as usize;
                for row in 0..h {
                    std::ptr::copy_nonoverlapping(src.add(row * pitch), dst.add(row * w), w);
                }
                let uv_src = src.add(pitch * h);
                let uv_dst = dst.add(w * h);
                for row in 0..h / 2 {
                    std::ptr::copy_nonoverlapping(uv_src.add(row * pitch), uv_dst.add(row * w), w);
                }
                buffer.Unlock()?;
                buffer.SetCurrentLength(size as u32)?;
                Ok(buffer)
            });
            self.context.Unmap(&staging, 0);
            let buffer = result?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(ppts)?;
            sample.SetSampleDuration(pdur)?;
            match self.mft.ProcessInput(self.in_id, &sample, 0) {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    self.drain_sync(out)?;
                    self.mft.ProcessInput(self.in_id, &sample, 0)?;
                }
                Err(e) => return Err(e),
            }
        }
        self.frames_since_key += 1;
        self.drain_sync(out)?;
        Ok(true)
    }

    /// Collects finished frames without blocking.
    pub fn poll(&mut self, out: &mut Vec<EncodedFrame>) -> Result<()> {
        let Some(events) = self.events.clone() else {
            return Ok(());
        };
        loop {
            let ev = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(ev) => ev,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(e),
            };
            let kind = unsafe { ev.GetType()? };
            if kind == METransformNeedInput.0 as u32 {
                self.need_input += 1;
            } else if kind == METransformHaveOutput.0 as u32 {
                self.process_output(out)?;
            }
        }
    }

    fn drain_sync(&mut self, out: &mut Vec<EncodedFrame>) -> Result<()> {
        while self.process_output(out)? {}
        Ok(())
    }

    /// Fetches one output sample. Returns false when the encoder needs more input.
    fn process_output(&mut self, out: &mut Vec<EncodedFrame>) -> Result<bool> {
        unsafe {
            let provided = if self.provides_samples {
                None
            } else {
                if self.out_sample.is_none() {
                    let s = MFCreateSample()?;
                    s.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
                    self.out_sample = Some(s);
                }
                let s = self.out_sample.clone().unwrap();
                s.GetBufferByIndex(0)?.SetCurrentLength(0)?;
                Some(s)
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.out_id,
                pSample: ManuallyDrop::new(provided),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let r = self.mft.ProcessOutput(0, &mut buffers, &mut status);
            let sample = ManuallyDrop::take(&mut buffers[0].pSample);
            drop(ManuallyDrop::take(&mut buffers[0].pEvents));
            match r {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let t = self.mft.GetOutputAvailableType(self.out_id, 0)?;
                    self.mft.SetOutputType(self.out_id, &t, 0)?;
                    return Ok(true);
                }
                Err(e) => return Err(e),
            }
            let Some(sample) = sample else { return Ok(true) };
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buffer.Unlock()?;
            if data.is_empty() {
                return Ok(true);
            }
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            if key {
                self.frames_since_key = 0;
            }
            let pts = sample.GetSampleTime().unwrap_or(0);
            out.push(EncodedFrame { data, pts, key });
            Ok(true)
        }
    }

    /// SPS/PPS from the output media type (Annex B), for encoders that do not repeat them
    /// in the bitstream.
    pub fn sequence_header(&self) -> Option<Vec<u8>> {
        unsafe {
            let t = self.mft.GetOutputCurrentType(self.out_id).ok()?;
            let size = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
            let mut buf = vec![0u8; size as usize];
            t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None).ok()?;
            Some(buf)
        }
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(s) = self.mft.cast::<IMFShutdown>() {
                let _ = s.Shutdown();
            }
            if let Some(a) = self.activate.take() {
                let _ = a.ShutdownObject();
            }
        }
    }
}
