use std::time::Duration;

use crate::ftdi::{FtdiGpio, OutputState};
use color_eyre::{eyre::WrapErr as _, Result};
use tracing::info;

pub const BUTTON_PIN: crate::ftdi::Pin = FtdiGpio::CTS_PIN;
pub const RECOVERY_PIN: crate::ftdi::Pin = FtdiGpio::RTS_PIN;
pub const NVIDIA_VENDOR_ID: u16 = 0x0955;

pub fn is_recovery_mode_detected() -> Result<bool> {
    let num_nvidia_devices = nusb::list_devices()
        .wrap_err("failed to enumerate usb devices")?
        .filter(|d| d.vendor_id() == NVIDIA_VENDOR_ID)
        .count();
    Ok(num_nvidia_devices > 0)
}

// Note: we are calling some blocking code from async here, but its probably fine.
#[tracing::instrument]
pub async fn reboot(recovery: bool) -> Result<()> {
    fn make_ftdi() -> Result<FtdiGpio> {
        FtdiGpio::builder()
            .with_default_device()
            .and_then(|b| b.configure())
            .wrap_err("failed to create ftdi device")
    }
    info!("Turning off");
    let ftdi = tokio::task::spawn_blocking(|| -> Result<_, color_eyre::Report> {
        let mut ftdi = make_ftdi()?;
        ftdi.set_pin(BUTTON_PIN, OutputState::Low)?;
        ftdi.set_pin(RECOVERY_PIN, OutputState::High)?;
        Ok(ftdi)
    })
    .await
    .wrap_err("task panicked")??;
    tokio::time::sleep(Duration::from_secs(10)).await;

    info!("Resetting FTDI");
    ftdi.destroy().wrap_err("failed to destroy ftdi")?;
    tokio::time::sleep(Duration::from_secs(4)).await;

    info!("Turning on");
    let ftdi = tokio::task::spawn_blocking(move || -> Result<_, color_eyre::Report> {
        let mut ftdi = make_ftdi()?;
        let recovery_state = if recovery {
            OutputState::Low
        } else {
            OutputState::High
        };
        ftdi.set_pin(BUTTON_PIN, OutputState::Low)?;
        ftdi.set_pin(RECOVERY_PIN, recovery_state)?;
        Ok(ftdi)
    })
    .await
    .wrap_err("task panicked")??;
    tokio::time::sleep(Duration::from_secs(4)).await;

    ftdi.destroy().wrap_err("failed to destroy ftdi")?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    info!("Done triggering reboot");

    Ok(())
}
