//! Screen capture with DXGI Desktop Duplication, plus GPU colour conversion and scaling
//! (BGRA -> NV12) with the D3D11 video processor. Frames never leave the GPU.
//!
//! Desktop Duplication works with fullscreen, borderless and windowed games, does not
//! inject anything into the game process, and has no capture border.

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use windows::core::{Interface, Result};
use windows::Win32::Foundation::{HMODULE, LUID, RECT};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::HMONITOR;

use super::util::from_wide;

pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub adapter_name: String,
    pub luid: LUID,
}

/// Creates a D3D11 device on the adapter that drives `monitor` (Desktop Duplication only
/// works from the adapter the output is attached to; this matters on hybrid laptops).
pub fn open_gpu_for_monitor(monitor: HMONITOR) -> Result<(Gpu, IDXGIOutput1)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut fallback: Option<(IDXGIAdapter1, IDXGIOutput)> = None;
        let mut found: Option<(IDXGIAdapter1, IDXGIOutput)> = None;
        let mut ai = 0;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            let mut oi = 0;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                let desc = output.GetDesc()?;
                if desc.Monitor == monitor {
                    found = Some((adapter.clone(), output.clone()));
                }
                if fallback.is_none() && desc.AttachedToDesktop.as_bool() {
                    fallback = Some((adapter.clone(), output.clone()));
                }
                oi += 1;
            }
            ai += 1;
        }
        let (adapter, output) = found
            .or(fallback)
            .ok_or_else(|| windows::core::Error::from_hresult(DXGI_ERROR_NOT_FOUND))?;
        let adesc = adapter.GetDesc1()?;

        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0];
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        let device: ID3D11Device = device.unwrap();
        let context: ID3D11DeviceContext = context.unwrap();
        // The Media Foundation encoder uses the device from its own threads.
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
        Ok((
            Gpu { device, context, adapter_name: from_wide(&adesc.Description), luid: adesc.AdapterLuid },
            output.cast()?,
        ))
    }
}

/// D3D11 video processor converting the captured desktop into the encoder's NV12 frames.
struct Converter {
    vctx: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    outputs: Vec<ID3D11VideoProcessorOutputView>,
}

pub struct DesktopCapture {
    pub gpu: Gpu,
    output: IDXGIOutput1,
    dup: Option<IDXGIOutputDuplication>,
    rotation: DXGI_MODE_ROTATION,
    /// Our copy of the latest desktop image (BGRA), and its size.
    frame: Option<(ID3D11Texture2D, u32, u32, DXGI_FORMAT)>,
    has_image: bool,
    last_dup_attempt: Option<Instant>,
    conv: Option<Converter>,
    pool: Vec<ID3D11Texture2D>,
    next: usize,
    out_w: u32,
    out_h: u32,
    fps: u32,
}

/// Number of NV12 frames in flight. The hardware encoder reads them asynchronously, so a
/// texture is only reused after the encoder has had several frame times to consume it.
const POOL: usize = 8;

impl DesktopCapture {
    pub fn new(gpu: Gpu, output: IDXGIOutput1) -> Result<DesktopCapture> {
        let mut cap = DesktopCapture {
            gpu,
            output,
            dup: None,
            rotation: DXGI_MODE_ROTATION_IDENTITY,
            frame: None,
            has_image: false,
            last_dup_attempt: None,
            conv: None,
            pool: Vec::new(),
            next: 0,
            out_w: 0,
            out_h: 0,
            fps: 60,
        };
        cap.duplicate()?;
        Ok(cap)
    }

    /// Logical desktop size of the output (after rotation).
    pub fn desktop_size(&self) -> Result<(u32, u32)> {
        let d = unsafe { self.output.GetDesc()? };
        let r = d.DesktopCoordinates;
        Ok(((r.right - r.left).max(2) as u32, (r.bottom - r.top).max(2) as u32))
    }

    fn duplicate(&mut self) -> Result<()> {
        self.last_dup_attempt = Some(Instant::now());
        self.dup = None;
        unsafe {
            let formats = [DXGI_FORMAT_B8G8R8A8_UNORM];
            // DuplicateOutput1 converts HDR/10-bit desktops to 8-bit for us.
            let dup = match self.output.cast::<IDXGIOutput5>() {
                Ok(o5) => match o5.DuplicateOutput1(&self.gpu.device, 0, &formats) {
                    Ok(d) => d,
                    Err(_) => self.output.DuplicateOutput(&self.gpu.device)?,
                },
                Err(_) => self.output.DuplicateOutput(&self.gpu.device)?,
            };
            let desc = dup.GetDesc();
            self.rotation = desc.Rotation;
            self.dup = Some(dup);
        }
        Ok(())
    }

