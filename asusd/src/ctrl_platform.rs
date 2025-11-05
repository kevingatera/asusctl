use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use config_traits::StdConfig;
use futures_util::lock::Mutex;
use log::{debug, error, info, warn};
use rog_platform::asus_armoury::{AttrValue, FirmwareAttribute, FirmwareAttributes};
use rog_platform::cpu::{CPUControl, CPUGovernor, CPUEPP};
use rog_platform::platform::{PlatformProfile, Properties, RogPlatform};
use rog_platform::power::AsusPower;
use zbus::fdo::Error as FdoErr;
use zbus::object_server::SignalEmitter;
use zbus::{interface, Connection};

use crate::asus_armoury::{set_config_or_default, ArmouryAttributeRegistry};
use crate::config::Config;
use crate::error::RogError;
use crate::{task_watch_item, CtrlTask, ReloadAndNotify};

const PLATFORM_ZBUS_PATH: &str = "/xyz/ljones";

macro_rules! platform_get_value {
    ($self:ident, $property:tt, $prop_name:literal) => {
        concat_idents::concat_idents!(has = has_, $property {
            if $self.platform.has() {
                concat_idents::concat_idents!(get = get_, $property {
                    $self.platform
                    .get()
                    .map_err(|err| {
                        warn!("{}: {}", $prop_name, err);
                        FdoErr::Failed(format!("RogPlatform: {}: {}", $prop_name, err))
                    })
                })
            } else {
                return Err(FdoErr::NotSupported(format!("RogPlatform: {} not supported", $prop_name)));
            }
        })
    }
}

#[derive(Clone)]
pub struct CtrlPlatform {
    power: AsusPower,
    platform: RogPlatform,
    attributes: FirmwareAttributes,
    cpu_control: Option<CPUControl>,
    config: Arc<Mutex<Config>>,
    connection: Connection,
    armoury_registry: ArmouryAttributeRegistry,
}

impl CtrlPlatform {
    pub fn new(
        platform: RogPlatform,
        power: AsusPower,
        attributes: FirmwareAttributes,
        config: Arc<Mutex<Config>>,
        config_path: &Path,
        signal_context: SignalEmitter<'static>,
        connection: Connection,
        armoury_registry: ArmouryAttributeRegistry,
    ) -> Result<Self, RogError> {
        let config1 = config.clone();
        let config_path = config_path.to_owned();

        let ret_self = CtrlPlatform {
            power,
            platform,
            attributes,
            config,
            cpu_control: CPUControl::new()
                .map_err(|e| error!("Couldn't get CPU control sysfs: {e}"))
                .ok(),
            connection,
            armoury_registry,
        };
        let mut inotify_self = ret_self.clone();

        tokio::spawn(async move {
            use futures_util::StreamExt;
            info!("Starting inotify watch for asusd config file");

            let mut buffer = [0; 32];
            loop {
                // vi and vim do stupid shit causing the file watch to be removed
                let inotify = inotify::Inotify::init().unwrap();
                inotify
                    .watches()
                    .add(
                        &config_path,
                        inotify::WatchMask::MODIFY
                            | inotify::WatchMask::CLOSE_WRITE
                            | inotify::WatchMask::ATTRIB
                            | inotify::WatchMask::CREATE,
                    )
                    .inspect_err(|e| {
                        if e.kind() == std::io::ErrorKind::NotFound {
                            error!("Not found: {:?}", config_path);
                        } else {
                            error!("Could not set asusd config inotify: {:?}", config_path);
                        }
                    })
                    .ok();
                let mut events = inotify.into_event_stream(&mut buffer).unwrap();

                while let Some(ev) = events.next().await {
                    if let Ok(ev) = ev {
                        if ev.mask == inotify::EventMask::IGNORED {
                            warn!(
                                "Something modified asusd.ron vi/vim style. Now need to reload \
                                 inotify watch"
                            );
                            break;
                        }
                    }

                    let res = config1.lock().await.read_new();
                    if let Some(new_cfg) = res {
                        inotify_self
                            .reload_and_notify(&signal_context, new_cfg)
                            .await
                            .unwrap();
                    }
                }
            }
        });

        Ok(ret_self)
    }

    async fn restore_charge_limit(&self) {
        let limit = self.config.lock().await.base_charge_control_end_threshold;
        if limit > 0
            && std::mem::replace(
                &mut self.config.lock().await.charge_control_end_threshold,
                limit,
            ) != limit
        {
            self.power
                .set_charge_control_end_threshold(limit)
                .map_err(|e| {
                    error!("Couldn't restore charge limit: {e}");
                })
                .ok();
            self.config.lock().await.write();
        }
    }

