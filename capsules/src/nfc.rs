use core::cell::Cell;
use core::cmp;
use kernel::common::cells::{OptionalCell, TakeCell};
use kernel::debug;
use kernel::hil::nfc;
use kernel::{AppId, AppSlice, Callback, Driver, Grant, ReturnCode, Shared};

/// Syscall driver number.
use crate::driver;
pub const DRIVER_NUM: usize = driver::NUM::NFC as usize;

#[derive(Default)]
pub struct App {
    tx_callback: Option<Callback>,
    tx_buffer: Option<AppSlice<Shared, u8>>,
    rx_callback: Option<Callback>,
    rx_buffer: Option<AppSlice<Shared, u8>>,
}

pub const MAX_LENGTH: usize = 256;

pub struct NfcDriver<'a> {
    driver: &'a dyn nfc::NfcTag<'a>,
    application: Grant<App>,
    tx_in_progress: OptionalCell<AppId>,
    tx_buffer: TakeCell<'static, [u8]>,
    rx_in_progress: OptionalCell<AppId>,
    rx_buffer: TakeCell<'static, [u8]>,
    driver_selected: Cell<bool>,
}

impl<'a> NfcDriver<'a> {
    pub fn new(
        driver: &'a dyn nfc::NfcTag<'a>,
        tx_buffer: &'static mut [u8; MAX_LENGTH],
        rx_buffer: &'static mut [u8; MAX_LENGTH],
        grant: Grant<App>,
    ) -> NfcDriver<'a> {
        NfcDriver {
            driver: driver,
            application: grant,
            tx_in_progress: OptionalCell::empty(),
            tx_buffer: TakeCell::new(tx_buffer),
            rx_in_progress: OptionalCell::empty(),
            rx_buffer: TakeCell::new(rx_buffer),
            driver_selected: Cell::new(false),
        }
    }

    /// Internal helper function for setting up frame transmission
    fn transmit_new(&self, app_id: AppId, app: &mut App, len: usize) -> ReturnCode {
        // Driver not ready yet
        if !self.driver_selected.get() {
            return ReturnCode::EOFF;
        }
        match app.tx_buffer.take() {
            Some(slice) => {
                self.transmit(app_id, app, slice, len);
                ReturnCode::SUCCESS
            }
            None => ReturnCode::EBUSY,
        }
    }

    /// Internal helper function for data transmission
    fn transmit(&self, app_id: AppId, app: &mut App, slice: AppSlice<Shared, u8>, len: usize) {
        if self.tx_in_progress.is_none() {
            self.tx_in_progress.set(app_id);
            self.tx_buffer.take().map(|buffer| {
                for (i, c) in slice.as_ref().iter().enumerate() {
                    buffer[i] = *c;
                }
                self.driver.transmit_buffer(buffer, len);
            });
        } else {
            app.tx_buffer = Some(slice);
        }
    }

    /// Internal helper function for starting a receive operation
    fn receive_new(&self, app_id: AppId, app: &mut App, _len: usize) -> ReturnCode {
        // Driver not ready yet
        if !self.driver_selected.get() {
            return ReturnCode::EOFF;
        }
        if self.rx_buffer.is_none() {
            return ReturnCode::EBUSY;
        }

        if app.rx_buffer.is_some() {
            self.rx_buffer.take().map(|buffer| {
                self.rx_in_progress.set(app_id);
                self.driver.receive_buffer(buffer);
            });
            ReturnCode::SUCCESS
        } else {
            debug!(" >> FAIL: no application buffer supplied!");
            // Must supply buffer before performing receive operation
            ReturnCode::EINVAL
        }
    }
}

impl<'a> nfc::Client<'a> for NfcDriver<'a> {
    fn tag_selected(&self) {
        self.driver_selected.set(true);
        // 0xfffff results in 1048575 / 13.56e6 = 77ms
        // The anticollision is finished, we can now
        // set the frame delay to the maximum value
        self.driver.set_framedelaymax(0xfffff);
    }

    fn field_detected(&self) {}

    fn field_lost(&self) {
        self.driver_selected.set(false);
    }

    fn frame_received(&self, buffer: &'static mut [u8], rx_len: usize, result: ReturnCode) {
        self.rx_buffer.replace(buffer);
        self.rx_in_progress.take().map(|appid| {
            let _ = self.application.enter(appid, |app, _| {
                app.rx_buffer = app.rx_buffer.take().map(|mut rb| {
                    // Figure out length to copy.
                    let max_len = cmp::min(rx_len, rb.len());
                    // Copy over data to app buffer.
                    self.rx_buffer.map(|buffer| {
                        for idx in 0..max_len {
                            rb.as_mut()[idx] = buffer[idx];
                        }
                    });
                    app.rx_callback
                        .map(|mut cb| cb.schedule(result.into(), max_len, 0));
                    rb
                });
            });
        });
    }

    fn frame_transmitted(&self, buffer: &'static mut [u8], result: ReturnCode) {
        self.tx_buffer.replace(buffer);
        self.tx_in_progress.take().map(|appid| {
            let _ = self.application.enter(appid, |app, _| {
                app.tx_callback
                    .map(|mut cb| cb.schedule(result.into(), 0, 0));
            });
        });
    }
}

