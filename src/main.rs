use std::{
    collections::{HashMap, HashSet},
    ffi::CString,
    os::fd::{BorrowedFd, IntoRawFd},
    process::exit,
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use colpetto::{
    event::KeyState,
    helper::{
        Handle as LibinputHandle,
        event::{EventType, KeyboardEvent},
    },
};
use context::WgpuContext;
use input_linux_sys::{
    KEY_ESC, KEY_F1, KEY_F2, KEY_F3, KEY_F4, KEY_F5, KEY_F6, KEY_F7, KEY_F8, KEY_F9, KEY_LEFTALT,
    KEY_LEFTCTRL, KEY_RIGHTALT, KEY_RIGHTCTRL,
};
use saddle::Seat;
use tokio::{
    pin,
    sync::{RwLock, mpsc, watch},
    time::sleep,
};
use tokio_stream::{StreamExt, wrappers::WatchStream};
use tracing::{debug, error, info};

mod context;

/// Maps function keys to VT numbers
struct KeyMap {
    mappings: HashMap<u32, u32>,
}

impl KeyMap {
    fn new() -> Self {
        let mut mappings = HashMap::new();

        // Function keys mapped to respective VTs
        mappings.insert(KEY_F1 as u32, 1);
        mappings.insert(KEY_F2 as u32, 2);
        mappings.insert(KEY_F3 as u32, 3);
        mappings.insert(KEY_F4 as u32, 4);
        mappings.insert(KEY_F5 as u32, 5);
        mappings.insert(KEY_F6 as u32, 6);
        mappings.insert(KEY_F7 as u32, 7);
        mappings.insert(KEY_F8 as u32, 8);
        mappings.insert(KEY_F9 as u32, 9);

        Self { mappings }
    }

    fn get_vt(&self, key: u32) -> Option<u32> {
        self.mappings.get(&key).copied()
    }
}

struct ModifierState {
    pressed_keys: HashSet<u32>,
}

impl ModifierState {
    fn new() -> Self {
        Self {
            pressed_keys: HashSet::new(),
        }
    }

    fn update(&mut self, key: u32, state: KeyState) {
        match state {
            KeyState::Pressed => {
                self.pressed_keys.insert(key);
            }
            KeyState::Released => {
                self.pressed_keys.remove(&key);
            }
        }
    }

    fn is_ctrl_pressed(&self) -> bool {
        self.pressed_keys.contains(&(KEY_LEFTCTRL as u32))
            || self.pressed_keys.contains(&(KEY_RIGHTCTRL as u32))
    }

    fn is_alt_pressed(&self) -> bool {
        self.pressed_keys.contains(&(KEY_LEFTALT as u32))
            || self.pressed_keys.contains(&(KEY_RIGHTALT as u32))
    }

    fn is_ctrl_alt_pressed(&self) -> bool {
        self.is_ctrl_pressed() && self.is_alt_pressed()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    tokio::spawn(async {
        sleep(Duration::from_secs(10)).await;
        exit(-1)
    });

    let seat = Seat::new().await?;
    let seat_name = CString::new(seat.seat_name()).expect("Invalid seat name");

    let (libinput_handle, mut event_stream) = {
        let open_seat = seat.clone();
        let close_seat = seat.clone();

        LibinputHandle::new(
            move |path| {
                let seat = open_seat.clone();

                async move {
                    match seat.open_device(path).await {
                        Ok(fd) => fd.into_raw_fd(),
                        Err(err) => {
                            error!("Failed to open device: {err}");
                            -1
                        }
                    }
                }
            },
            move |fd| {
                let seat = close_seat.clone();

                async move {
                    let _ = seat.close_device(unsafe { BorrowedFd::borrow_raw(fd) });
                }
            },
            seat_name,
        )?
    };

    let key_map = KeyMap::new();
    let modifier_state = Arc::new(RwLock::new(ModifierState::new()));

    let (control_sx, control_rx) = watch::channel::<bool>(false);
    let libinput_control_rx = control_sx.subscribe();

    tokio::spawn({
        let seat = seat.clone();
        let libinput_handle = libinput_handle.clone();
        let modifier_state = modifier_state.clone();

        async move {
            let stream = seat.active_stream().await;

            pin!(stream);

            while let Some(is_active) = stream.try_next().await? {
                if is_active {
                    info!("Session became active, taking control");
                    seat.aquire_session().await?;
                    control_sx.send(true)?;

                    // Reset modifier state when session becomes active to avoid stuck keys
                    *modifier_state.write().await = ModifierState::new();
                    libinput_handle.resume()?;
                } else {
                    info!("Session became inactive");
                    seat.release_session().await?;
                    control_sx.send(false)?;
                    libinput_handle.suspend()?;
                }
            }

            anyhow::Ok(())
        }
    });

    let (exit_sx, mut exit_rx) = mpsc::unbounded_channel();

    tokio::spawn({
        let seat = seat.clone();

        async move {
            let mut has_control = false;
            let mut control_stream = WatchStream::new(libinput_control_rx);

            loop {
                tokio::select! {
                    Some(control) = control_stream.next() =>  has_control = control,
                    Some(event) = event_stream.next() => {
                        match event {
                            Ok(event) => match event.event_type {
                                EventType::Keyboard(KeyboardEvent::Key { key, state, .. }) => {
                                    modifier_state.write().await.update(key, state);

                                    if state == KeyState::Pressed {
                                        // Handle ESC for exit
                                        if key as i32 == KEY_ESC {
                                            libinput_handle.shutdown();
                                            if exit_sx.send(()).is_err() {
                                                break
                                            }
                                        }

                                        // Only process function keys when Ctrl+Alt are held
                                        if modifier_state.read().await.is_ctrl_alt_pressed() {
                                            if let Some(vt) = key_map.get_vt(key) {
                                                if has_control {
                                                    info!("Ctrl+Alt+F{vt} pressed, switching to VT {vt}");

                                                    if let Err(e) = seat.switch_session(vt).await {
                                                        error!("Failed to switch to VT {vt}: {e}");
                                                    }
                                                } else {
                                                    debug!("Not switching VT - session inactive");
                                                }
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            },
                            Err(_) => break,
                        }
                    }
                }
            }
        }
    });

    let mut has_control = false;
    let mut control_stream = WatchStream::new(control_rx);
    let mut render_context = None;

    loop {
        tokio::select! {
            biased;
            _ = exit_rx.recv() => {
                info!("Exiting...");
                break
            }
            Some(control) = control_stream.next() => {
                has_control = control;
            }
            else => {}  // No control changes
        };

        if has_control {
            if render_context.is_none() {
                info!("Creating rendering context");
                render_context = Some(WgpuContext::new().await?);
            }
        } else if render_context.is_some() {
            info!("Dropping rendering context");
            render_context = None;
        }

        if let Some(ref context) = render_context {
            context.present()?;
        }
    }

    Ok(())
}