    async fn run_ac_or_bat_cmd(&self, power_plugged: bool) {
        let prog: Vec<String> = if power_plugged {
            // AC ONLINE
            self.config
                .lock()
                .await
                .ac_command
                .split_whitespace()
                .map(|s| s.to_string())
                .collect()
        } else {
            // BATTERY
            self.config
                .lock()
                .await
                .bat_command
                .split_whitespace()
                .map(|s| s.to_string())
                .collect()
        };
        if (!prog.is_empty()) && (!prog[0].is_empty()) {
            let mut cmd = Command::new(&prog[0]);
            for arg in prog.iter().skip(1) {
                cmd.arg(arg);
            }
            if let Err(e) = cmd.spawn() {
                if power_plugged {
                    error!("AC power command error: {e}");
                } else {
                    error!("Battery power command error: {e}");
                }
            }
        }
    }

    fn check_and_set_epp(&self, enegy_pref: CPUEPP, change_epp: bool) {
        if !change_epp {
            info!("ThrottlePolicy unlinked from EPP");
            return;
        }
        info!("ThrottlePolicy setting EPP");
        if let Some(cpu) = self.cpu_control.as_ref() {
            if let Ok(epp) = cpu.get_available_epp() {
                debug!("Available EPP: {epp:?}");
                if epp.contains(&enegy_pref) {
                    debug!("Setting {enegy_pref:?}");
                    cpu.set_epp(enegy_pref).ok();
                } else if let Ok(gov) = cpu.get_governor() {
                    if gov != CPUGovernor::Powersave {
                        warn!("powersave governor is not is use, trying to set.");
                        cpu.set_governor(CPUGovernor::Powersave)
                            .map_err(|e| error!("couldn't set powersave: {e:?}"))
                            .ok();
                        if epp.contains(&enegy_pref) {
                            debug!("Setting {enegy_pref:?}");
                            cpu.set_epp(enegy_pref)
                                .map_err(|e| error!("couldn't set EPP: {e:?}"))
                                .ok();
                        }
                    }
                }
            }
        }
    }

    async fn get_config_epp_for_throttle(&self, throttle: PlatformProfile) -> CPUEPP {
        match throttle {
            PlatformProfile::Balanced => self.config.lock().await.profile_balanced_epp,
            PlatformProfile::Performance => self.config.lock().await.profile_performance_epp,
            PlatformProfile::Quiet => self.config.lock().await.profile_quiet_epp,
            PlatformProfile::LowPower => self.config.lock().await.profile_quiet_epp,
            PlatformProfile::Custom => self.config.lock().await.profile_custom_epp,
        }
    }