    /// Fetches the newest desktop image if it changed. Returns Err only for device-level
    /// failures that require recreating the whole pipeline.
    pub fn grab(&mut self) -> Result<()> {
        if self.dup.is_none() {
            // Duplication is lost during UAC prompts, the lock screen, mode changes and
            // exclusive fullscreen transitions; retry a few times per second.
            if self.last_dup_attempt.map(|t| t.elapsed() < Duration::from_millis(250)).unwrap_or(false) {
                return Ok(());
            }
            match self.duplicate() {
                Ok(()) => {}
                Err(e) if is_device_lost(&e) => return Err(e),
                Err(e) if e.code() == DXGI_ERROR_UNSUPPORTED => return Err(e),
                Err(_) => return Ok(()),
            }
        }
        let dup = self.dup.clone().unwrap();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        unsafe {
            match dup.AcquireNextFrame(0, &mut info, &mut resource) {
                Ok(()) => {
                    let result = if info.LastPresentTime != 0 || !self.has_image {
                        match resource.as_ref().map(|r| r.cast::<ID3D11Texture2D>()) {
                            Some(Ok(tex)) => self.copy_frame(&tex),
                            _ => Ok(()),
                        }
                    } else {
                        Ok(())
                    };
                    let _ = dup.ReleaseFrame();
                    result
                }
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(()),
                Err(e) if is_device_lost(&e) => Err(e),
                Err(_) => {
                    // ACCESS_LOST / INVALID_CALL: recreate the duplication next time.
                    self.dup = None;
                    Ok(())
                }
            }
        }
    }

    unsafe fn copy_frame(&mut self, src: &ID3D11Texture2D) -> Result<()> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        src.GetDesc(&mut desc);
        let matches = matches!(&self.frame, Some((_, w, h, f)) if *w == desc.Width && *h == desc.Height && *f == desc.Format);
        if !matches {
            let own = D3D11_TEXTURE2D_DESC {
                Width: desc.Width,
                Height: desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: desc.Format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex = None;
            self.gpu.device.CreateTexture2D(&own, None, Some(&mut tex))?;
            self.frame = Some((tex.unwrap(), desc.Width, desc.Height, desc.Format));
            self.conv = None; // input size changed: rebuild the converter
        }
        let (own, ..) = self.frame.as_ref().unwrap();
        self.gpu.context.CopyResource(own, src);
        self.has_image = true;
        Ok(())
    }

    /// Sets the encoder frame size. Must be called before `convert`.
    pub fn set_output(&mut self, w: u32, h: u32, fps: u32) -> Result<()> {
        self.out_w = w;
        self.out_h = h;
        self.fps = fps;
        self.conv = None;
        self.pool.clear();
        for _ in 0..POOL {
            self.pool.push(create_nv12(&self.gpu.device, w, h)?);
        }
        Ok(())
    }

    fn build_converter(&self) -> Result<Converter> {
        let (input_tex, src_w, src_h, _) = self.frame.as_ref().unwrap();
        unsafe {
            let vdev: ID3D11VideoDevice = self.gpu.device.cast()?;
            let vctx: ID3D11VideoContext = self.gpu.context.cast()?;
            let rate = DXGI_RATIONAL { Numerator: self.fps, Denominator: 1 };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: *src_w,
                InputHeight: *src_h,
                OutputFrameRate: rate,
                OutputWidth: self.out_w,
                OutputHeight: self.out_h,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let enumerator = vdev.CreateVideoProcessorEnumerator(&desc)?;
            let vp = vdev.CreateVideoProcessor(&enumerator, 0)?;

            let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                },
            };
            let mut input = None;
            vdev.CreateVideoProcessorInputView(input_tex, &enumerator, &in_desc, Some(&mut input))?;

            let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut outputs = Vec::with_capacity(self.pool.len());
            for tex in &self.pool {
                let mut view = None;
                vdev.CreateVideoProcessorOutputView(tex, &enumerator, &out_desc, Some(&mut view))?;
                outputs.push(view.unwrap());
            }

