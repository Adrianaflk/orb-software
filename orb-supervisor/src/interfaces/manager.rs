//! [`Manager`] defines the `org.worldcoin.OrbSupervisor1.Manager` Dbus interface.
//!
//! It currently only supports the `BackgroundDownloadsAllowed` property used by the update to
//! decide whether or not it can download updates.

use tokio::{
    sync::watch,
    time::{Duration, Instant},
};
use tracing::{debug, info, instrument, warn};
use zbus::{dbus_interface, Connection, DBusError, SignalContext};
use zbus_systemd::{login1, systemd1};

use crate::tasks;

/// The duration of time since the last "start signup" event that has to have passed
/// before the update agent is permitted to start a download.
pub const DEFAULT_DURATION_TO_ALLOW_DOWNLOADS: Duration = Duration::from_secs(3600);

pub const BACKGROUND_DOWNLOADS_ALLOWED_PROPERTY_NAME: &str =
    "BackgroundDownloadsAllowed";
pub const INTERFACE_NAME: &str = "org.worldcoin.OrbSupervisor1.Manager";
pub const OBJECT_PATH: &str = "/org/worldcoin/OrbSupervisor1/Manager";

#[derive(Debug, DBusError)]
#[dbus_error(prefix = "org.worldcoin.OrbSupervisor1.Manager")]
pub enum BusError {
    #[dbus_error(zbus_error)]
    ZBus(zbus::Error),
    UpdatesBlocked(String),
    InvalidArgs(String),
}

impl BusError {
    fn updates_blocked(msg: impl Into<String>) -> Self {
        Self::UpdatesBlocked(msg.into())
    }
}

pub struct Manager {
    duration_to_allow_downloads: Duration,
    last_signup_event: watch::Sender<Instant>,
    system_connection: Option<Connection>,
}

impl Manager {
    /// Constructs a new `Manager` instance.
    #[allow(clippy::must_use_candidate)]
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(Instant::now());
        Self {
            duration_to_allow_downloads: DEFAULT_DURATION_TO_ALLOW_DOWNLOADS,
            last_signup_event: tx,
            system_connection: None,
        }
    }

    #[must_use]
    pub fn duration_to_allow_downloads(
        self,
        duration_to_allow_downloads: Duration,
    ) -> Self {
        Self {
            duration_to_allow_downloads,
            ..self
        }
    }

    #[allow(clippy::must_use_candidate)]
    pub fn are_downloads_allowed(&self) -> bool {
        self.last_signup_event.borrow().elapsed() >= self.duration_to_allow_downloads
    }

    fn reset_last_signup_event(&mut self) {
        self.last_signup_event.send_replace(Instant::now());
    }

    pub fn set_system_connection(&mut self, conn: zbus::Connection) {
        self.system_connection.replace(conn);
    }

    /// Resets the internal timer tracking the last signup event to the current time and emits a
    /// `PropertyChanged` for the `BackgroundDownloadsAllowed` signal.
    ///
    /// # Errors
    ///
    /// The same as calling [`zbus::fdo::Properties::properties_changed`].
    pub async fn reset_last_signup_event_and_notify(
        &mut self,
        signal_context: &SignalContext<'_>,
    ) -> zbus::Result<()> {
        self.reset_last_signup_event();
        self.background_downloads_allowed_changed(signal_context)
            .await
    }
}

impl Default for Manager {
    fn default() -> Self {
        Self::new()
    }
}