    async fn update_policy_ac_or_bat(
        &self,
        previous_power_plugged: bool,
        power_plugged: bool,
        change_epp: bool,
        signal_ctxt: Option<&SignalEmitter<'static>>,
    ) {
        let should_change_profile = if power_plugged {
            self.config.lock().await.change_platform_profile_on_ac
        } else {
            self.config.lock().await.change_platform_profile_on_battery
        };

        if !should_change_profile {
            debug!(
                "Power status changed but change_platform_profile_on_{} set false. Not setting the profile",
                if power_plugged { "ac" } else { "battery" }
            );
            // Still emit signal even if not changing profile, so UI is notified of power change
            if let Some(ctxt) = signal_ctxt {
                if let Ok(_current_profile) = self.platform.get_platform_profile() {
                    debug!("Emitting platform_profile_changed signal (no profile change)");
                    if let Err(e) = self.platform_profile_changed(ctxt).await {
                        warn!("Failed to emit platform_profile_changed signal: {e}");
                    } else {
                        debug!("Successfully emitted platform_profile_changed signal");
                    }
                    if let Err(e) = self.enable_ppt_group_changed(ctxt).await {
                        warn!("Failed to emit enable_ppt_group_changed signal: {e}");
                    } else {
                        debug!("Successfully emitted enable_ppt_group_changed signal");
                    }
                }
            }
            self.config.lock().await.last_power_plugged = u8::from(power_plugged);
            return;
        }

        // Save current profile for the previous power state before switching
        let current_profile_result = self.platform.get_platform_profile();
        {
            let mut config = self.config.lock().await;
            debug!(
                "Saving current profile: previous_power_plugged={}, new_power_plugged={}",
                previous_power_plugged, power_plugged
            );
            if let Ok(current_profile) = current_profile_result {
                let profile: PlatformProfile = current_profile.into();
                debug!("Current profile is {:?}", profile);
                if previous_power_plugged {
                    debug!("Saving {:?} to last_profile_on_ac", profile);
                    config.last_profile_on_ac = Some(profile);
                } else {
                    debug!("Saving {:?} to last_profile_on_battery", profile);
                    config.last_profile_on_battery = Some(profile);
                }
            }
            config.last_power_plugged = u8::from(power_plugged);
            config.write();
        }

        // Use saved profile if available, otherwise fall back to configured default
        let throttle = {
            let config = self.config.lock().await;
            if power_plugged {
                debug!("Power plugged (AC), using last_profile_on_ac or default");
                debug!("  last_profile_on_ac={:?}, platform_profile_on_ac={:?}",
                    config.last_profile_on_ac, config.platform_profile_on_ac);
                config.last_profile_on_ac.unwrap_or(config.platform_profile_on_ac)
            } else {
                debug!("Power unplugged (Battery), using last_profile_on_battery or default");
                debug!("  last_profile_on_battery={:?}, platform_profile_on_battery={:?}",
                    config.last_profile_on_battery, config.platform_profile_on_battery);
                config.last_profile_on_battery.unwrap_or(config.platform_profile_on_battery)
            }
        };
        debug!("Power plugged={}, setting profile {throttle:?} before EPP", power_plugged);
        let epp = self.get_config_epp_for_throttle(throttle).await;
        self.platform.set_platform_profile(throttle.into()).ok();
        self.check_and_set_epp(epp, change_epp);

        // Emit signal to notify clients (like rog-control-center) of the profile change
        if let Some(ctxt) = signal_ctxt {
            // Verify profile was set correctly before emitting signal
            if let Ok(_current_profile) = self.platform.get_platform_profile() {
                debug!("Emitting platform_profile_changed signal after setting {throttle:?}");
                if let Err(e) = self.platform_profile_changed(ctxt).await {
                    warn!("Failed to emit platform_profile_changed signal: {e}");
                } else {
                    debug!("Successfully emitted platform_profile_changed signal");
                }
                if let Err(e) = self.enable_ppt_group_changed(ctxt).await {
                    warn!("Failed to emit enable_ppt_group_changed signal: {e}");
                } else {
                    debug!("Successfully emitted enable_ppt_group_changed signal");
                }
            } else {
                warn!("Failed to verify profile after setting, not emitting signal");
            }
        }
    }
}

#[interface(name = "xyz.ljones.Platform")]
impl CtrlPlatform {
    #[zbus(property)]
    async fn version(&self) -> String {
        crate::VERSION.to_string()
    }

    /// Returns a list of property names that this system supports
    async fn supported_properties(&self) -> Vec<Properties> {
        let mut supported = Vec::new();

        macro_rules! platform_name {
            ($property:tt, $prop_name:ty) => {
                concat_idents::concat_idents!(has = has_, $property {
                    if self.platform.has() {
                        supported.push($prop_name.to_owned());
                    }
                })
            }
        }

        macro_rules! power_name {
            ($property:tt, $prop_name:ty) => {
                concat_idents::concat_idents!(has = has_, $property {
                    if self.power.has() {
                        supported.push($prop_name.to_owned());
                    }
                })
            }
        }

        // TODO: automate this
        power_name!(
            charge_control_end_threshold,
            Properties::ChargeControlEndThreshold
        );

        platform_name!(platform_profile, Properties::ThrottlePolicy);

        supported
    }

    #[zbus(property)]
    fn charge_control_end_threshold(&self) -> Result<u8, FdoErr> {
        let limit = self.power.get_charge_control_end_threshold()?;
        Ok(limit)
    }

    #[zbus(property)]
    async fn set_charge_control_end_threshold(&mut self, limit: u8) -> Result<(), FdoErr> {
        if !(20..=100).contains(&limit) {
            return Err(RogError::ChargeLimit(limit))?;
        }
        self.power.set_charge_control_end_threshold(limit)?;
        self.config.lock().await.charge_control_end_threshold = limit;
        self.config.lock().await.base_charge_control_end_threshold = limit;
        self.config.lock().await.write();
        Ok(())
    }

    async fn one_shot_full_charge(&self) -> Result<(), FdoErr> {
        let base_limit = std::mem::replace(
            &mut self.config.lock().await.charge_control_end_threshold,
            100,
        );
        if base_limit != 100 {
            self.power.set_charge_control_end_threshold(100)?;
            self.config.lock().await.base_charge_control_end_threshold = base_limit;
            self.config.lock().await.write();
        }
        Ok(())
    }