            // Full-range sRGB in, BT.709 limited-range YUV out (what players expect for HD).
            if let Ok(vctx1) = vctx.cast::<ID3D11VideoContext1>() {
                vctx1.VideoProcessorSetStreamColorSpace1(&vp, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                vctx1.VideoProcessorSetOutputColorSpace1(&vp, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            } else {
                // Bitfields: Usage, RGB_Range, YCbCr_Matrix, YCbCr_xvYCC, Nominal_Range(2).
                let input_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (2 << 4) };
                let output_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
                vctx.VideoProcessorSetStreamColorSpace(&vp, 0, &input_cs);
                vctx.VideoProcessorSetOutputColorSpace(&vp, &output_cs);
            }
            vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);

            // Rotated displays: the duplicated image is in scan-out orientation.
            let rotated = matches!(self.rotation, DXGI_MODE_ROTATION_ROTATE90 | DXGI_MODE_ROTATION_ROTATE270);
            let rot = match self.rotation {
                DXGI_MODE_ROTATION_ROTATE90 => Some(D3D11_VIDEO_PROCESSOR_ROTATION_270),
                DXGI_MODE_ROTATION_ROTATE180 => Some(D3D11_VIDEO_PROCESSOR_ROTATION_180),
                DXGI_MODE_ROTATION_ROTATE270 => Some(D3D11_VIDEO_PROCESSOR_ROTATION_90),
                _ => None,
            };
            if let Some(r) = rot {
                vctx.VideoProcessorSetStreamRotation(&vp, 0, true, r);
            }

            // Letterbox to preserve aspect ratio if the desktop size no longer matches.
            let (lw, lh) = if rotated { (*src_h, *src_w) } else { (*src_w, *src_h) };
            let dest = fit_rect(lw, lh, self.out_w, self.out_h);
            let src_rect = RECT { left: 0, top: 0, right: *src_w as i32, bottom: *src_h as i32 };
            vctx.VideoProcessorSetStreamSourceRect(&vp, 0, true, Some(&src_rect));
            vctx.VideoProcessorSetStreamDestRect(&vp, 0, true, Some(&dest));
            let full = RECT { left: 0, top: 0, right: self.out_w as i32, bottom: self.out_h as i32 };
            vctx.VideoProcessorSetOutputTargetRect(&vp, true, Some(&full));
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 { YCbCr: D3D11_VIDEO_COLOR_YCbCrA { Y: 0.0625, Cb: 0.5, Cr: 0.5, A: 1.0 } },
            };
            vctx.VideoProcessorSetOutputBackgroundColor(&vp, true, &black);

            Ok(Converter { vctx, enumerator, vp, input: input.unwrap(), outputs })
        }
    }

    /// Converts the latest desktop image into the next NV12 pool texture. Returns `None`
    /// until the first image has been captured.
    pub fn convert(&mut self) -> Result<Option<ID3D11Texture2D>> {
        if !self.has_image || self.pool.is_empty() {
            return Ok(None);
        }
        if self.conv.is_none() {
            self.conv = Some(self.build_converter()?);
        }
        let conv = self.conv.as_ref().unwrap();
        let i = self.next;
        self.next = (self.next + 1) % self.pool.len();
        unsafe {
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(conv.input.clone())),
                ..Default::default()
            };
            let mut streams = [stream];
            let r = conv.vctx.VideoProcessorBlt(&conv.vp, &conv.outputs[i], 0, &streams);
            ManuallyDrop::drop(&mut streams[0].pInputSurface);
            r?;
        }
        let _ = &conv.enumerator;
        Ok(Some(self.pool[i].clone()))
    }
}

fn fit_rect(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> RECT {
    let (sw, sh, dw, dh) = (src_w as u64, src_h as u64, dst_w as u64, dst_h as u64);
    let (w, h) = if sw * dh > dw * sh { (dw, (dw * sh / sw) & !1) } else { ((dh * sw / sh) & !1, dh) };
    let x = (dw - w) / 2;
    let y = (dh - h) / 2;
    RECT { left: x as i32, top: y as i32, right: (x + w) as i32, bottom: (y + h) as i32 }
}

pub fn create_nv12(device: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D> {
    let mut desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        // Some encoders (Intel) want the video-encoder bind flag; not every driver allows it.
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe {
        if device.CreateTexture2D(&desc, None, Some(&mut tex)).is_err() {
            desc.BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32;
            device.CreateTexture2D(&desc, None, Some(&mut tex))?;
        }
    }
    Ok(tex.unwrap())
}

pub fn is_device_lost(e: &windows::core::Error) -> bool {
    let c = e.code();
    c == DXGI_ERROR_DEVICE_REMOVED || c == DXGI_ERROR_DEVICE_RESET || c == DXGI_ERROR_DEVICE_HUNG || c == DXGI_ERROR_DRIVER_INTERNAL_ERROR
}
