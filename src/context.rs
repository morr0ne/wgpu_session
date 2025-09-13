use anyhow::{Context, Result};
use diretto::{
    ClientCapability, Connector, Device as DrmDevice, ModeType, sys::DRM_MODE_OBJECT_PLANE,
};
use rustix::{
    fd::{AsFd, AsRawFd},
    fs::{Mode, OFlags, open},
};
use tracing::{debug, trace, warn};
use wgpu::{Backends, PresentMode, SurfaceTargetUnsafe};

#[derive(Debug)]
struct DrmState {
    device: DrmDevice,
    connector: Connector,
    mode: diretto::Mode,
    plane_id: u32,
    has_master: bool,
}

#[derive(Debug)]
struct WgpuState<'s> {
    surface: wgpu::Surface<'s>,
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

pub struct WgpuContext<'s> {
    drm_state: DrmState,
    wgpu_state: Option<WgpuState<'s>>,
}

fn open_drm_device() -> Result<DrmDevice> {
    let fd = open(
        "/dev/dri/card1",
        OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let device = unsafe { DrmDevice::new_unchecked(fd) };

    debug!("Opened DRM device /dev/dri/card1");
    Ok(device)
}

fn setup_drm_master(device: &DrmDevice) -> Result<()> {
    device.set_master().context("Failed to become DRM master")?;
    device
        .set_client_capability(ClientCapability::Atomic, true)
        .context("Failed to set atomic capability")?;
    debug!("Acquired DRM master status");
    Ok(())
}

fn release_drm_master(device: &DrmDevice) -> Result<()> {
    device.drop_master().context("Failed to drop DRM master")?;
    debug!("Released DRM master status");
    Ok(())
}

fn setup_drm_resources(device: &DrmDevice) -> Result<(Connector, diretto::Mode, u32)> {
    let resources = device.get_resources()?;

    // Find connected connector
    let connector = {
        let mut found_connector = None;
        for connector_id in &resources.connectors {
            let connector = device.get_connector(*connector_id, false)?;
            if connector.connection.is_connected() {
                found_connector = Some(connector);
                break;
            }
        }
        found_connector.ok_or_else(|| anyhow::anyhow!("No connected display found"))?
    };

    // Find best mode
    let mode = {
        let mut best_mode = None;
        let mut max_area = 0;

        for current_mode in connector.modes.iter().copied() {
            if current_mode.ty().contains(ModeType::DEFAULT) {
                best_mode = Some(current_mode);
                break;
            }

            let area = current_mode.display_width() as u32 * current_mode.display_height() as u32;
            if area > max_area {
                best_mode = Some(current_mode);
                max_area = area;
            }
        }
        best_mode.ok_or_else(|| anyhow::anyhow!("No suitable mode found"))?
    };

    debug!(
        "Selected mode {}x{}@{}",
        mode.display_width(),
        mode.display_height(),
        mode.vertical_refresh_rate()
    );

    // Find primary plane
    let plane_id = {
        let plane_resources = device.get_plane_resources()?;
        let mut primary_plane = None;

        for id in plane_resources {
            let (props, values) = unsafe { device.get_properties(id, DRM_MODE_OBJECT_PLANE)? };

            for (index, prop) in props.into_iter().enumerate() {
                let (name, _) = unsafe { device.get_property(prop)? };
                let current_value = values[index];

                if name.as_c_str() == c"type" && current_value == 1 {
                    trace!("Found primary plane: {}", id);
                    primary_plane = Some(id);
                    break;
                }
            }

            if primary_plane.is_some() {
                break;
            }
        }
        primary_plane.ok_or_else(|| anyhow::anyhow!("No primary plane found"))?
    };

    Ok((connector, mode, plane_id))
}

impl Drop for DrmState {
    fn drop(&mut self) {
        if self.has_master {
            if let Err(e) = release_drm_master(&self.device) {
                warn!("Failed to release DRM master on drop: {}", e);
            }
        }
    }
}

impl<'s> WgpuContext<'s> {
    pub async fn new() -> Result<Self> {
        let device = open_drm_device()?;
        setup_drm_master(&device)?;

        let (connector, mode, plane_id) = setup_drm_resources(&device)?;

        let drm_state = DrmState {
            device,
            connector,
            mode,
            plane_id,
            has_master: true,
        };

        let mut context = Self {
            drm_state,
            wgpu_state: None,
        };

        context.create_wgpu_resources().await?;
        Ok(context)
    }

    async fn create_wgpu_resources(&mut self) -> Result<()> {
        let surface_target = SurfaceTargetUnsafe::Drm {
            fd: self.drm_state.device.as_fd().as_raw_fd(),
            plane: self.drm_state.plane_id,
            connector_id: self.drm_state.connector.connector_id.into(),
            width: self.drm_state.mode.display_width() as u32,
            height: self.drm_state.mode.display_height() as u32,
            refresh_rate: self.drm_state.mode.vertical_refresh_rate() * 1000,
        };

        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: Backends::VULKAN,
            flags: wgpu::InstanceFlags::default()
                | wgpu::InstanceFlags::ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER,
            ..Default::default()
        });

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                ..Default::default()
            })
            .await
            .context("Failed to find an appropriate adapter")?;

        let surface = unsafe { instance.create_surface_unsafe(surface_target)? };

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: None,
                required_features: wgpu::Features::empty(),
                required_limits: adapter.limits(),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                trace: wgpu::Trace::Off,
            })
            .await
            .context("Failed to create device")?;

        let mut config = surface
            .get_default_config(
                &adapter,
                self.drm_state.mode.display_width().into(),
                self.drm_state.mode.display_height().into(),
            )
            .context("Surface not supported by adapter")?;

        config.present_mode = PresentMode::AutoVsync;
        surface.configure(&device, &config);

        self.wgpu_state = Some(WgpuState {
            surface,
            instance,
            adapter,
            device,
            queue,
        });

        debug!("Created WGPU resources");
        Ok(())
    }

    fn destroy_wgpu_resources(&mut self) {
        if self.wgpu_state.take().is_some() {
            debug!("Destroyed WGPU resources");
        }
    }

    pub fn suspend(&mut self) -> Result<()> {
        debug!("Suspending context");
        self.destroy_wgpu_resources();

        if self.drm_state.has_master {
            release_drm_master(&self.drm_state.device)?;
            self.drm_state.has_master = false;
        }

        Ok(())
    }

    pub async fn resume(&mut self) -> Result<()> {
        debug!("Resuming context");

        if !self.drm_state.has_master {
            setup_drm_master(&self.drm_state.device)?;
            self.drm_state.has_master = true;
        }

        if self.wgpu_state.is_none() {
            self.create_wgpu_resources().await?;
        }

        Ok(())
    }

    pub fn is_active(&self) -> bool {
        self.drm_state.has_master && self.wgpu_state.is_some()
    }

    pub fn present(&self) -> Result<()> {
        let wgpu_state = self
            .wgpu_state
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Cannot present: WGPU resources not available"))?;

        if !self.drm_state.has_master {
            return Err(anyhow::anyhow!("Cannot present: no DRM master status"));
        }

        let frame = wgpu_state
            .surface
            .get_current_texture()
            .context("Failed to acquire next swapchain texture")?;

        let texture_view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = wgpu_state
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());

        let renderpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &texture_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::GREEN),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });

        drop(renderpass);
        wgpu_state.queue.submit([encoder.finish()]);
        frame.present();

        Ok(())
    }
}