    /// Toggle to next platform_profile. Names provided by `Profiles`.
    /// If fan-curves are supported will also activate a fan curve for profile.
    async fn next_platform_profile(
        &mut self,
        #[zbus(signal_context)] ctxt: SignalEmitter<'_>,
    ) -> Result<(), FdoErr> {
        let policy: PlatformProfile =
            platform_get_value!(self, platform_profile, "platform_profile").map(|n| n.into())?;
        let choices =
            platform_get_value!(self, platform_profile_choices, "platform_profile_choices")?;
        let policy = PlatformProfile::next(policy, &choices);

        if self.platform.has_platform_profile() {
            let change_epp = self.config.lock().await.platform_profile_linked_epp;
            let epp = self.get_config_epp_for_throttle(policy).await;
            self.check_and_set_epp(epp, change_epp);

            // Save profile for current power state
            {
                let mut config = self.config.lock().await;
                if let Ok(power_plugged) = self.power.get_online() {
                    if power_plugged > 0 {
                        config.last_profile_on_ac = Some(policy);
                    } else {
                        config.last_profile_on_battery = Some(policy);
                    }
                }
            }

            self.platform
                .set_platform_profile(policy.into())
                .map_err(|err| {
                    warn!("platform_profile {}", err);
                    FdoErr::Failed(format!("RogPlatform: platform_profile: {err}"))
                })?;
            self.enable_ppt_group_changed(&ctxt).await?;
            Ok(self.platform_profile_changed(&ctxt).await?)
        } else {
            Err(FdoErr::NotSupported(
                "RogPlatform: platform_profile not supported".to_owned(),
            ))
        }
    }

    #[zbus(property)]
    fn platform_profile_choices(&self) -> Result<Vec<PlatformProfile>, FdoErr> {
        platform_get_value!(self, platform_profile_choices, "platform_profile_choices")
    }

    #[zbus(property)]
    fn platform_profile(&self) -> Result<PlatformProfile, FdoErr> {
        let policy: PlatformProfile = self.platform.get_platform_profile()?.as_str().into();
        Ok(policy)
    }

    #[zbus(property)]
    async fn set_platform_profile(
        &mut self,
        #[zbus(signal_context)] ctxt: SignalEmitter<'_>,
        policy: PlatformProfile,
    ) -> Result<(), FdoErr> {
        // TODO: watch for external changes
        if self.platform.has_platform_profile() {
            let change_epp = self.config.lock().await.platform_profile_linked_epp;
            let epp = self.get_config_epp_for_throttle(policy).await;
            self.check_and_set_epp(epp, change_epp);

            // Save profile for current power state
            {
                let mut config = self.config.lock().await;
                if let Ok(power_plugged) = self.power.get_online() {
                    if power_plugged > 0 {
                        config.last_profile_on_ac = Some(policy);
                    } else {
                        config.last_profile_on_battery = Some(policy);
                    }
                }
            }

            self.config.lock().await.write();

            let choices = self.platform.get_platform_profile_choices()?;
            if !choices.contains(&policy) {
                return Err(FdoErr::NotSupported(format!(
                    "RogPlatform: platform_profile: {} not supported",
                    policy
                )));
            }

            self.platform
                .set_platform_profile(policy.into())
                .map_err(|err| {
                    warn!("platform_profile {}", err);
                    FdoErr::Failed(format!("RogPlatform: platform_profile: {err}"))
                })?;
            self.enable_ppt_group_changed(&ctxt).await?;
            Ok(self.platform_profile_changed(&ctxt).await?)
        } else {
            Err(FdoErr::NotSupported(
                "RogPlatform: platform_profile not supported".to_owned(),
            ))
        }
    }

    #[zbus(property)]
    async fn platform_profile_linked_epp(&self) -> Result<bool, FdoErr> {
        Ok(self.config.lock().await.platform_profile_linked_epp)
    }

    #[zbus(property)]
    async fn set_platform_profile_linked_epp(&self, linked: bool) -> Result<(), zbus::Error> {
        self.config.lock().await.platform_profile_linked_epp = linked;
        self.config.lock().await.write();
        Ok(())
    }

    #[zbus(property)]
    async fn platform_profile_on_battery(&self) -> Result<PlatformProfile, FdoErr> {
        Ok(self.config.lock().await.platform_profile_on_battery)
    }