#[dbus_interface(name = "org.worldcoin.OrbSupervisor1.Manager")]
impl Manager {
    #[dbus_interface(property, name = "BackgroundDownloadsAllowed")]
    #[instrument(
        fields(
            dbus_interface = "org.worldcoin.OrbSupervisor1.Manager.BackgroundDownloadsAllowed"
        ),
        skip_all
    )]
    async fn background_downloads_allowed(&self) -> bool {
        debug!(
            millis = self.last_signup_event.borrow().elapsed().as_millis(),
            "time since last signup event",
        );
        self.are_downloads_allowed()
    }

    #[dbus_interface(name = "RequestUpdatePermission")]
    #[instrument(
        name = "org.worldcoin.OrbSupervisor1.Manager.RequestUpdatePermission",
        skip_all
    )]
    async fn request_update_permission(&self) -> Result<(), BusError> {
        debug!("RequestUpdatePermission was called");
        let conn = self
            .system_connection
            .as_ref()
            .expect("manager must be conntected to system dbus");
        let systemd_proxy = systemd1::ManagerProxy::new(conn).await?;
        // Spawn task to shut down worldcoin core
        let mut shutdown_core_task = tasks::update::spawn_shutdown_worldcoin_core_timer(
            systemd_proxy.clone(),
            self.last_signup_event.subscribe(),
        );
        // Wait for one second to see if worldcoin core is already shut down
        match tokio::time::timeout(Duration::from_secs(1), &mut shutdown_core_task)
            .await
        {
            Ok(Ok(Ok(()))) => {
                debug!("worldcoin core shut down task returned in less than 1s, permitting update");
                Ok(())
            }
            Ok(Ok(Err(e))) => {
                warn!(
                    error = ?e,
                    "worldcoin core shutdown task returned with error in less than 1s; permitting update because of unclear status",
                );
                Ok(())
            }
            Ok(Err(e)) => {
                warn!(
                    panic_msg = ?e,
                    "worldcoin core shutdown task panicked trying; permitting update because of unclear status",
                );
                Ok(())
            }
            Err(elapsed) => {
                debug!(%elapsed, "shutting down worldcoin core takes longer than 1s; running in background and blocking update by returning a method error");
                let _deteched_shutdown_task =
                    tasks::update::spawn_start_update_agent_after_core_shutdown_task(
                        systemd_proxy,
                        shutdown_core_task,
                    );
                Err(BusError::updates_blocked(
                    "orb core is still running and will be shut down 20 minutes after the last \
                     signup; supervisor will start update agent after",
                ))
            }
        }
    }

    #[dbus_interface(name = "ScheduleShutdown")]
    #[instrument(
        name = "org.worldcoin.OrbSupervisor1.Manager.ScheduleShutdown",
        skip_all
    )]
    async fn schedule_shutdown(&self, kind: &str, when: u64) -> Result<(), BusError> {
        debug!("ScheduleShutdown was called");
        let shutdown =
            tasks::shutdown::ScheduledShutdown::try_from_dbus((kind.to_string(), when))
                .map_err(|err| {
                    BusError::InvalidArgs(format!(
                        "schedule shutdown failed: `{err:?}`"
                    ))
                })?;
        let conn = self
            .system_connection
            .as_ref()
            .expect("manager must be conntected to system dbus");
        let logind_proxy = login1::ManagerProxy::new(conn).await?;
        let schedule_shutdown_task =
            tasks::shutdown::spawn_logind_schedule_shutdown_task(
                logind_proxy,
                shutdown.clone(),
            );
        match schedule_shutdown_task.await {
            Ok(Ok(())) => info!("scheduled shutdown `{shutdown:?}`"),
            Ok(Err(err @ tasks::shutdown::Error::Defer(_))) => warn!(
                error = ?err,
                "skipped shutdown `{shutdown:?}`"
            ),
            Ok(Err(err)) => warn!(
                error = ?err,
                "failed to schedule shutdown `{shutdown:?}` with error `{err:?}`"
            ),
            Err(err) => warn!(
                panic_msg = ?err,
                "logind schedule shutdown task panicked trying;",
            ),
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use zbus::Interface;

    use super::{Manager, DEFAULT_DURATION_TO_ALLOW_DOWNLOADS};

    #[test]
    fn manager_interface_name_matches_exported_const() {
        assert_eq!(super::INTERFACE_NAME, &*Manager::name());
    }

    #[tokio::test]
    async fn manager_background_downloads_allowed_property_matched_exported_const() {
        let manager = Manager::new();
        assert!(manager
            .get(super::BACKGROUND_DOWNLOADS_ALLOWED_PROPERTY_NAME)
            .await
            .is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn downloads_are_disallowed_if_last_signup_event_is_too_recent() {
        let manager = Manager::new()
            .duration_to_allow_downloads(DEFAULT_DURATION_TO_ALLOW_DOWNLOADS);
        tokio::time::advance(DEFAULT_DURATION_TO_ALLOW_DOWNLOADS / 2).await;

        assert!(!manager.are_downloads_allowed());
    }

    #[tokio::test(start_paused = true)]
    async fn downloads_are_allowed_if_last_signup_event_is_old_enough() {
        let manager = Manager::new()
            .duration_to_allow_downloads(DEFAULT_DURATION_TO_ALLOW_DOWNLOADS);
        tokio::time::advance(DEFAULT_DURATION_TO_ALLOW_DOWNLOADS * 2).await;
        assert!(manager.are_downloads_allowed());
    }

    #[tokio::test(start_paused = true)]
    async fn downloads_become_disallowed_after_reset() {
        let mut manager = Manager::new()
            .duration_to_allow_downloads(DEFAULT_DURATION_TO_ALLOW_DOWNLOADS);
        tokio::time::advance(DEFAULT_DURATION_TO_ALLOW_DOWNLOADS * 2).await;
        assert!(manager.are_downloads_allowed());
        manager.reset_last_signup_event();
        assert!(!manager.are_downloads_allowed());
    }
}