impl Driver for NfcDriver<'_> {
    /// Setup shared buffers.
    ///
    /// ### `allow_num`
    ///
    /// - `1`: Readable buffer for transmission buffer, if
    ///        provided buffer length is more than MAX_LENGTH then
    ///        return EINVAL
    /// - `2`: Writeable buffer for reception buffer, if
    ///        provided buffer length is not MAX_LENGTH then
    ///        return EINVAL
    fn allow(
        &self,
        appid: AppId,
        allow_num: usize,
        slice: Option<AppSlice<Shared, u8>>,
    ) -> ReturnCode {
        match allow_num {
            1 => self
                .application
                .enter(appid, |app, _| {
                    if let Some(buf) = &slice {
                        if buf.len() > MAX_LENGTH {
                            return ReturnCode::EINVAL;
                        }
                    }
                    app.tx_buffer = slice;
                    ReturnCode::SUCCESS
                })
                .unwrap_or_else(|err| err.into()),
            2 => self
                .application
                .enter(appid, |app, _| {
                    if let Some(buf) = &slice {
                        if buf.len() != MAX_LENGTH {
                            return ReturnCode::EINVAL;
                        }
                    }
                    app.rx_buffer = slice;
                    ReturnCode::SUCCESS
                })
                .unwrap_or_else(|err| err.into()),
            _ => ReturnCode::ENOSUPPORT,
        }
    }

    /// Setup callbacks.
    ///
    /// ### `subscribe_num`
    ///
    /// - `1`: Frame transmission completed callback
    /// - `2`: Frame reception completed callback
    fn subscribe(
        &self,
        subscribe_num: usize,
        callback: Option<Callback>,
        appid: AppId,
    ) -> ReturnCode {
        match subscribe_num {
            1 => self
                .application
                .enter(appid, |app, _| {
                    app.tx_callback = callback;
                    ReturnCode::SUCCESS
                })
                .unwrap_or_else(|err| err.into()),
            2 => self
                .application
                .enter(appid, |app, _| {
                    app.rx_callback = callback;
                    ReturnCode::SUCCESS
                })
                .unwrap_or_else(|err| err.into()),
            _ => ReturnCode::ENOSUPPORT,
        }
    }

    /// NFC control
    ///
    /// ### `command_num`
    ///
    /// - `0`: Driver check.
    /// - `1`: Transmits a buffer passed via `allow`, up to the length
    ///        passed in `arg1`.
    /// - `2`: Receives into a buffer passed via `allow`, up to the length
    ///        passed in `arg1`.
    /// - `3`: Controls tag emulation, enables it if the value in `arg1`
    ///        is positive, disables it in case of 0.
    /// - `4`: Configures the tag based on the value of `arg1`.
    fn command(&self, command_num: usize, arg1: usize, _: usize, appid: AppId) -> ReturnCode {
        match command_num {
            0 /* check if present */ => ReturnCode::SUCCESS,
            1 => {
                let len = arg1;
                self.application.enter(appid, |app, _| {
                    self.transmit_new(appid, app, len)
                }).unwrap_or_else(|err| err.into())
            },
            2 => {
                let len = arg1;
                self.application.enter(appid, |app, _| {
                    self.receive_new(appid, app, len)
                }).unwrap_or_else(|err| err.into())
            },
            3 /* enable tag emulation */=> {
                match arg1 as u8 {
                    0 /* false */ => self.driver.deactivate(),
                    _ /* true */ => self.driver.activate(),
                }
                ReturnCode::SUCCESS
            }
            4 /* tag type configuration */ => {
                let tag_type = arg1;
                self.driver.configure(tag_type as u8);
                ReturnCode::SUCCESS
            }
            _ => ReturnCode::ENOSUPPORT,
        }
    }
}