    #[zbus(property)]
    async fn set_platform_profile_on_battery(
        &mut self,
        #[zbus(signal_context)] ctxt: SignalEmitter<'_>,
        policy: PlatformProfile,
    ) -> Result<(), FdoErr> {
        self.config.lock().await.platform_profile_on_battery = policy;
        // Save as the remembered profile for battery power
        self.config.lock().await.last_profile_on_battery = Some(policy);
        self.set_platform_profile(ctxt, policy).await?;
        self.config.lock().await.write();
        Ok(())
    }

    #[zbus(property)]
    async fn change_platform_profile_on_battery(&self) -> Result<bool, FdoErr> {
        Ok(self.config.lock().await.change_platform_profile_on_battery)
    }

    #[zbus(property)]
    async fn set_change_platform_profile_on_battery(&mut self, change: bool) -> Result<(), FdoErr> {
        self.config.lock().await.change_platform_profile_on_battery = change;
        self.config.lock().await.write();
        Ok(())
    }

    #[zbus(property)]
    async fn platform_profile_on_ac(&self) -> Result<PlatformProfile, FdoErr> {
        Ok(self.config.lock().await.platform_profile_on_ac)
    }

    #[zbus(property)]
    async fn set_platform_profile_on_ac(
        &mut self,
        #[zbus(signal_context)] ctxt: SignalEmitter<'_>,
        policy: PlatformProfile,
    ) -> Result<(), FdoErr> {
        self.config.lock().await.platform_profile_on_ac = policy;
        // Save as the remembered profile for AC power
        self.config.lock().await.last_profile_on_ac = Some(policy);
        self.set_platform_profile(ctxt, policy).await?;
        self.config.lock().await.write();
        Ok(())
    }

    #[zbus(property)]
    async fn change_platform_profile_on_ac(&self) -> Result<bool, FdoErr> {
        Ok(self.config.lock().await.change_platform_profile_on_ac)
    }

    #[zbus(property)]
    async fn set_change_platform_profile_on_ac(&mut self, change: bool) -> Result<(), FdoErr> {
        self.config.lock().await.change_platform_profile_on_ac = change;
        self.config.lock().await.write();
        Ok(())
    }

    /// The energy_performance_preference for the quiet throttle/platform
    /// profile
    #[zbus(property)]
    async fn profile_quiet_epp(&self) -> Result<CPUEPP, FdoErr> {
        Ok(self.config.lock().await.profile_quiet_epp)
    }

    #[zbus(property)]
    async fn set_profile_quiet_epp(&mut self, epp: CPUEPP) -> Result<(), FdoErr> {
        let change_pp = self.config.lock().await.platform_profile_linked_epp;
        self.config.lock().await.profile_quiet_epp = epp;
        self.check_and_set_epp(epp, change_pp);
        self.config.lock().await.write();
        Ok(())
    }

    /// The energy_performance_preference for the balanced throttle/platform
    /// profile
    #[zbus(property)]
    async fn profile_balanced_epp(&self) -> Result<CPUEPP, FdoErr> {
        Ok(self.config.lock().await.profile_balanced_epp)
    }

    #[zbus(property)]
    async fn set_profile_balanced_epp(&mut self, epp: CPUEPP) -> Result<(), FdoErr> {
        let change_pp = self.config.lock().await.platform_profile_linked_epp;
        self.config.lock().await.profile_balanced_epp = epp;
        self.check_and_set_epp(epp, change_pp);
        self.config.lock().await.write();
        Ok(())
    }

    /// The energy_performance_preference for the performance throttle/platform
    /// profile
    #[zbus(property)]
    async fn profile_performance_epp(&self) -> Result<CPUEPP, FdoErr> {
        Ok(self.config.lock().await.profile_performance_epp)
    }

    #[zbus(property)]
    async fn set_profile_performance_epp(&mut self, epp: CPUEPP) -> Result<(), FdoErr> {
        let change_pp = self.config.lock().await.platform_profile_linked_epp;
        self.config.lock().await.profile_performance_epp = epp;
        self.check_and_set_epp(epp, change_pp);

        self.config.lock().await.write();
        Ok(())
    }

    /// Set if the PPT tuning group for the current profile is enabled
    #[zbus(property)]
    async fn enable_ppt_group(&self) -> Result<bool, FdoErr> {
        let power_plugged = self
            .power
            .get_online()
            .map_err(|e| {
                error!("Could not get power status: {e:?}");
                e
            })
            .unwrap_or_default();
        let profile: PlatformProfile = self.platform.get_platform_profile()?.into();
        Ok(self
            .config
            .lock()
            .await
            .select_tunings(power_plugged == 1, profile)
            .enabled)
    }

    /// Set if the PPT tuning group for the current profile is enabled
    #[zbus(property)]
    async fn set_enable_ppt_group(&mut self, enable: bool) -> Result<(), FdoErr> {
        let power_plugged = self
            .power
            .get_online()
            .map_err(|e| {
                error!("Could not get power status: {e:?}");
                e
            })
            .unwrap_or_default();
        let profile: PlatformProfile = self.platform.get_platform_profile()?.into();

        if enable {
            // Clone to reduce blocking
            let tuning = self
                .config
                .lock()
                .await
                .select_tunings(power_plugged == 1, profile)
                .clone();

            for attr in self.attributes.attributes() {
                let name: FirmwareAttribute = attr.name().into();
                if name.is_ppt() {
                    // reset stored value
                    if let Some(tune) = self
                        .config
                        .lock()
                        .await
                        .select_tunings(power_plugged == 1, profile)
                        .group
                        .get_mut(&name)
                    {
                        let value = tuning
                            .group
                            .get(&name)
                            .map(|v| AttrValue::Integer(*v))
                            .unwrap_or_else(|| attr.default_value().clone());
                        // restore default
                        attr.set_current_value(&value)?;
                        if let AttrValue::Integer(i) = value {
                            *tune = i
                        }
                    }
                }
            }
        } else {
            // finally, reapply the profile to ensure acpi does the thingy
            self.platform.set_platform_profile(profile.into())?;
        }

        self.config
            .lock()
            .await
            .select_tunings(power_plugged == 1, profile)
            .enabled = enable;
        self.config.lock().await.write();

        Ok(())
    }
}

impl crate::ZbusRun for CtrlPlatform {
    async fn add_to_server(self, server: &mut Connection) {
        Self::add_to_server_helper(self, PLATFORM_ZBUS_PATH, server).await;
    }
}

impl ReloadAndNotify for CtrlPlatform {
    type Data = Config;

    /// Called on config file changed externally
    async fn reload_and_notify(
        &mut self,
        signal_context: &SignalEmitter<'static>,
        data: Self::Data,
    ) -> Result<(), RogError> {
        let mut config = self.config.lock().await;
        if *config != data {
            info!("asusd.ron updated externally, reloading and updating internal copy");

            let mut base_charge_control_end_threshold = None;
            let last_power_plugged = config.last_power_plugged;

            if self.power.has_charge_control_end_threshold()
                && data.charge_control_end_threshold != config.charge_control_end_threshold
            {
                let limit = data.charge_control_end_threshold;
                warn!("setting charge_control_end_threshold to {limit}");
                self.power.set_charge_control_end_threshold(limit)?;
                self.charge_control_end_threshold_changed(signal_context)
                    .await?;
                base_charge_control_end_threshold = (config.base_charge_control_end_threshold > 0)
                    .then_some(config.base_charge_control_end_threshold)
                    .or(Some(limit));
            }

            if self.platform.has_platform_profile()
                && config.platform_profile_linked_epp != data.platform_profile_linked_epp
            {
                let profile: PlatformProfile = self.platform.get_platform_profile()?.into();

                let epp = match profile {
                    PlatformProfile::Balanced => data.profile_balanced_epp,
                    PlatformProfile::Performance => data.profile_performance_epp,
                    PlatformProfile::Quiet => data.profile_quiet_epp,
                    PlatformProfile::LowPower => data.profile_quiet_epp,
                    PlatformProfile::Custom => data.profile_custom_epp,
                };
                warn!("setting epp to {epp:?}");
                self.check_and_set_epp(epp, true);
            }
            // reload_and_notify!(platform_profile, "platform_profile");

            *config = data;
            config.last_power_plugged = last_power_plugged;
            config.base_charge_control_end_threshold =
                base_charge_control_end_threshold.unwrap_or_default();
        }
        Ok(())
    }
}

impl crate::Reloadable for CtrlPlatform {
    async fn reload(&mut self) -> Result<(), RogError> {
        info!("Begin Platform settings restore");
        if self.power.has_charge_control_end_threshold() {
            // self.restore_charge_limit().await;
            let limit = self.config.lock().await.charge_control_end_threshold;
            info!("reloading charge_control_end_threshold to {limit}");
            self.power.set_charge_control_end_threshold(limit)?;
        } else {
            warn!("No charge_control_end_threshold found")
        }

        if let Ok(power_plugged) = self.power.get_online() {
            let previous_power_state_u8 = self.config.lock().await.last_power_plugged;
            let previous_power_state = previous_power_state_u8 > 0;
            let power_state_changed = previous_power_state_u8 != power_plugged;
            
            if self.platform.has_platform_profile() {
                // Initialize saved profiles if they don't exist
                {
                    let mut config = self.config.lock().await;
                    if let Ok(current_profile) = self.platform.get_platform_profile() {
                        let profile: PlatformProfile = current_profile.into();
                        if power_plugged > 0 {
                            if config.last_profile_on_ac.is_none() {
                                config.last_profile_on_ac = Some(profile);
                            }
                        } else {
                            if config.last_profile_on_battery.is_none() {
                                config.last_profile_on_battery = Some(profile);
                            }
                        }
                    }
                }
                
                let change_epp = self.config.lock().await.platform_profile_linked_epp;
                
                if power_state_changed {
                    // Only update if power state actually changed
                    self
                        .update_policy_ac_or_bat(
                            previous_power_state,
                            power_plugged > 0,
                            change_epp,
                            None,
                        )
                        .await;
                } else {
                    // At startup (no change detected), use saved profile if available, otherwise use configured default
                    let throttle = {
                        let config = self.config.lock().await;
                        if power_plugged > 0 {
                            config.last_profile_on_ac.unwrap_or(config.platform_profile_on_ac)
                        } else {
                            config.last_profile_on_battery.unwrap_or(config.platform_profile_on_battery)
                        }
                    };
                    let epp = self.get_config_epp_for_throttle(throttle).await;
                    self.platform.set_platform_profile(throttle.into()).ok();
                    self.check_and_set_epp(epp, change_epp);
                }
            }
            
            self.run_ac_or_bat_cmd(power_plugged > 0).await;
        }

        Ok(())
    }
}

impl CtrlPlatform {
    task_watch_item!(charge_control_end_threshold "charge_control_end_threshold" power);
}

impl CtrlTask for CtrlPlatform {
    fn zbus_path() -> &'static str {
        PLATFORM_ZBUS_PATH
    }

    async fn create_tasks(&self, signal_ctxt: SignalEmitter<'static>) -> Result<(), RogError> {
        let platform1 = self.clone();
        let platform2 = self.clone();
        let platform3 = self.clone();
        let signal_ctxt_copy = signal_ctxt.clone();
        let signal_ctxt_copy2 = signal_ctxt.clone();
        self.create_sys_event_tasks(
            move |sleeping| {
                let platform1 = platform1.clone();
                let signal_ctxt_copy = signal_ctxt_copy2.clone();
                async move {
                    // This block is commented out due to some kind of issue reported. Maybe the
                    // desktops used were storing a value whcih was then read here.
                    // Don't store it on suspend, assume that the current config setting is desired
                    // if sleeping && platform1.power.has_charge_control_end_threshold() {
                    //     platform1.config.lock().await.charge_control_end_threshold = platform1
                    //         .power
                    //         .get_charge_control_end_threshold()
                    //         .unwrap_or(100);
                    // } else
                    if !sleeping && platform1.power.has_charge_control_end_threshold() {
                        platform1
                            .power
                            .set_charge_control_end_threshold(
                                platform1.config.lock().await.charge_control_end_threshold,
                            )
                            .ok();
                    }
                    if let Ok(power_plugged_raw) = platform1.power.get_online() {
                        let new_state = power_plugged_raw > 0;
                        let (previous_state, change_epp) = {
                            let cfg = platform1.config.lock().await;
                            (cfg.last_power_plugged > 0, cfg.platform_profile_linked_epp)
                        };
                        if previous_state != new_state {
                            if !sleeping && platform1.platform.has_platform_profile() {
                                platform1
                                    .update_policy_ac_or_bat(
                                        previous_state,
                                        new_state,
                                        change_epp,
                                        Some(&signal_ctxt_copy),
                                    )
                                    .await;
                            }
                            if !sleeping {
                                platform1.run_ac_or_bat_cmd(new_state).await;
                                if let Ok(profile) =
                                    platform1.platform.get_platform_profile().map(|p| p.into())
                                {
                                    let attrs = FirmwareAttributes::new();
                                    {
                                        let mut cfg = platform1.config.lock().await;
                                        set_config_or_default(
                                            &attrs,
                                            &mut cfg,
                                            new_state,
                                            profile,
                                        )
                                        .await;
                                    }
                                    if let Err(e) = platform1
                                        .armoury_registry
                                        .emit_limits(&platform1.connection)
                                        .await
                                    {
                                        error!(
                                            "Failed to emit armoury updates after power change: \
                                             {e:?}"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            },
            move |shutting_down| {
                let platform2 = platform2.clone();
                async move {
                    info!("RogPlatform reloading panel_od");
                    let lock = platform2.config.lock().await;
                    if shutting_down
                        && platform2.power.has_charge_control_end_threshold()
                        && lock.base_charge_control_end_threshold > 0
                    {
                        info!("RogPlatform restoring charge_control_end_threshold");
                        platform2
                            .power
                            .set_charge_control_end_threshold(
                                lock.base_charge_control_end_threshold,
                            )
                            .map_err(|err| {
                                warn!("CtrlCharge: charge_control_end_threshold {}", err);
                                err
                            })
                            .ok();
                    }
                }
            },
            move |_lid_closed| {
                // on lid change
                async move {}
            },
            move |power_plugged| {
                let platform3 = platform3.clone();
                let signal_ctxt_copy = signal_ctxt.clone();
                // power change
                async move {
                    let (previous_state, change_epp) = {
                        let cfg = platform3.config.lock().await;
                        (cfg.last_power_plugged > 0, cfg.platform_profile_linked_epp)
                    };
                    if previous_state != power_plugged && platform3.platform.has_platform_profile() {
                        platform3
                            .update_policy_ac_or_bat(
                                previous_state,
                                power_plugged,
                                change_epp,
                                Some(&signal_ctxt_copy),
                            )
                            .await;
                    }
                    platform3.run_ac_or_bat_cmd(power_plugged).await;
                    // In case one-shot charge was used, restore the old charge limit
                    if platform3.power.has_charge_control_end_threshold() && !power_plugged {
                        platform3.restore_charge_limit().await;
                    }

                    if let Ok(profile) = platform3
                        .platform
                        .get_platform_profile()
                        .map(|p| p.into())
                        .map_err(|e| {
                            error!("Platform: get_platform_profile error: {e}");
                        })
                    {
                        // TODO: manage this better, shouldn't need to create every time
                        let attrs = FirmwareAttributes::new();
                        {
                            let mut cfg = platform3.config.lock().await;
                            set_config_or_default(&attrs, &mut cfg, power_plugged, profile).await;
                        }
                        if let Err(e) = platform3
                            .armoury_registry
                            .emit_limits(&platform3.connection)
                            .await
                        {
                            error!("Failed to emit armoury updates after AC/DC toggle: {e:?}");
                        }
                        platform3
                            .enable_ppt_group_changed(&signal_ctxt_copy)
                            .await
                            .ok();
                    }
                }
            },
        )
        .await;

        // This spawns a new task for every item.
        // TODO: find a better way to manage this
        self.watch_charge_control_end_threshold(signal_ctxt_copy.clone())
            .await?;

        let watch_platform_profile = self.platform.monitor_platform_profile()?;
        let ctrl = self.clone();

        // Need a copy here, not ideal. But first use in asus_armoury.rs is
        // moved to zbus
        let attrs = FirmwareAttributes::new();
        tokio::spawn(async move {
            use futures_lite::StreamExt;
            let mut buffer = [0; 32];
            if let Ok(mut stream) = watch_platform_profile.into_event_stream(&mut buffer) {
                while (stream.next().await).is_some() {
                    // this blocks
                    debug!("Platform: watch_platform_profile changed");
                    if let Ok(profile) = ctrl
                        .platform
                        .get_platform_profile()
                        .map(|p| p.into())
                        .map_err(|e| {
                            error!("Platform: get_platform_profile error: {e}");
                        })
                    {
                        let change_epp = ctrl.config.lock().await.platform_profile_linked_epp;
                        let epp = ctrl.get_config_epp_for_throttle(profile).await;
                        ctrl.check_and_set_epp(epp, change_epp);
                        ctrl.platform_profile_changed(&signal_ctxt_copy).await.ok();
                        ctrl.enable_ppt_group_changed(&signal_ctxt_copy).await.ok();
                        let power_plugged = ctrl
                            .power
                            .get_online()
                            .map_err(|e| {
                                error!("Could not get power status: {e:?}");
                                e
                            })
                            .unwrap_or_default();
                        set_config_or_default(
                            &attrs,
                            &mut *ctrl.config.lock().await,
                            power_plugged == 1,
                            profile,
                        )
                        .await;
                        if let Err(e) = ctrl.armoury_registry.emit_limits(&ctrl.connection).await {
                            error!("Failed to emit armoury updates after profile change: {e:?}");
                        }
                    }
                }
            }
        });

        Ok(())
    }
}
